//! ## Translator Sv2
//!
//! Provides the core logic and main struct (`TranslatorSv2`) for running a
//! Stratum V1 to Stratum V2 translation proxy.
//!
//! This module orchestrates the interaction between downstream SV1 miners and upstream SV2
//! applications (proxies or pool servers).
//!
//! The central component is the `TranslatorSv2` struct, which encapsulates the state and
//! provides the `start` method as the main entry point for running the translator service.
//! It relies on several sub-modules (`config`, `downstream_sv1`, `upstream_sv2`, `proxy`, `status`,
//! etc.) for specialized functionalities.
#![allow(clippy::module_inception)]
use async_channel::{unbounded, Receiver, Sender};
use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};
use stratum_apps::{
    task_manager::TaskManager, utils::types::Sv2Frame, SHUTDOWN_BROADCAST_CAPACITY,
};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

pub use stratum_apps::stratum_core::sv1_api::server_to_client;

use config::TranslatorConfig;

use crate::{
    error::TproxyErrorKind,
    status::{State, Status},
    sv1::sv1_server::sv1_server::Sv1Server,
    sv2::{ChannelManager, Upstream},
    utils::{ShutdownMessage, UpstreamEntry},
};

pub mod config;
pub mod error;
mod io_task;
mod monitoring;
pub mod status;
pub mod sv1;
mod sv1_monitoring;
pub mod sv2;
pub mod utils;

/// The main struct that manages the SV1/SV2 translator.
#[derive(Clone, Debug)]
pub struct TranslatorSv2 {
    config: TranslatorConfig,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl TranslatorSv2 {
    /// Creates a new `TranslatorSv2`.
    ///
    /// Initializes the translator with the given configuration and sets up
    /// the reconnect wait time.
    pub fn new(config: TranslatorConfig) -> Self {
        Self { config }
    }

    /// Starts the translator.
    ///
    /// This method starts the main event loop, which handles connections,
    /// protocol translation, job management, and status reporting.
    pub async fn start(self) {
        info!("Starting Translator Proxy...");
        // only initialized once
        TPROXY_MODE
            .set(self.config.aggregate_channels.into())
            .expect("TPROXY_MODE initialized more than once");

        let (notify_shutdown, _) =
            broadcast::channel::<ShutdownMessage>(SHUTDOWN_BROADCAST_CAPACITY);
        let (shutdown_complete_tx, mut shutdown_complete_rx) = mpsc::channel::<()>(1);
        let task_manager = Arc::new(TaskManager::new());
        let (status_sender, status_receiver) = async_channel::unbounded::<Status>();

        let (channel_manager_to_upstream_sender, channel_manager_to_upstream_receiver) =
            unbounded();
        let (upstream_to_channel_manager_sender, upstream_to_channel_manager_receiver) =
            unbounded();
        let (channel_manager_to_sv1_server_sender, channel_manager_to_sv1_server_receiver) =
            unbounded();
        let (sv1_server_to_channel_manager_sender, sv1_server_to_channel_manager_receiver) =
            unbounded();

        debug!("All inter-subsystem channels initialized");

        let mut upstream_addresses = self
            .config
            .upstreams
            .iter()
            .map(|u| UpstreamEntry {
                addr: SocketAddr::new(u.address.parse().unwrap(), u.port),
                authority_pubkey: u.authority_pubkey,
                tried_or_flagged: false,
            })
            .collect::<Vec<_>>();

        let downstream_addr: SocketAddr = SocketAddr::new(
            self.config.downstream_address.parse().unwrap(),
            self.config.downstream_port,
        );

        let sv1_server = Sv1Server::new(
            downstream_addr,
            channel_manager_to_sv1_server_receiver,
            sv1_server_to_channel_manager_sender,
            channel_manager_to_upstream_sender.clone(),
            self.config.clone(),
        );

        info!("Initializing upstream connection...");

        if let Err(e) = self
            .initialize_upstream(
                &mut upstream_addresses,
                channel_manager_to_upstream_receiver.clone(),
                upstream_to_channel_manager_sender.clone(),
                notify_shutdown.clone(),
                status_sender.clone(),
                shutdown_complete_tx.clone(),
                task_manager.clone(),
                sv1_server.clone(),
                self.config.required_extensions.clone(),
            )
            .await
        {
            error!("Failed to initialize any upstream connection: {e:?}");
            return;
        }

        let channel_manager: Arc<ChannelManager> = Arc::new(ChannelManager::new(
            channel_manager_to_upstream_sender,
            upstream_to_channel_manager_receiver,
            channel_manager_to_sv1_server_sender.clone(),
            sv1_server_to_channel_manager_receiver,
            status_sender.clone(),
            self.config.supported_extensions.clone(),
            self.config.required_extensions.clone(),
        ));

        info!("Launching ChannelManager tasks...");
        ChannelManager::run_channel_manager_tasks(
            channel_manager.clone(),
            notify_shutdown.clone(),
            shutdown_complete_tx.clone(),
            status_sender.clone(),
            task_manager.clone(),
        )
        .await;

        // Start monitoring server if configured
        if let Some(monitoring_addr) = self.config.monitoring_address() {
            info!(
                "Initializing monitoring server on http://{}",
                monitoring_addr
            );

            let monitoring_server = stratum_apps::monitoring::MonitoringServer::new(
                monitoring_addr,
                Some(channel_manager.clone()), // SV2 channels opened with servers
                None,                          /* no SV2 channels opened with clients (SV1
                                                * handled separately) */
            )
            .expect("Failed to initialize monitoring server")
            .with_sv1_monitoring(Arc::new(sv1_server.clone())) // SV1 client connections
            .expect("Failed to add SV1 monitoring");

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

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received — initiating graceful shutdown...");
                    let _ = notify_shutdown.send(ShutdownMessage::ShutdownAll);
                    break;
                }
                message = status_receiver.recv() => {
                    if let Ok(status) = message {
                        match status.state {
                            State::DownstreamShutdown{downstream_id,..} => {
                                warn!("Downstream {downstream_id:?} disconnected — notifying SV1 server.");
                                let _ = notify_shutdown.send(ShutdownMessage::DownstreamShutdown(downstream_id));
                            }
                            State::Sv1ServerShutdown(_) => {
                                warn!("SV1 Server shutdown requested — initiating full shutdown.");
                                let _ = notify_shutdown.send(ShutdownMessage::ShutdownAll);
                                break;
                            }
                            State::ChannelManagerShutdown(_) => {
                                warn!("Channel Manager shutdown requested — initiating full shutdown.");
                                let _ = notify_shutdown.send(ShutdownMessage::ShutdownAll);
                                break;
                            }
                            State::UpstreamShutdown(msg) => {
                                warn!("Upstream connection dropped: {msg:?} — attempting reconnection...");
                                let (tx, mut rx) = mpsc::channel(1);
                                let _ = notify_shutdown.send(ShutdownMessage::UpstreamFallback{tx});
                                // via this we wait for all subsystem to acknowledge the fallback
                                rx.recv().await;
                                info!("Fallback signal acknowledged");

                                if let Err(e) = self.initialize_upstream(
                                    &mut upstream_addresses,
                                    channel_manager_to_upstream_receiver.clone(),
                                    upstream_to_channel_manager_sender.clone(),
                                    notify_shutdown.clone(),
                                    status_sender.clone(),
                                    shutdown_complete_tx.clone(),
                                    task_manager.clone(),
                                    sv1_server.clone(),
                                    self.config.required_extensions.clone(),
                                ).await {
                                    error!("Couldn't perform fallback, shutting system down: {e:?}");
                                    let _ = notify_shutdown.send(ShutdownMessage::ShutdownAll);
                                    break;
                                } else {
                                    info!("Upstream restarted successfully.");
                                }
                            }
                        }
                    }
                }
            }
        }

        drop(shutdown_complete_tx);
        info!("Waiting for shutdown completion signals from subsystems...");
        let shutdown_timeout = tokio::time::Duration::from_secs(5);
        tokio::select! {
            _ = shutdown_complete_rx.recv() => {
                info!("All subsystems reported shutdown complete.");
            }
            _ = tokio::time::sleep(shutdown_timeout) => {
                warn!("Graceful shutdown timed out after {shutdown_timeout:?} — forcing shutdown.");
                task_manager.abort_all().await;
            }
        }
        info!("Joining remaining tasks...");
        task_manager.join_all().await;
        info!("TranslatorSv2 shutdown complete.");
    }

    /// Initializes the upstream connection list, handling retries, fallbacks, and flagging.
    ///
    /// Upstreams are tried sequentially, each receiving a fixed number of retries before we
    /// advance to the next entry. This ensures we exhaust every healthy upstream before shutting
    /// the translator down.
    ///
    /// The `tried_or_flagged` flag in the `UpstreamEntry` acts as the upstream's state machine:
    ///  `false` means "never tried", while `true` means "already connected or marked as
    /// malicious". Once an upstream is flagged we skip it on future loops
    /// to avoid hammering known-bad endpoints during failover.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize_upstream(
        &self,
        upstreams: &mut [UpstreamEntry],
        channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
        upstream_to_channel_manager_sender: Sender<Sv2Frame>,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        status_sender: Sender<Status>,
        shutdown_complete_tx: mpsc::Sender<()>,
        task_manager: Arc<TaskManager>,
        sv1_server_instance: Sv1Server,
        required_extensions: Vec<u16>,
    ) -> Result<(), TproxyErrorKind> {
        const MAX_RETRIES: usize = 3;
        let upstream_len = upstreams.len();
        for (i, upstream_entry) in upstreams.iter_mut().enumerate() {
            // Skip upstreams already marked as malicious. We’ve previously failed or
            // blacklisted them, so no need to warn or attempt reconnecting again.
            if upstream_entry.tried_or_flagged {
                debug!(
                    "Upstream previously marked as malicious, skipping initial attempt warnings."
                );
                continue;
            }

            info!(
                "Trying upstream {} of {}: {:?}",
                i + 1,
                upstream_len,
                upstream_entry.addr
            );
            for attempt in 1..=MAX_RETRIES {
                info!("Connection attempt {}/{}...", attempt, MAX_RETRIES);
                tokio::time::sleep(Duration::from_secs(1)).await;

                match try_initialize_upstream(
                    upstream_entry,
                    upstream_to_channel_manager_sender.clone(),
                    channel_manager_to_upstream_receiver.clone(),
                    notify_shutdown.clone(),
                    status_sender.clone(),
                    shutdown_complete_tx.clone(),
                    task_manager.clone(),
                    required_extensions.clone(),
                )
                .await
                {
                    Ok(pair) => {
                        // starting sv1 server instance
                        if let Err(e) = sv1_server_instance
                            .start(
                                notify_shutdown.clone(),
                                shutdown_complete_tx.clone(),
                                status_sender.clone(),
                                task_manager.clone(),
                            )
                            .await
                        {
                            error!("SV1 server startup failed: {e:?}");
                            return Err(e.kind);
                        }

                        upstream_entry.tried_or_flagged = true;
                        return Ok(pair);
                    }
                    Err(e) => {
                        warn!(
                            "Attempt {}/{} failed for {:?}: {:?}",
                            attempt, MAX_RETRIES, upstream_entry.addr, e
                        );
                        if attempt == MAX_RETRIES {
                            warn!(
                                "Max retries reached for {:?}, moving to next upstream",
                                upstream_entry.addr
                            );
                        }
                    }
                }
            }
            upstream_entry.tried_or_flagged = true;
        }

        tracing::error!("All upstreams failed after {} retries each", MAX_RETRIES);
        Err(TproxyErrorKind::CouldNotInitiateSystem)
    }
}

// Attempts to initialize a single upstream.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
async fn try_initialize_upstream(
    upstream_addr: &UpstreamEntry,
    upstream_to_channel_manager_sender: Sender<Sv2Frame>,
    channel_manager_to_upstream_receiver: Receiver<Sv2Frame>,
    notify_shutdown: broadcast::Sender<ShutdownMessage>,
    status_sender: Sender<Status>,
    shutdown_complete_tx: mpsc::Sender<()>,
    task_manager: Arc<TaskManager>,
    required_extensions: Vec<u16>,
) -> Result<(), TproxyErrorKind> {
    let upstream = Upstream::new(
        upstream_addr,
        upstream_to_channel_manager_sender,
        channel_manager_to_upstream_receiver,
        notify_shutdown.clone(),
        shutdown_complete_tx.clone(),
        task_manager.clone(),
        required_extensions,
    )
    .await?;

    upstream
        .start(
            notify_shutdown,
            shutdown_complete_tx,
            status_sender,
            task_manager,
        )
        .await?;
    Ok(())
}

/// Defines the operational mode for Translator Proxy.
///
/// It can operate in two different modes that affect how Sv1
/// downstream connections are mapped to the upstream Sv2 channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TproxyMode {
    /// All Sv1 downstream connections share a single extended Sv2 channel.
    /// This mode uses extranonce_prefix allocation to distinguish between
    /// different downstream miners while presenting them as a single entity
    /// to the upstream server. This is more efficient for pools with many
    /// miners.
    Aggregated,
    /// Each Sv1 downstream connection gets its own dedicated extended Sv2 channel.
    /// This mode provides complete isolation between downstream connections
    /// but may be less efficient for large numbers of miners.
    NonAggregated,
}

impl From<bool> for TproxyMode {
    fn from(aggregate: bool) -> Self {
        if aggregate {
            return TproxyMode::Aggregated;
        }

        TproxyMode::NonAggregated
    }
}

static TPROXY_MODE: OnceLock<TproxyMode> = OnceLock::new();

#[cfg(not(test))]
pub fn tproxy_mode() -> TproxyMode {
    *TPROXY_MODE.get().expect("TPROXY_MODE has to exist")
}

// We don’t initialize `TPROXY_MODE` in tests, so any test that
// depends on it will panic if the mode is undefined.
// This `cfg` wrapper ensures `tproxy_mode` does not panic in
// an undefined state by providing a default value when needed.
#[cfg(test)]
pub fn tproxy_mode() -> TproxyMode {
    *TPROXY_MODE.get_or_init(|| TproxyMode::Aggregated)
}

#[inline]
pub fn is_aggregated() -> bool {
    matches!(tproxy_mode(), TproxyMode::Aggregated)
}

#[inline]
pub fn is_non_aggregated() -> bool {
    matches!(tproxy_mode(), TproxyMode::NonAggregated)
}
