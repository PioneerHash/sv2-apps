use std::{net::SocketAddr, sync::Arc, thread::JoinHandle, time::Duration};

use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::CancellationToken;
use stratum_apps::{
    key_utils::Secp256k1PublicKey,
    stratum_core::{bitcoin::consensus::Encodable, parsers_sv2::JobDeclaration},
    task_manager::TaskManager,
    tp_type::TemplateProviderType,
    utils::types::Sv2Frame,
    SHUTDOWN_BROADCAST_CAPACITY,
};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::ChannelManager,
    config::{ConfigJDCMode, JobDeclaratorClientConfig},
    error::JDCErrorKind,
    jd_mode::{set_jd_mode, JdMode},
    job_declarator::JobDeclarator,
    status::{State, Status},
    template_receiver::{
        bitcoin_core::{connect_to_bitcoin_core, BitcoinCoreSv2Config},
        sv2_tp::Sv2Tp,
    },
    upstream::Upstream,
    utils::{ShutdownMessage, UpstreamState},
};

mod channel_manager;
pub mod config;
mod downstream;
pub mod ehash_mint;
pub mod error;
mod io_task;
pub mod jd_mode;
mod job_declarator;
pub mod monitoring;
mod status;
mod template_receiver;
mod upstream;
pub mod utils;

/// Represent Job Declarator Client
#[derive(Clone)]
pub struct JobDeclaratorClient {
    config: JobDeclaratorClientConfig,
    notify_shutdown: broadcast::Sender<ShutdownMessage>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl JobDeclaratorClient {
    /// Creates a new [`JobDeclaratorClient`] instance.
    pub fn new(config: JobDeclaratorClientConfig) -> Self {
        let (notify_shutdown, _) =
            tokio::sync::broadcast::channel::<ShutdownMessage>(SHUTDOWN_BROADCAST_CAPACITY);
        Self {
            config,
            notify_shutdown,
        }
    }

    /// Starts the Job Declarator Client (JDC) main loop.
    pub async fn start(&self) {
        info!(
            "Job declarator client starting... setting up subsystems, User Identity: {}",
            self.config.user_identity()
        );

        let miner_coinbase_outputs = vec![self.config.get_txout()];
        let mut encoded_outputs = vec![];

        miner_coinbase_outputs
            .consensus_encode(&mut encoded_outputs)
            .expect("Invalid coinbase output in config");

        let notify_shutdown = self.notify_shutdown.clone();
        let (shutdown_complete_tx, mut shutdown_complete_rx) = mpsc::channel::<()>(1);
        let task_manager = Arc::new(TaskManager::new());

        let (status_sender, status_receiver) = async_channel::unbounded::<Status>();

        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();

        let (channel_manager_to_jd_sender, channel_manager_to_jd_receiver) = unbounded();
        let (jd_to_channel_manager_sender, jd_to_channel_manager_receiver) = unbounded();

        let (channel_manager_to_downstream_sender, _channel_manager_to_downstream_receiver) =
            broadcast::channel(10);
        let (downstream_to_channel_manager_sender, downstream_to_channel_manager_receiver) =
            unbounded();

        let (channel_manager_to_tp_sender, channel_manager_to_tp_receiver) = unbounded();
        let (tp_to_channel_manager_sender, tp_to_channel_manager_receiver) = unbounded();

        debug!("Channels initialized.");

        // Initialize ehash-mint client if configured (before ChannelManager so we can pass the sender)
        let ehash_report_sender = if let Some((ehash_client, ehash_receiver)) =
            ehash_mint::EhashMintClient::new(self.config.ehash_mint())
        {
            info!("ehash-mint integration enabled");

            let sender = ehash_client.report_sender();

            // Start the mint communication task
            let mut shutdown_rx_ehash = notify_shutdown.subscribe();
            let shutdown_signal = async move {
                loop {
                    match shutdown_rx_ehash.recv().await {
                        Ok(ShutdownMessage::ShutdownAll) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            };
            // Convert to broadcast receiver for the mint task
            let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
            task_manager.spawn(async move {
                shutdown_signal.await;
                let _ = shutdown_tx.send(());
            });

            ehash_mint::start_mint_task(
                self.config.ehash_mint().clone(),
                ehash_receiver,
                shutdown_rx,
            );

            sender
        } else {
            ehash_mint::ShareReportSender::Inactive
        };

        let channel_manager = ChannelManager::new(
            self.config.clone(),
            channel_manager_to_upstream_sender.clone(),
            upstream_to_channel_manager_receiver.clone(),
            channel_manager_to_jd_sender.clone(),
            jd_to_channel_manager_receiver.clone(),
            channel_manager_to_tp_sender.clone(),
            tp_to_channel_manager_receiver.clone(),
            channel_manager_to_downstream_sender.clone(),
            downstream_to_channel_manager_receiver,
            encoded_outputs.clone(),
            self.config.supported_extensions().to_vec(),
            self.config.required_extensions().to_vec(),
            ehash_report_sender,
        )
        .await
        .unwrap();

        // Start monitoring server if configured
        if let Some(monitoring_addr) = self.config.monitoring_address() {
            info!(
                "Initializing monitoring server on http://{}",
                monitoring_addr
            );

            let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
                monitoring_addr,
                Some(Arc::new(channel_manager.clone())), // SV2 channels opened with servers
                Some(Arc::new(channel_manager.clone())), // SV2 channels opened with clients
            )
            .expect("Failed to initialize monitoring server");

            // Create shutdown signal that waits for ShutdownAll
            let mut notify_shutdown_monitoring = notify_shutdown.subscribe();
            let shutdown_signal = async move {
                loop {
                    match notify_shutdown_monitoring.recv().await {
                        Ok(ShutdownMessage::ShutdownAll) => break,
                        Ok(_) => continue, // Ignore other shutdown messages
                        Err(_) => break,
                    }
                }
            };

            task_manager.spawn(async move {
                if let Err(e) = monitoring_server.run(shutdown_signal).await {
                    error!("Monitoring server error: {:?}", e);
                }
            });
        }

        let channel_manager_clone = channel_manager.clone();
        let mut bitcoin_core_sv2_join_handle: Option<JoinHandle<()>> = None;

        match self.config.template_provider_type().clone() {
            TemplateProviderType::Sv2Tp {
                address,
                public_key,
            } => {
                let template_receiver = Sv2Tp::new(
                    address.clone(),
                    public_key,
                    channel_manager_to_tp_receiver,
                    tp_to_channel_manager_sender,
                    notify_shutdown.clone(),
                    task_manager.clone(),
                    status_sender.clone(),
                )
                .await
                .unwrap();

                let notify_shutdown_cl = notify_shutdown.clone();
                let status_sender_cl = status_sender.clone();
                let task_manager_cl = task_manager.clone();

                template_receiver
                    .start(
                        address,
                        notify_shutdown_cl,
                        status_sender_cl,
                        task_manager_cl,
                    )
                    .await;

                info!("Sv2 Template Provider setup done");
            }
            TemplateProviderType::BitcoinCoreIpc {
                network,
                data_dir,
                fee_threshold,
                min_interval,
            } => {
                let unix_socket_path = stratum_apps::tp_type::resolve_ipc_socket_path(
                    &network, data_dir,
                )
                .expect(
                    "Could not determine Bitcoin data directory. Please set data_dir in config.",
                );

                info!(
                    "Using Bitcoin Core IPC socket at: {}",
                    unix_socket_path.display()
                );

                // incoming and outgoing TDP channels from the perspective of BitcoinCoreSv2
                let incoming_tdp_receiver = channel_manager_to_tp_receiver.clone();
                let outgoing_tdp_sender = tp_to_channel_manager_sender.clone();

                let bitcoin_core_config = BitcoinCoreSv2Config {
                    unix_socket_path,
                    fee_threshold,
                    min_interval,
                    incoming_tdp_receiver,
                    outgoing_tdp_sender,
                    cancellation_token: CancellationToken::new(),
                };

                bitcoin_core_sv2_join_handle = Some(
                    connect_to_bitcoin_core(
                        bitcoin_core_config,
                        notify_shutdown.clone(),
                        task_manager.clone(),
                        status_sender.clone(),
                    )
                    .await,
                );
            }
        }

        let mut upstream_addresses: Vec<_> = self
            .config
            .upstreams()
            .iter()
            .map(|u| {
                let pool_addr = SocketAddr::new(
                    u.pool_address.parse().expect("Invalid pool address"),
                    u.pool_port,
                );
                let jd_addr = SocketAddr::new(
                    u.jds_address.parse().expect("Invalid JD address"),
                    u.jds_port,
                );
                (pool_addr, jd_addr, u.authority_pubkey, false)
            })
            .collect();

        channel_manager
            .start(
                notify_shutdown.clone(),
                status_sender.clone(),
                task_manager.clone(),
                miner_coinbase_outputs.clone(),
            )
            .await;

        info!("Attempting to initialize upstream...");

        match self
            .initialize_jd(
                &mut upstream_addresses,
                channel_manager_to_upstream_receiver.clone(),
                upstream_to_channel_manager_sender.clone(),
                channel_manager_to_jd_receiver.clone(),
                jd_to_channel_manager_sender.clone(),
                notify_shutdown.clone(),
                status_sender.clone(),
                self.config.mode.clone(),
                task_manager.clone(),
            )
            .await
        {
            Ok((upstream, job_declarator)) => {
                upstream
                    .start(
                        self.config.min_supported_version(),
                        self.config.max_supported_version(),
                        notify_shutdown.clone(),
                        shutdown_complete_tx.clone(),
                        status_sender.clone(),
                        task_manager.clone(),
                    )
                    .await;

                job_declarator
                    .start(
                        notify_shutdown.clone(),
                        shutdown_complete_tx,
                        status_sender.clone(),
                        task_manager.clone(),
                    )
                    .await;

                channel_manager_clone
                    .upstream_state
                    .set(UpstreamState::NoChannel);
                _ = channel_manager_clone.allocate_tokens(1).await;
            }
            Err(e) => {
                tracing::error!("Failed to initialize upstream: {:?}", e);
                set_jd_mode(jd_mode::JdMode::SoloMining);
            }
        };

        _ = channel_manager_clone
            .clone()
            .start_downstream_server(
                *self.config.authority_public_key(),
                *self.config.authority_secret_key(),
                self.config.cert_validity_sec(),
                *self.config.listening_address(),
                task_manager.clone(),
                notify_shutdown.clone(),
                status_sender.clone(),
                downstream_to_channel_manager_sender.clone(),
                channel_manager_to_downstream_sender.clone(),
                self.config.supported_extensions().to_vec(),
                self.config.required_extensions().to_vec(),
            )
            .await;

        info!("Spawning status listener task...");
        let notify_shutdown_clone = notify_shutdown.clone();

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received — initiating graceful shutdown...");
                    let _ = notify_shutdown_clone.send(ShutdownMessage::ShutdownAll);
                    break;
                }
                message = status_receiver.recv() => {
                    if let Ok(status) = message {
                        match status.state {
                            State::DownstreamShutdown{downstream_id,..} => {
                                warn!("Downstream {downstream_id:?} disconnected — Channel manager.");
                                let _ = notify_shutdown_clone.send(ShutdownMessage::DownstreamShutdown(downstream_id));
                            }
                            State::TemplateReceiverShutdown(_) => {
                                warn!("Template Receiver shutdown requested — initiating full shutdown.");
                                let _ = notify_shutdown_clone.send(ShutdownMessage::ShutdownAll);
                                break;
                            }
                            State::ChannelManagerShutdown(_) => {
                                warn!("Channel Manager shutdown requested — initiating full shutdown.");
                                let _ = notify_shutdown_clone.send(ShutdownMessage::ShutdownAll);
                                break;
                            }
                            State::UpstreamShutdownFallback(_) | State::JobDeclaratorShutdownFallback(_) => {
                                warn!("Upstream/Job Declarator connection dropped — attempting reconnection...");
                                let (tx, mut rx) = mpsc::channel::<()>(1);
                                let _ = notify_shutdown_clone.send(ShutdownMessage::UpstreamShutdownFallback((encoded_outputs.clone(), tx)));
                                set_jd_mode(JdMode::SoloMining);
                                shutdown_complete_rx.recv().await;
                                tracing::error!("Existing Upstream or JD instance taken out");
                                rx.recv().await;
                                tracing::error!("All entities acknowledged Upstream fallback. Preparing fallback.");

                                let (shutdown_complete_tx_fallback, shutdown_complete_rx_fallback) = mpsc::channel::<()>(1);

                                shutdown_complete_rx = shutdown_complete_rx_fallback;

                                info!("Attempting to initialize Jd and upstream...");

                                match self
                                    .initialize_jd(
                                        &mut upstream_addresses,
                                        channel_manager_to_upstream_receiver.clone(),
                                        upstream_to_channel_manager_sender.clone(),
                                        channel_manager_to_jd_receiver.clone(),
                                        jd_to_channel_manager_sender.clone(),
                                        notify_shutdown.clone(),
                                        status_sender.clone(),
                                        self.config.mode.clone(),
                                        task_manager.clone(),
                                    )
                                    .await
                                {
                                    Ok((upstream, job_declarator)) => {
                                        upstream
                                            .start(
                                                self.config.min_supported_version(),
                                                self.config.max_supported_version(),
                                                notify_shutdown.clone(),
                                                shutdown_complete_tx_fallback.clone(),
                                                status_sender.clone(),
                                                task_manager.clone(),
                                            )
                                            .await;

                                        job_declarator
                                            .start(
                                                notify_shutdown.clone(),
                                                shutdown_complete_tx_fallback,
                                                status_sender.clone(),
                                                task_manager.clone(),
                                            )
                                            .await;

                                        channel_manager_clone.upstream_state.set(UpstreamState::NoChannel);

                                        _ = channel_manager_clone.allocate_tokens(1).await;
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to initialize upstream: {:?}", e);
                                        channel_manager_clone.upstream_state.set(UpstreamState::SoloMining);
                                        set_jd_mode(jd_mode::JdMode::SoloMining);
                                        info!("Fallback to solo mining mode");
                                    }
                                };

                                _ = channel_manager_clone.clone()
                                    .start_downstream_server(
                                        *self.config.authority_public_key(),
                                        *self.config.authority_secret_key(),
                                        self.config.cert_validity_sec(),
                                        *self.config.listening_address(),
                                        task_manager.clone(),
                                        notify_shutdown.clone(),
                                        status_sender.clone(),
                                        downstream_to_channel_manager_sender.clone(),
                                        channel_manager_to_downstream_sender.clone(),
                                        self.config.supported_extensions().to_vec(),
                                        self.config.required_extensions().to_vec(),
                                    )
                                    .await;
                                }
                        }
                    }
                }
            }
        }

        if let Some(bitcoin_core_sv2_join_handle) = bitcoin_core_sv2_join_handle {
            info!("Waiting for BitcoinCoreSv2 dedicated thread to shutdown...");
            match bitcoin_core_sv2_join_handle.join() {
                Ok(_) => info!("BitcoinCoreSv2 dedicated thread shutdown complete."),
                Err(e) => error!("BitcoinCoreSv2 dedicated thread error: {e:?}"),
            }
        }

        warn!("Graceful shutdown");
        task_manager.abort_all().await;

        info!("Joining remaining tasks...");
        task_manager.join_all().await;
        info!("JD Client shutdown complete.");
    }

    /// Initializes an upstream pool + JD connection pair.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize_jd(
        &self,
        upstreams: &mut [(SocketAddr, SocketAddr, Secp256k1PublicKey, bool)],
        channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
        upstream_to_channel_manager_sender: Sender<Sv2Frame>,
        channel_manager_to_jd_receiver: Receiver<JobDeclaration<'static>>,
        jd_to_channel_manager_sender: Sender<JobDeclaration<'static>>,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        status_sender: Sender<Status>,
        mode: ConfigJDCMode,
        task_manager: Arc<TaskManager>,
    ) -> Result<(Upstream, JobDeclarator), JDCErrorKind> {
        const MAX_RETRIES: usize = 3;
        let upstream_len = upstreams.len();
        for (i, upstream_addr) in upstreams.iter_mut().enumerate() {
            info!(
                "Trying upstream {} of {}: {:?}",
                i + 1,
                upstream_len,
                upstream_addr
            );

            tokio::time::sleep(Duration::from_secs(1)).await;

            if upstream_addr.3 {
                info!(
                    "Upstream previously marked as malicious, skipping initial attempt warnings."
                );
                continue;
            }

            for attempt in 1..=MAX_RETRIES {
                info!("Connection attempt {}/{}...", attempt, MAX_RETRIES);

                match try_initialize_single(
                    upstream_addr,
                    upstream_to_channel_manager_sender.clone(),
                    channel_manager_to_upstream_receiver.clone(),
                    jd_to_channel_manager_sender.clone(),
                    channel_manager_to_jd_receiver.clone(),
                    notify_shutdown.clone(),
                    status_sender.clone(),
                    mode.clone(),
                    task_manager.clone(),
                    &self.config,
                )
                .await
                {
                    Ok(pair) => {
                        upstream_addr.3 = true;
                        return Ok(pair);
                    }
                    Err(e) => {
                        let (tx, mut rx) = mpsc::channel::<()>(1);
                        let _ = notify_shutdown.send(ShutdownMessage::JobDeclaratorShutdown(tx));
                        rx.recv().await;
                        tracing::error!("All sparsed upstream and JDS connection is be terminated");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        warn!(
                            "Attempt {}/{} failed for {:?}: {:?}",
                            attempt, MAX_RETRIES, upstream_addr, e
                        );
                        if attempt == MAX_RETRIES {
                            warn!(
                                "Max retries reached for {:?}, moving to next upstream",
                                upstream_addr
                            );
                        }
                    }
                }
            }
            upstream_addr.3 = true;
        }

        tracing::error!("All upstreams failed after {} retries each", MAX_RETRIES);
        Err(JDCErrorKind::CouldNotInitiateSystem)
    }
}

// Attempts to initialize a single upstream (pool + JDS pair).
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
async fn try_initialize_single(
    upstream_addr: &(SocketAddr, SocketAddr, Secp256k1PublicKey, bool),
    upstream_to_channel_manager_sender: Sender<Sv2Frame>,
    channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
    jd_to_channel_manager_sender: Sender<JobDeclaration<'static>>,
    channel_manager_to_jd_receiver: Receiver<JobDeclaration<'static>>,
    notify_shutdown: broadcast::Sender<ShutdownMessage>,
    status_sender: Sender<Status>,
    mode: ConfigJDCMode,
    task_manager: Arc<TaskManager>,
    config: &JobDeclaratorClientConfig,
) -> Result<(Upstream, JobDeclarator), JDCErrorKind> {
    info!("Upstream connection in-progress at initialize single");
    let upstream = Upstream::new(
        upstream_addr,
        upstream_to_channel_manager_sender,
        channel_manager_to_upstream_receiver,
        notify_shutdown.clone(),
        task_manager.clone(),
        status_sender.clone(),
        config.required_extensions().to_vec(),
    )
    .await
    .map_err(|error| error.kind)?;

    info!("Upstream connection done at initialize single");

    let job_declarator = JobDeclarator::new(
        upstream_addr,
        jd_to_channel_manager_sender,
        channel_manager_to_jd_receiver,
        notify_shutdown,
        mode,
        task_manager.clone(),
        status_sender.clone(),
    )
    .await
    .map_err(|error| error.kind)?;

    Ok((upstream, job_declarator))
}

impl Drop for JobDeclaratorClient {
    fn drop(&mut self) {
        info!("JobDeclaratorClient dropped");
        let _ = self.notify_shutdown.send(ShutdownMessage::ShutdownAll);
    }
}
