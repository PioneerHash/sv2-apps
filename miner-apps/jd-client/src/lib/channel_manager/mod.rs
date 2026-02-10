use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_channel::{Receiver, Sender};
use stratum_apps::{
    coinbase_output_constraints::coinbase_output_constraints_message,
    custom_mutex::Mutex,
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        bitcoin::{Amount, Target, TxOut},
        channels_sv2::{
            client::extended::ExtendedChannel,
            outputs::deserialize_outputs,
            server::{
                group::GroupChannel,
                jobs::{
                    extended::ExtendedJob, factory::JobFactory, job_store::DefaultJobStore,
                    standard::StandardJob,
                },
                standard::StandardChannel,
            },
            Vardiff, VardiffState,
        },
        framing_sv2,
        handlers_sv2::{
            HandleExtensionsFromServerAsync, HandleJobDeclarationMessagesFromServerAsync,
            HandleMiningMessagesFromClientAsync, HandleMiningMessagesFromServerAsync,
            HandleTemplateDistributionMessagesFromServerAsync,
        },
        job_declaration_sv2::{
            AllocateMiningJobToken, AllocateMiningJobTokenSuccess, DeclareMiningJob,
        },
        mining_sv2::{
            ExtendedExtranonce, OpenExtendedMiningChannel, SetCustomMiningJob, SetTarget,
            UpdateChannel,
        },
        noise_sv2::Responder,
        parsers_sv2::{AnyMessage, JobDeclaration, Mining, TemplateDistribution, Tlv},
        template_distribution_sv2::{NewTemplate, SetNewPrevHash as SetNewPrevHashTdp},
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{protocol_message_type, MessageType},
        types::{
            ChannelId, DownstreamId, Message, RequestId, SharesBatchSize, SharesPerMinute,
            Sv2Frame, TemplateId, UpstreamJobId, VardiffKey,
        },
    },
};
use tokio::{net::TcpListener, select, sync::broadcast};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::downstream_message_handler::RouteMessageTo,
    config::JobDeclaratorClientConfig,
    downstream::Downstream,
    ehash_mint::{
        EhashPubkeyStore, PendingChannelShares, RegisterChannelPubkeyData, ShareReportSender,
    },
    error::{self, JDCError, JDCErrorKind, JDCResult},
    status::{handle_error, Status, StatusSender},
    utils::{
        AtomicUpstreamState, DownstreamChannelJobId, PendingChannelRequest, ShutdownMessage,
        UpstreamState,
    },
};
mod downstream_message_handler;
mod extensions_message_handler;
mod jd_message_handler;
mod template_message_handler;
mod upstream_message_handler;

pub const JDC_SEARCH_SPACE_BYTES: usize = 4;
// These are only used for solo-mining, very similar to pool
const CLIENT_SEARCH_SPACE_BYTES: usize = 16;
pub const FULL_EXTRANONCE_SIZE: usize = JDC_SEARCH_SPACE_BYTES + CLIENT_SEARCH_SPACE_BYTES;

/// A `DeclaredJob` encapsulates all the relevant data associated with a single
/// job declaration, including its template, optional messages, coinbase output,
/// and transaction list.
#[derive(Clone, Debug)]
pub struct DeclaredJob {
    // The original `DeclareMiningJob` message associated with this job,
    // if one was sent.
    declare_mining_job: Option<DeclareMiningJob<'static>>,
    // The template associated with the declared job.
    template: NewTemplate<'static>,
    // The `SetNewPrevHashTdp` message associated with this job, if available.
    prev_hash: Option<SetNewPrevHashTdp<'static>>,
    // The `SetCustomMiningJob` message associated with this job,
    // if a custom job was created.
    set_custom_mining_job: Option<SetCustomMiningJob<'static>>,
    // The coinbase output for this job.
    coinbase_output: Vec<u8>,
    // The list of transactions included in the job’s template.
    tx_list: Vec<Vec<u8>>,
}

/// Central state container for the **Channel Manager**.
///
/// `ChannelManagerData` holds all runtime state that the JDC
/// needs to manage downstream clients, upstream connections, extranonce allocation,
/// job tracking, and various ID factories.  
pub struct ChannelManagerData {
    // Mapping of `downstream_id` → `Downstream` object,
    // used by the channel manager to locate and interact with downstream clients.
    pub downstream: HashMap<DownstreamId, Downstream>,
    // Extranonce prefix factory for **extended downstream channels**.
    // Each new extended downstream receives a unique extranonce prefix.
    extranonce_prefix_factory_extended: ExtendedExtranonce,
    // Extranonce prefix factory for **standard downstream channels**.
    // Each new standard downstream receives a unique extranonce prefix.
    extranonce_prefix_factory_standard: ExtendedExtranonce,
    // Factory that generates **monotonically increasing request IDs**
    // for messages sent from the JDC.
    request_id_factory: AtomicU32,
    // Factory that assigns a unique ID to each new **downstream connection**.
    downstream_id_factory: AtomicUsize,
    // Factory that assigns a unique **sequence number** to each share
    // submitted from the JDC to the upstream.
    sequence_number_factory: AtomicU32,
    // The last **future template** received from the upstream.
    last_future_template: Option<NewTemplate<'static>>,
    // The last **new prevhash** received from the upstream.
    last_new_prev_hash: Option<SetNewPrevHashTdp<'static>>,
    // The most recent set of **allocation tokens** received from the JDS.
    allocate_tokens: Option<AllocateMiningJobTokenSuccess<'static>>,
    // Stores new templates as they arrive, mapped by their **template ID**.
    template_store: HashMap<TemplateId, NewTemplate<'static>>,
    // Stores the last declared job, keyed by the `request_id` used when
    // declaring the job to the JDS.
    // This is later used to send a `SetCustomMiningJob`.
    last_declare_job_store: HashMap<RequestId, DeclaredJob>,
    // Maps a template ID → corresponding upstream job ID.
    template_id_to_upstream_job_id: HashMap<TemplateId, UpstreamJobId>,
    // Maps a downstream ID + channel_id + job ID → corresponding template ID.
    downstream_channel_id_and_job_id_to_template_id: HashMap<DownstreamChannelJobId, TemplateId>,
    // The coinbase outputs currently in use.
    coinbase_outputs: Vec<u8>,
    // The active upstream extended channel (client-side instance), if any.
    pub upstream_channel: Option<ExtendedChannel<'static>>,
    // Optional "pool tag" string, identifying the pool.
    pool_tag_string: Option<String>,
    // List of pending downstream connection requests,
    // persisted while the JDC is opening a channel with the upstream.
    pending_downstream_requests: VecDeque<PendingChannelRequest>,
    // Factory for creating **custom mining jobs**, if available.
    job_factory: Option<JobFactory>,
    // Mapping of `(downstream_id, channel_id)` → vardiff controller.
    // Each entry manages variable difficulty for a specific downstream channel.
    vardiff: HashMap<VardiffKey, VardiffState>,
    /// Extensions that have been successfully negotiated with the upstream server
    negotiated_extensions: Vec<u16>,
    /// Extensions that the JDC supports
    supported_extensions: Vec<u16>,
    /// Extensions that the JDC requires
    required_extensions: Vec<u16>,
    /// Stores the mapping of (downstream_id, channel_id) → EhashPubkey for ehash minting
    pub ehash_pubkey_store: EhashPubkeyStore,
    /// Sender for share reports to ehash-mint (no-op if ehash-mint not configured)
    pub ehash_report_sender: ShareReportSender,
    /// Buffered shares for channels awaiting hpub registration via RegisterChannelPubkey.
    /// Key is (downstream_id, channel_id).
    pub pending_ehash_shares: HashMap<(usize, u32), PendingChannelShares>,
    /// Timeout in seconds for receiving RegisterChannelPubkey after channel open.
    /// If exceeded, the downstream is disconnected.
    pub ehash_pubkey_timeout_secs: u64,
    /// Current estimated block height (incremented on each SetNewPrevHash).
    /// This is an estimate - the actual height is not included in Sv2 messages.
    pub estimated_block_height: u64,
}

impl ChannelManagerData {
    /// Resets the internal state of the Channel Manager.
    ///
    /// This method is primarily used during **fallback scenarios** to clear and
    /// reinitialize all internal data structures. It ensures that the Channel Manager
    /// returns to a clean state, ready to handle fresh upstream or downstream connections.
    pub fn reset(&mut self, coinbase_outputs: Vec<u8>) {
        self.downstream.clear();
        self.template_store.clear();
        self.last_declare_job_store.clear();
        self.template_id_to_upstream_job_id.clear();
        self.downstream_channel_id_and_job_id_to_template_id.clear();
        self.pending_downstream_requests.clear();

        self.downstream_id_factory = AtomicUsize::new(0);
        self.request_id_factory = AtomicU32::new(0);

        let (range_0, range_1, range_2) = {
            let range_1 = 0..JDC_SEARCH_SPACE_BYTES;
            (
                0..range_1.start,
                range_1.clone(),
                range_1.end..FULL_EXTRANONCE_SIZE,
            )
        };
        self.extranonce_prefix_factory_extended =
            ExtendedExtranonce::new(range_0.clone(), range_1.clone(), range_2.clone(), None)
                .expect("valid ranges");
        self.extranonce_prefix_factory_standard =
            ExtendedExtranonce::new(range_0, range_1, range_2, None).expect("valid ranges");

        self.allocate_tokens = None;
        self.upstream_channel = None;
        self.pool_tag_string = None;

        self.coinbase_outputs = coinbase_outputs;

        // Clear ehash pubkey store on reset (channels are being reset)
        self.ehash_pubkey_store = EhashPubkeyStore::new();
        // Clear pending ehash shares on reset
        self.pending_ehash_shares.clear();
        // Note: ehash_report_sender is preserved - connection to mint stays open
        // Note: ehash_pubkey_timeout_secs is preserved - it's configuration
    }
}

/// Represents all communication channels managed by the Channel Manager.
///
/// The `ChannelManagerChannel` holds all the asynchronous communication primitives
/// required for message exchange between the **Channel Manager** and other subsystems.
/// It ensures decoupled, structured communication between upstreams, downstreams,
/// the Job Dispatcher Service (JDS), and the Template Provider (TP).
///
/// # Channels
/// 1. **Upstream**:
///    - `(upstream_sender, upstream_receiver)` Used to send and receive messages from the upstream
///      subsystem.
///
/// 2. **JDS**:
///    - `(jd_sender, jd_receiver)` Handles communication with JDS.
///
/// 3. **Template Provider**:
///    - `(tp_sender, tp_receiver)` Manages communication with the Template Provider.
///
/// 4. **Downstream**:
///    - `(downstream_sender, downstream_receiver)` Broadcasts messages to all downstream clients
///      and receives messages from them.
///
/// 5. **Status**:
///    - `status_sender` Allows the Channel Manager to notify the main status loop of critical state
///      changes.

#[derive(Clone)]
pub struct ChannelManagerChannel {
    upstream_sender: Sender<Sv2Frame>,
    upstream_receiver: Receiver<Sv2Frame>,
    jd_sender: Sender<JobDeclaration<'static>>,
    jd_receiver: Receiver<JobDeclaration<'static>>,
    tp_sender: Sender<TemplateDistribution<'static>>,
    tp_receiver: Receiver<TemplateDistribution<'static>>,
    downstream_sender: broadcast::Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
    downstream_receiver: Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
    /// Sender for ehash extension messages to downstreams (cloned per downstream).
    ehash_sender: Sender<RegisterChannelPubkeyData>,
    /// Receiver for ehash extension messages from downstreams.
    ehash_receiver: Receiver<RegisterChannelPubkeyData>,
}

/// Contains all the state of mutable and immutable data required
/// by channel manager to process its task along with channels
/// to perform message traversal.
#[derive(Clone)]
pub struct ChannelManager {
    pub channel_manager_data: Arc<Mutex<ChannelManagerData>>,
    channel_manager_channel: ChannelManagerChannel,
    miner_tag_string: String,
    share_batch_size: SharesBatchSize,
    shares_per_minute: SharesPerMinute,
    user_identity: String,
    /// This represent the current state of Upstream channel
    /// 1. NoChannel: No active upstream connection.
    /// 2. Pending: A channel request has been sent, awaiting response.
    /// 3. Connected: An upstream channel is successfully established.
    /// 4. SoloMining: No upstream is available; the JDC operates in solo mining mode. case.
    pub upstream_state: AtomicUpstreamState,
    /// Whether developer mode is enabled (skips share PoW validation).
    developer_mode: bool,
    /// Last time a developer mode warning was logged (Unix timestamp in seconds).
    developer_mode_last_warning: Arc<AtomicU64>,
    /// Interval in minutes between developer mode warnings.
    developer_mode_warning_interval_mins: u64,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl ChannelManager {
    /// Constructor method used to instantiate the Channel Manager
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: JobDeclaratorClientConfig,
        upstream_sender: Sender<Sv2Frame>,
        upstream_receiver: Receiver<Sv2Frame>,
        jd_sender: Sender<JobDeclaration<'static>>,
        jd_receiver: Receiver<JobDeclaration<'static>>,
        tp_sender: Sender<TemplateDistribution<'static>>,
        tp_receiver: Receiver<TemplateDistribution<'static>>,
        downstream_sender: broadcast::Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        downstream_receiver: Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        coinbase_outputs: Vec<u8>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        ehash_report_sender: ShareReportSender,
    ) -> JDCResult<Self, error::ChannelManager> {
        let (range_0, range_1, range_2) = {
            let range_1 = 0..JDC_SEARCH_SPACE_BYTES;
            (
                0..range_1.start,
                range_1.clone(),
                range_1.end..FULL_EXTRANONCE_SIZE,
            )
        };

        let make_extranonce_factory = || {
            ExtendedExtranonce::new(range_0.clone(), range_1.clone(), range_2.clone(), None)
                .expect("Failed to create ExtendedExtranonce with valid ranges")
        };

        let extranonce_prefix_factory_extended = make_extranonce_factory();
        let extranonce_prefix_factory_standard = make_extranonce_factory();

        let channel_manager_data = Arc::new(Mutex::new(ChannelManagerData {
            downstream: HashMap::new(),
            extranonce_prefix_factory_extended,
            extranonce_prefix_factory_standard,
            downstream_id_factory: AtomicUsize::new(0),
            request_id_factory: AtomicU32::new(0),
            sequence_number_factory: AtomicU32::new(0),
            last_future_template: None,
            last_new_prev_hash: None,
            allocate_tokens: None,
            template_store: HashMap::new(),
            last_declare_job_store: HashMap::new(),
            template_id_to_upstream_job_id: HashMap::new(),
            downstream_channel_id_and_job_id_to_template_id: HashMap::new(),
            coinbase_outputs,
            upstream_channel: None,
            pool_tag_string: None,
            pending_downstream_requests: VecDeque::new(),
            job_factory: None,
            vardiff: HashMap::new(),
            negotiated_extensions: vec![],
            supported_extensions,
            required_extensions,
            ehash_pubkey_store: EhashPubkeyStore::new(),
            ehash_report_sender,
            pending_ehash_shares: HashMap::new(),
            ehash_pubkey_timeout_secs: config.ehash_pubkey_timeout_secs().unwrap_or(5),
            estimated_block_height: 0,
        }));

        // Create ehash channel for RegisterChannelPubkey messages from downstreams
        let (ehash_sender, ehash_receiver) = async_channel::unbounded();

        let channel_manager_channel = ChannelManagerChannel {
            upstream_sender,
            upstream_receiver,
            jd_sender,
            jd_receiver,
            tp_sender,
            tp_receiver,
            downstream_sender,
            downstream_receiver,
            ehash_sender,
            ehash_receiver,
        };

        let developer_mode = config.is_developer_mode();
        let developer_mode_warning_interval_mins = config.developer_mode_warning_interval_mins();

        // Log startup warning if developer mode is enabled
        if developer_mode {
            warn!(
                "DEVELOPER MODE ENABLED - Share PoW validation is DISABLED. \
                 This is UNSAFE for production use. Shares will be accepted without proof-of-work verification."
            );
        }

        let channel_manager = ChannelManager {
            channel_manager_data,
            channel_manager_channel,
            share_batch_size: config.share_batch_size(),
            shares_per_minute: config.shares_per_minute(),
            miner_tag_string: config.jdc_signature().to_string(),
            user_identity: config.user_identity().to_string(),
            upstream_state: AtomicUpstreamState::new(UpstreamState::SoloMining),
            developer_mode,
            developer_mode_last_warning: Arc::new(AtomicU64::new(0)),
            developer_mode_warning_interval_mins,
        };

        Ok(channel_manager)
    }

    /// Logs a periodic warning when developer mode is active.
    /// This method rate-limits warnings to once per configured interval.
    pub fn log_developer_mode_warning(&self) {
        if !self.developer_mode {
            return;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let last_warning = self.developer_mode_last_warning.load(Ordering::Relaxed);
        let interval_secs = self.developer_mode_warning_interval_mins * 60;

        if now >= last_warning + interval_secs {
            if self
                .developer_mode_last_warning
                .compare_exchange(last_warning, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                warn!(
                    "DEVELOPER MODE ACTIVE - Share PoW validation is DISABLED. \
                     This warning repeats every {} minutes.",
                    self.developer_mode_warning_interval_mins
                );
            }
        }
    }

    /// Returns whether developer mode is enabled.
    pub fn is_developer_mode(&self) -> bool {
        self.developer_mode
    }

    // Bootstraps a group channel with the given parameters.
    // Returns a `GroupChannel` if successful, otherwise returns `None`.
    //
    // To be called before calling Downstream::new.
    fn bootstrap_group_channel(
        &self,
        channel_id: ChannelId,
    ) -> Option<GroupChannel<'static, DefaultJobStore<ExtendedJob<'static>>>> {
        let (full_extranonce_size, pool_tag_string, last_future_template, last_new_prev_hash) =
            self.channel_manager_data.super_safe_lock(|data| {
                (
                    data.upstream_channel
                        .as_ref()
                        .map(|channel| channel.get_full_extranonce_size())
                        .unwrap_or(FULL_EXTRANONCE_SIZE), /* Default to FULL_EXTRANONCE_SIZE if
                                                           * upstream channel is not present
                                                           * (solo mining mode) */
                    data.pool_tag_string.clone(),
                    data.last_future_template
                        .clone()
                        .expect("No future template found after readiness check"),
                    data.last_new_prev_hash
                        .clone()
                        .expect("No new prevhash found after readiness check"),
                )
            });
        let miner_tag_string = self.miner_tag_string.clone();
        let mut group_channel = match GroupChannel::new_for_job_declaration_client(
            channel_id,
            DefaultJobStore::new(),
            full_extranonce_size,
            pool_tag_string.clone(),
            miner_tag_string.clone(),
        ) {
            Ok(channel) => channel,
            Err(e) => {
                error!(error = ?e, "Failed to create group channel");
                return None;
            }
        };

        let coinbase_outputs = self
            .channel_manager_data
            .super_safe_lock(|data| data.coinbase_outputs.clone());
        let mut coinbase_outputs = match deserialize_outputs(coinbase_outputs) {
            Ok(outputs) => outputs,
            Err(e) => {
                error!(error = ?e, "Failed to deserialize coinbase outputs");
                return None;
            }
        };

        coinbase_outputs[0].value =
            Amount::from_sat(last_future_template.coinbase_tx_value_remaining);

        if let Err(e) =
            group_channel.on_new_template(last_future_template, coinbase_outputs.clone())
        {
            error!(error = ?e, "Failed to add template to group channel");
            return None;
        }

        if let Err(e) = group_channel.on_set_new_prev_hash(last_new_prev_hash) {
            error!(error = ?e, "Failed to set new prevhash for group channel");
            return None;
        }

        Some(group_channel)
    }

    /// Starts the downstream server, and accepts new connection request.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_downstream_server(
        self,
        authority_public_key: Secp256k1PublicKey,
        authority_secret_key: Secp256k1SecretKey,
        cert_validity_sec: u64,
        listening_address: SocketAddr,
        task_manager: Arc<TaskManager>,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        status_sender: Sender<Status>,
        channel_manager_sender: Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        channel_manager_receiver: broadcast::Sender<(
            DownstreamId,
            Mining<'static>,
            Option<Vec<Tlv>>,
        )>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> JDCResult<(), error::ChannelManager> {
        let mut shutdown_rx = notify_shutdown.subscribe();

        // Wait for initial template and prevhash before accepting connections
        loop {
            let has_required_data = self.channel_manager_data.super_safe_lock(|data| {
                data.last_future_template.is_some() && data.last_new_prev_hash.is_some()
            });

            if has_required_data {
                info!("Required template data received, ready to accept connections");
                break;
            }

            warn!("Waiting for initial template and prevhash from Template Provider...");
            select! {
                message = shutdown_rx.recv() => {
                    match message {
                        Ok(ShutdownMessage::ShutdownAll) => {
                            info!("Channel Manager: received shutdown while waiting for templates");
                            return Ok(());
                        }
                        Err(e) => {
                            warn!(error = ?e, "shutdown channel closed unexpectedly");
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }

        info!("Starting downstream server at {listening_address}");
        let server = TcpListener::bind(listening_address).await.map_err(|e| {
            error!(error = ?e, "Failed to bind downstream server at {listening_address}");
            JDCError::shutdown(e)
        })?;

        let task_manager_clone = task_manager.clone();
        task_manager.spawn(async move {

            loop {
                select! {
                    message = shutdown_rx.recv() => {
                        match message {
                            Ok(ShutdownMessage::ShutdownAll) => {
                                info!("Channel Manager: received shutdown signal");
                                break;
                            }
                            Ok(ShutdownMessage::JobDeclaratorShutdownFallback(_)) => {
                                info!("Downstream Server: received job declarator shutdown signal");
                                break;
                            }
                            Ok(ShutdownMessage::UpstreamShutdownFallback(_)) => {
                                info!("Downstream Server: received upstream shutdown signal");
                                break;
                            }
                            Err(e) => {
                                warn!(error = ?e, "shutdown channel closed unexpectedly");
                                break;
                            }
                            _ => {}
                        }
                    }
                    res = server.accept() => {
                        match res {
                            Ok((stream, socket_address)) => {
                                info!(%socket_address, "New downstream connection");
                                let responder = match Responder::from_authority_kp(
                                    &authority_public_key.into_bytes(),
                                    &authority_secret_key.into_bytes(),
                                    std::time::Duration::from_secs(cert_validity_sec),
                                ) {
                                    Ok(r) => r,
                                    Err(e) => {
                                        error!(error = ?e, "Failed to create responder");
                                        continue;
                                    }
                                };
                                let noise_stream = match NoiseTcpStream::<Message>::new(
                                    stream,
                                    stratum_apps::stratum_core::codec_sv2::HandshakeRole::Responder(responder),
                                )
                                .await
                                {
                                    Ok(ns) => ns,
                                    Err(e) => {
                                        error!(error = ?e, "Noise handshake failed");
                                        continue;
                                    }
                                };

                                let downstream_id = self
                                    .channel_manager_data
                                    .super_safe_lock(|data| data.downstream_id_factory.fetch_add(1, Ordering::Relaxed));

                                let channel_id_factory = AtomicU32::new(1);
                                let group_channel_id = channel_id_factory.fetch_add(1, Ordering::SeqCst);

                                let group_channel = match self.bootstrap_group_channel(
                                    group_channel_id,
                                ) {
                                    Some(group_channel) => group_channel,
                                    None => {
                                        error!("Failed to bootstrap group channel - disconnecting downstream client with id {downstream_id}");
                                        let e = JDCError::<error::ChannelManager>::disconnect(JDCErrorKind::CouldNotInitiateSystem, downstream_id);
                                        handle_error(&StatusSender::ChannelManager(status_sender.clone()), e).await;
                                        break;
                                    }
                                };

                                let downstream = Downstream::new(
                                    downstream_id,
                                    channel_id_factory,
                                    group_channel,
                                    channel_manager_sender.clone(),
                                    channel_manager_receiver.clone(),
                                    self.channel_manager_channel.ehash_sender.clone(),
                                    noise_stream,
                                    notify_shutdown.clone(),
                                    task_manager_clone.clone(),
                                    status_sender.clone(),
                                    supported_extensions.clone(),
                                    required_extensions.clone(),
                                );

                                self.channel_manager_data.super_safe_lock(|data| {
                                    data.downstream.insert(downstream_id, downstream.clone());
                                });

                                downstream
                                    .start(
                                        notify_shutdown.clone(),
                                        status_sender.clone(),
                                        task_manager_clone.clone(),
                                    )
                                    .await;
                                }

                                Err(e) => {
                                    error!(error = ?e, "Failed to accept new downstream connection");
                                }
                            }
                    }
                }
            }
            info!("Downstream server: Unified loop break");
        });
        Ok(())
    }

    /// The central orchestrator of the Channel Manager.  
    ///  
    /// Responsible for receiving messages from all subsystems, processing them,  
    /// and either forwarding them to the appropriate subsystem or updating  
    /// the internal state of the Channel Manager as needed.
    pub async fn start(
        mut self,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
        coinbase_outputs: Vec<TxOut>,
    ) {
        let status_sender = StatusSender::ChannelManager(status_sender);
        let mut shutdown_rx = notify_shutdown.subscribe();

        if let Err(e) = self.coinbase_output_constraints(coinbase_outputs).await {
            error!(error = ?e, "Failed to send CoinbaseOutputConstraints message to TP");
            handle_error(&status_sender, e).await;
            return;
        }

        task_manager.spawn(async move {
            let cm = self.clone();
            let vd = self.clone();
            let vardiff_future = vd.run_vardiff_loop();
            tokio::pin!(vardiff_future);
            loop {
                let mut cm_jds = cm.clone();
                let mut cm_pool = cm.clone();
                let mut cm_template = cm.clone();
                let mut cm_downstreams = cm.clone();
                tokio::select! {
                    message = shutdown_rx.recv() => {
                        match message {
                            Ok(ShutdownMessage::ShutdownAll) => {
                                info!("Channel Manager: received shutdown signal");
                                break;
                            }
                            Ok(ShutdownMessage::DownstreamShutdown(downstream_id)) => {
                                info!(%downstream_id, "Channel Manager: removing downstream after shutdown");
                                if let Err(e) = self.remove_downstream(downstream_id) {
                                    tracing::error!(%downstream_id, error = ?e, "Failed to remove downstream");
                                }
                            }
                            Ok(ShutdownMessage::JobDeclaratorShutdownFallback((coinbase_outputs,tx))) => {
                                info!("Channel Manager: Job declarator shutdown signal");
                                self.upstream_state.set(UpstreamState::SoloMining);
                                self.channel_manager_data.super_safe_lock(|data| data.reset(coinbase_outputs));
                                drop(tx);
                            }
                            Ok(ShutdownMessage::UpstreamShutdownFallback((coinbase_outputs,tx))) => {
                                info!("Channel Manager: Upstream shutdown signal");
                                self.upstream_state.set(UpstreamState::SoloMining);
                                self.channel_manager_data.super_safe_lock(|data| data.reset(coinbase_outputs));
                                drop(tx);
                            }
                            Err(e) => {
                                warn!(error = ?e, "shutdown channel closed unexpectedly");
                                break;
                            }
                            _ => {}
                        }
                    }
                    res = &mut vardiff_future => {
                        info!("Vardiff loop completed with: {res:?}");
                    }
                    res = cm_jds.handle_jds_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling JDS message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = cm_pool.handle_pool_message_frame() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Pool message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = cm_template.handle_template_provider_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Template Receiver message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = cm_downstreams.handle_downstream_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Downstreams message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = self.handle_ehash_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling ehash message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Handle RegisterChannelPubkey messages from downstreams.
    ///
    /// When a downstream (translator) sends a RegisterChannelPubkey message after
    /// receiving mining.authorize, this handler:
    /// 1. Stores the pubkey in ehash_pubkey_store
    /// 2. Processes any buffered shares for that channel
    async fn handle_ehash_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        if let Ok(data) = self.channel_manager_channel.ehash_receiver.recv().await {
            info!(
                downstream_id = data.downstream_id,
                channel_id = data.channel_id,
                pubkey = %data.pubkey.to_bech32(),
                "Processing RegisterChannelPubkey"
            );

            self.channel_manager_data.super_safe_lock(|cm_data| {
                // Store the pubkey
                cm_data.ehash_pubkey_store.insert(
                    data.downstream_id,
                    data.channel_id,
                    data.pubkey.clone(),
                );

                // Check for buffered shares
                let key = (data.downstream_id, data.channel_id);
                if let Some(pending) = cm_data.pending_ehash_shares.remove(&key) {
                    let share_count = pending.shares.len();
                    info!(
                        downstream_id = data.downstream_id,
                        channel_id = data.channel_id,
                        share_count,
                        "Processing buffered shares after RegisterChannelPubkey"
                    );

                    // Send all buffered shares to ehash-mint
                    for share in pending.shares {
                        let report = share.to_report(&data.pubkey);
                        cm_data.ehash_report_sender.try_send_share(report);
                    }
                }
            });
        }
        Ok(())
    }

    // Removes a downstream entry from the Channel Manager’s state.
    //
    // Given a `downstream_id`, this method:
    // 1. Removes the corresponding downstream from the `downstream` map.
    #[allow(clippy::result_large_err)]
    fn remove_downstream(
        &mut self,
        downstream_id: DownstreamId,
    ) -> JDCResult<(), error::ChannelManager> {
        self.channel_manager_data.super_safe_lock(|cm_data| {
            cm_data.downstream.remove(&downstream_id);
            cm_data
                .downstream_channel_id_and_job_id_to_template_id
                .retain(|key, _| key.downstream_id != downstream_id);
            cm_data
                .vardiff
                .retain(|key, _| key.downstream_id != downstream_id);
            // Clean up ehash pubkey store entries for this downstream
            cm_data.ehash_pubkey_store.remove_downstream(downstream_id);
        });
        Ok(())
    }

    /// Handles messages received from the JDS subsystem.  
    ///  
    /// This method listens for incoming frames on the `jd_receiver` channel.  
    /// - If the frame contains a JobDeclaration message, it forwards it to the   job declaration
    ///   message handler.
    /// - If the frame contains any unsupported message type, an error is returned.
    async fn handle_jds_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        if let Ok(message) = self.channel_manager_channel.jd_receiver.recv().await {
            self.handle_job_declaration_message_from_server(None, message, None)
                .await?;
        }
        Ok(())
    }

    /// Handles messages received from the Upstream subsystem.  
    ///  
    /// This method listens for incoming frames on the `upstream_receiver` channel.  
    /// - If the frame contains a **Mining** message, it forwards it to the   mining message
    ///   handler.
    /// - If the frame contains any unsupported message type, an error is returned.
    async fn handle_pool_message_frame(&mut self) -> JDCResult<(), error::ChannelManager> {
        if let Ok(mut sv2_frame) = self.channel_manager_channel.upstream_receiver.recv().await {
            let header = sv2_frame.get_header().ok_or_else(|| {
                error!("SV2 frame missing header");
                JDCError::fallback(framing_sv2::Error::MissingHeader)
            })?;
            let message_type = header.msg_type();
            let extension_type = header.ext_type();
            let payload = sv2_frame.payload();
            match protocol_message_type(extension_type, message_type) {
                MessageType::Mining => {
                    self.handle_mining_message_frame_from_server(None, header, payload)
                        .await?;
                }
                MessageType::Extensions => {
                    self.handle_extensions_message_frame_from_server(None, header, payload)
                        .await?;
                }
                _ => {
                    warn!("Received unsupported message type from upstream: {message_type}");
                    return Err(JDCError::log(JDCErrorKind::UnexpectedMessage(
                        extension_type,
                        message_type,
                    )));
                }
            }
        }
        Ok(())
    }

    // Handles messages received from the TP subsystem.
    //
    // This method listens for incoming frames on the `tp_receiver` channel.
    // - If the frame contains a TemplateDistribution message, it forwards it to the   template
    //   distribution message handler.
    // - If the frame contains any unsupported message type, an error is returned.
    async fn handle_template_provider_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        if let Ok(message) = self.channel_manager_channel.tp_receiver.recv().await {
            self.handle_template_distribution_message_from_server(None, message, None)
                .await?;
        }
        Ok(())
    }

    // Handles messages received from downstream clients and routes them appropriately.
    //
    // # Overview
    // This method is similar to the upstream JDS message handler, but introduces additional
    // logic for handling OpenChannel requests (both standard and extended).
    //
    // # Message Flow
    // - For most mining messages: The message is forwarded directly to
    //   `handle_mining_message_from_client`.
    // - For OpenChannel messages: At the time of request, the `channel_id` is not yet assigned, so
    //   we cannot map the message back to the downstream. To solve this:
    //   1. The `downstream_id` is appended to the `user_identity` (e.g.,
    //      `"identity#downstream_id"`).
    //   2. Later, the appended downstream ID is stripped and used by the message handler to
    //      correctly attribute the request.
    //
    // # Channel Establishment Logic
    // - NoChannel → Pending:
    //   - The first downstream OpenChannel request is stored in `pending_downstream_requests`.
    //   - The upstream state transitions from `NoChannel` to `Pending`.
    //   - A single channel request is then sent to the upstream (JDC → upstream).
    //
    // - Pending:
    //   - Additional downstream OpenChannel requests are stored in `pending_downstream_requests`
    //     until the upstream connection is established.
    //
    // - Connected / SoloMining:
    //   - Downstream OpenChannel requests are immediately forwarded to the mining handler.
    //
    // # Notes
    // - Only one upstream channel is created per JDC instance.
    // - After the upstream channel is established, all new downstream requests bypass the pending
    //   mechanism and are sent directly to the mining handler.
    async fn handle_downstream_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        if let Ok((downstream_id, message, tlvs)) = self
            .channel_manager_channel
            .downstream_receiver
            .recv()
            .await
        {
            match message {
                Mining::OpenExtendedMiningChannel(downstream_channel_request) => {
                    let downstream_msg = downstream_channel_request.clone().into_static();

                    match self.upstream_state.get() {
                        UpstreamState::NoChannel => {
                            self.channel_manager_data.super_safe_lock(|data| {
                                data.pending_downstream_requests
                                    .push_front((downstream_id, downstream_msg).into());
                            });

                            if self
                                .upstream_state
                                .compare_and_set(UpstreamState::NoChannel, UpstreamState::Pending)
                                .is_ok()
                            {
                                let mut upstream_message = downstream_channel_request;
                                upstream_message.user_identity = self
                                    .user_identity
                                    .clone()
                                    .try_into()
                                    .map_err(JDCError::shutdown)?;
                                upstream_message.request_id = 1;
                                upstream_message.min_extranonce_size +=
                                    JDC_SEARCH_SPACE_BYTES as u16;
                                let upstream_message =
                                    Mining::OpenExtendedMiningChannel(upstream_message)
                                        .into_static();
                                let sv2_frame: Sv2Frame = AnyMessage::Mining(upstream_message)
                                    .try_into()
                                    .map_err(JDCError::shutdown)?;
                                self.channel_manager_channel
                                    .upstream_sender
                                    .send(sv2_frame)
                                    .await
                                    .map_err(|_| {
                                        JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                                    })?;
                            }
                        }
                        UpstreamState::Pending => {
                            self.channel_manager_data.super_safe_lock(|data| {
                                data.pending_downstream_requests
                                    .push_back((downstream_id, downstream_msg).into());
                            });
                        }
                        UpstreamState::Connected => {
                            self.send_open_channel_request_to_mining_handler(
                                downstream_id,
                                Mining::OpenExtendedMiningChannel(downstream_msg),
                                tlvs.as_deref(),
                            )
                            .await?;
                        }
                        UpstreamState::SoloMining => {
                            self.send_open_channel_request_to_mining_handler(
                                downstream_id,
                                Mining::OpenExtendedMiningChannel(downstream_msg),
                                tlvs.as_deref(),
                            )
                            .await?;
                        }
                    }
                }
                Mining::OpenStandardMiningChannel(downstream_channel_request) => {
                    let downstream_msg = downstream_channel_request.clone().into_static();

                    match self.upstream_state.get() {
                        UpstreamState::NoChannel => {
                            self.channel_manager_data.super_safe_lock(|data| {
                                data.pending_downstream_requests
                                    .push_front((downstream_id, downstream_msg).into())
                            });

                            if self
                                .upstream_state
                                .compare_and_set(UpstreamState::NoChannel, UpstreamState::Pending)
                                .is_ok()
                            {
                                let upstream_open = OpenExtendedMiningChannel {
                                    user_identity: self.user_identity.clone().try_into().unwrap(),
                                    request_id: 1,
                                    nominal_hash_rate: downstream_channel_request.nominal_hash_rate,
                                    max_target: downstream_channel_request.max_target,
                                    min_extranonce_size: JDC_SEARCH_SPACE_BYTES as u16,
                                };

                                let message =
                                    Mining::OpenExtendedMiningChannel(upstream_open).into_static();
                                let sv2_frame: Sv2Frame = AnyMessage::Mining(message)
                                    .try_into()
                                    .map_err(JDCError::shutdown)?;
                                self.channel_manager_channel
                                    .upstream_sender
                                    .send(sv2_frame)
                                    .await
                                    .map_err(|_| {
                                        JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                                    })?;
                            }
                        }
                        UpstreamState::Pending => {
                            self.channel_manager_data.super_safe_lock(|data| {
                                data.pending_downstream_requests
                                    .push_back((downstream_id, downstream_msg).into())
                            });
                        }
                        UpstreamState::Connected => {
                            self.send_open_channel_request_to_mining_handler(
                                downstream_id,
                                Mining::OpenStandardMiningChannel(downstream_msg),
                                tlvs.as_deref(),
                            )
                            .await?;
                        }
                        UpstreamState::SoloMining => {
                            self.send_open_channel_request_to_mining_handler(
                                downstream_id,
                                Mining::OpenStandardMiningChannel(downstream_msg),
                                tlvs.as_deref(),
                            )
                            .await?;
                        }
                    }
                }
                _ => {
                    self.handle_mining_message_from_client(
                        Some(downstream_id),
                        message,
                        tlvs.as_deref(),
                    )
                    .await?;
                }
            }
        }

        Ok(())
    }

    // Utility method to send open channel request from downstream to message handler.
    #[inline]
    async fn send_open_channel_request_to_mining_handler(
        &mut self,
        downstream_id: DownstreamId,
        message: Mining<'_>,
        tlvs: Option<&[Tlv]>,
    ) -> JDCResult<(), error::ChannelManager> {
        self.handle_mining_message_from_client(Some(downstream_id), message, tlvs)
            .await?;
        Ok(())
    }

    /// Utility method to request for more token to JDS.
    pub async fn allocate_tokens(
        &self,
        token_to_allocate: u32,
    ) -> JDCResult<(), error::ChannelManager> {
        debug!("Allocating {} job tokens", token_to_allocate);

        for i in 0..token_to_allocate {
            let request_id = self
                .channel_manager_data
                .super_safe_lock(|data| data.request_id_factory.fetch_add(1, Ordering::Relaxed));

            debug!(
                request_id,
                "Allocating token {}/{}",
                i + 1,
                token_to_allocate
            );

            let message = JobDeclaration::AllocateMiningJobToken(AllocateMiningJobToken {
                user_identifier: self
                    .user_identity
                    .to_string()
                    .try_into()
                    .expect("Static string should always convert"),
                request_id,
            });

            self.channel_manager_channel
                .jd_sender
                .send(message)
                .await
                .map_err(|e| {
                    info!(error = ?e, "Failed to send AllocateMiningJobToken frame");
                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                })?;
        }

        info!("Requested allocation of {token_to_allocate} mining job tokens to JDS");
        Ok(())
    }

    // Runs the vardiff on extended channel.
    fn run_vardiff_on_extended_channel(
        downstream_id: DownstreamId,
        channel_id: ChannelId,
        channel_state: &mut stratum_apps::stratum_core::channels_sv2::server::extended::ExtendedChannel<
            'static,
            DefaultJobStore<ExtendedJob<'static>>,
        >,
        vardiff_state: &mut VardiffState,
        updates: &mut Vec<RouteMessageTo>,
    ) {
        let (hashrate, target, shares_per_minute) = (
            channel_state.get_nominal_hashrate(),
            channel_state.get_target(),
            channel_state.get_shares_per_minute(),
        );

        let Ok(new_hashrate_opt) = vardiff_state.try_vardiff(hashrate, target, shares_per_minute)
        else {
            debug!("Vardiff computation failed for extended channel {channel_id}");
            return;
        };

        let Some(new_hashrate) = new_hashrate_opt else {
            return;
        };

        match channel_state.update_channel(new_hashrate, None) {
            Ok(()) => {
                let updated_target = channel_state.get_target();
                updates.push(
                    (
                        downstream_id,
                        Mining::SetTarget(SetTarget {
                            channel_id,
                            maximum_target: updated_target.to_le_bytes().into(),
                        }),
                    )
                        .into(),
                );
                debug!("Updated target for extended channel_id={channel_id} to {updated_target:?}",);
            }
            Err(e) => warn!(
                "Failed to update extended channel channel_id={channel_id} during vardiff {e:?}"
            ),
        }
    }

    // Runs the vardiff on the standard channel.
    fn run_vardiff_on_standard_channel(
        downstream_id: DownstreamId,
        channel_id: ChannelId,
        channel: &mut StandardChannel<'static, DefaultJobStore<StandardJob<'static>>>,
        vardiff_state: &mut VardiffState,
        updates: &mut Vec<RouteMessageTo>,
    ) {
        let hashrate = channel.get_nominal_hashrate();
        let target = channel.get_target();
        let shares_per_minute = channel.get_shares_per_minute();

        let Ok(new_hashrate_opt) = vardiff_state.try_vardiff(hashrate, target, shares_per_minute)
        else {
            debug!("Vardiff computation failed for standard channel {channel_id}");
            return;
        };

        if let Some(new_hashrate) = new_hashrate_opt {
            match channel.update_channel(new_hashrate, None) {
                Ok(()) => {
                    let updated_target = channel.get_target();
                    updates.push(
                        (
                            downstream_id,
                            Mining::SetTarget(SetTarget {
                                channel_id,
                                maximum_target: updated_target.to_le_bytes().into(),
                            }),
                        )
                            .into(),
                    );
                    debug!("Updated target for standard channel channel_id={channel_id} to {updated_target:?}");
                }
                Err(e) => warn!(
                    "Failed to update standard channel channel_id={channel_id} during vardiff {e:?}"
                ),
            }
        }
    }

    // Periodic vardiff task loop.
    //
    // # Purpose
    // - Executes the vardiff cycle every 60 seconds for all downstreams.
    // - Delegates to [`Self::run_vardiff`] on each tick.
    async fn run_vardiff_loop(&self) -> JDCResult<(), error::ChannelManager> {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            info!("Starting vardiff loop for downstreams");

            if let Err(e) = self.run_vardiff().await {
                error!(error = ?e, "Vardiff iteration failed");
            }
        }
    }

    // Runs vardiff across **all channels** and generates updates.
    //
    // # Purpose
    // - Iterates through all downstream channels (both standard and extended).
    // - Runs vardiff for each channel and collects the resulting updates.
    // - Propagates difficulty changes to downstreams and also sends an `UpdateChannel` message
    //   upstream if applicable.
    async fn run_vardiff(&self) -> JDCResult<(), error::ChannelManager> {
        let mut messages: Vec<RouteMessageTo> = vec![];
        self.channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                for (vardiff_key, vardiff_state) in channel_manager_data.vardiff.iter_mut() {
                    let channel_id = &vardiff_key.channel_id;
                    let downstream_id = &vardiff_key.downstream_id;

                    let Some(downstream) = channel_manager_data.downstream.get_mut(downstream_id)
                    else {
                        continue;
                    };
                    downstream.downstream_data.super_safe_lock(|data| {
                        if let Some(standard_channel) = data.standard_channels.get_mut(channel_id) {
                            Self::run_vardiff_on_standard_channel(
                                *downstream_id,
                                *channel_id,
                                standard_channel,
                                vardiff_state,
                                &mut messages,
                            );
                        }
                        if let Some(extended_channel) = data.extended_channels.get_mut(channel_id) {
                            Self::run_vardiff_on_extended_channel(
                                *downstream_id,
                                *channel_id,
                                extended_channel,
                                vardiff_state,
                                &mut messages,
                            );
                        }
                    });
                }

                if !messages.is_empty() {
                    let mut downstream_hashrate = 0.0;
                    let mut min_target = [0xff; 32];

                    for (_, downstream) in channel_manager_data.downstream.iter() {
                        downstream.downstream_data.super_safe_lock(|data| {
                            let mut update_from_channel = |hashrate: f32, target: &Target| {
                                downstream_hashrate += hashrate;
                                min_target = std::cmp::min(target.to_le_bytes(), min_target);
                            };

                            for (_, channel) in data.standard_channels.iter() {
                                update_from_channel(
                                    channel.get_nominal_hashrate(),
                                    channel.get_target(),
                                );
                            }

                            for (_, channel) in data.extended_channels.iter() {
                                update_from_channel(
                                    channel.get_nominal_hashrate(),
                                    channel.get_target(),
                                );
                            }
                        });
                    }

                    if let Some(ref upstream_channel) = channel_manager_data.upstream_channel {
                        debug!(
                            "Checking upstream channel {} with hashrate {} and target {:?}",
                            upstream_channel.get_channel_id(),
                            upstream_channel.get_nominal_hashrate(),
                            upstream_channel.get_target()
                        );

                        info!("Sending update channel message upstream");
                        messages.push(
                            Mining::UpdateChannel(UpdateChannel {
                                channel_id: upstream_channel.get_channel_id(),
                                nominal_hash_rate: downstream_hashrate,
                                maximum_target: min_target.into(),
                            })
                            .into(),
                        )
                    }
                }
            });

        for message in messages {
            let _ = message.forward(&self.channel_manager_channel).await;
        }

        info!("Vardiff update cycle complete");
        Ok(())
    }

    /// Sends a CoinbaseOutputConstraints message to the template provider.
    ///
    /// # Purpose
    /// - Calculates the max coinbase output size and sigops for the coinbase outputs.
    /// - Sends the CoinbaseOutputConstraints message to the template provider.
    ///
    /// # Parameters
    /// - `coinbase_outputs`: The coinbase outputs to calculate the max coinbase output size and
    ///   sigops for.
    pub async fn coinbase_output_constraints(
        &self,
        coinbase_outputs: Vec<TxOut>,
    ) -> JDCResult<(), error::ChannelManager> {
        let msg = coinbase_output_constraints_message(coinbase_outputs);

        self.channel_manager_channel
            .tp_sender
            .send(TemplateDistribution::CoinbaseOutputConstraints(msg))
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to send CoinbaseOutputConstraints message to TP");
                JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
            })?;

        Ok(())
    }
}
