use crate::{
    config::TranslatorConfig,
    error::{self, TproxyError, TproxyErrorKind, TproxyResult},
    status::{handle_error, Status, StatusSender},
    sv1::{
        downstream::{downstream::Downstream, DownstreamMessages, RegisterChannelPubkeyRequest},
        sv1_server::{
            channel::Sv1ServerChannelState, data::Sv1ServerData,
            difficulty_manager::DifficultyManager,
        },
    },
    utils::ShutdownMessage,
};
use async_channel::{Receiver, Sender};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};
use stratum_apps::{
    custom_mutex::Mutex,
    network_helpers::sv1_connection::ConnectionSV1,
    stratum_core::{
        binary_sv2::Str0255,
        bitcoin::Target,
        channels_sv2::{target::hash_rate_to_target, Vardiff, VardiffState},
        extensions_sv2::UserIdentity,
        mining_sv2::{CloseChannel, SetTarget},
        parsers_sv2::{Mining, Tlv, TlvField},
        stratum_translation::{
            sv1_to_sv2::{
                build_sv2_open_extended_mining_channel,
                build_sv2_submit_shares_extended_from_sv1_submit,
            },
            sv2_to_sv1::{build_sv1_notify_from_sv2, build_sv1_set_difficulty_from_sv2_target},
        },
        sv1_api::{utils::HexU32Be, IsServer},
    },
    task_manager::TaskManager,
    utils::types::{DownstreamId, Hashrate, SharesPerMinute},
};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc},
};
use tracing::{debug, error, info, warn};

/// SV1 server that handles connections from SV1 miners.
///
/// This struct manages the SV1 server component of the translator, which:
/// - Accepts connections from SV1 miners
/// - Manages difficulty adjustment for connected miners
/// - Coordinates with the SV2 channel manager for upstream communication
/// - Tracks mining jobs and share submissions
///
/// The server maintains state for multiple downstream connections and implements
/// variable difficulty adjustment based on share submission rates.
pub struct Sv1Server {
    sv1_server_channel_state: Sv1ServerChannelState,
    pub(crate) sv1_server_data: Arc<Mutex<Sv1ServerData>>,
    shares_per_minute: SharesPerMinute,
    listener_addr: SocketAddr,
    config: TranslatorConfig,
    clean_job: AtomicBool,
    sequence_counter: AtomicU32,
    miner_counter: AtomicU32,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1Server {
    /// Drops the server's channel state, cleaning up resources.
    pub fn drop(&self) {
        self.sv1_server_channel_state.drop();
    }

    /// Creates a new SV1 server instance.
    ///
    /// # Arguments
    /// * `listener_addr` - The socket address to bind the server to
    /// * `channel_manager_receiver` - Channel to receive messages from the channel manager
    /// * `channel_manager_sender` - Channel to send messages to the channel manager
    /// * `config` - Configuration settings for the translator
    ///
    /// # Returns
    /// A new Sv1Server instance ready to accept connections
    pub fn new(
        listener_addr: SocketAddr,
        channel_manager_receiver: Receiver<(Mining<'static>, Option<Vec<Tlv>>)>,
        channel_manager_sender: Sender<(Mining<'static>, Option<Vec<Tlv>>)>,
        ehash_upstream_sender: Sender<stratum_apps::utils::types::Sv2Frame>,
        config: TranslatorConfig,
    ) -> Self {
        let shares_per_minute = config.downstream_difficulty_config.shares_per_minute;
        let sv1_server_channel_state = Sv1ServerChannelState::new(
            channel_manager_receiver,
            channel_manager_sender,
            ehash_upstream_sender,
        );
        let sv1_server_data = Arc::new(Mutex::new(Sv1ServerData::new(config.aggregate_channels)));
        Self {
            sv1_server_channel_state,
            sv1_server_data,
            config,
            listener_addr,
            shares_per_minute,
            clean_job: AtomicBool::new(true),
            miner_counter: AtomicU32::new(0),
            sequence_counter: AtomicU32::new(0),
        }
    }

    /// Starts the SV1 server and begins accepting connections.
    ///
    /// This method:
    /// - Binds to the configured listening address
    /// - Spawns the variable difficulty adjustment loop
    /// - Enters the main event loop to handle:
    ///   - New miner connections
    ///   - Shutdown signals
    ///   - Messages from downstream miners (submit shares)
    ///   - Messages from upstream SV2 channel manager
    ///
    /// The server will continue running until a shutdown signal is received.
    ///
    /// # Arguments
    /// * `notify_shutdown` - Broadcast channel for shutdown coordination
    /// * `shutdown_complete_tx` - Channel to signal shutdown completion
    /// * `status_sender` - Channel for sending status updates
    /// * `task_manager` - Manager for spawned async tasks
    ///
    /// # Returns
    /// * `Ok(())` - Server shut down gracefully
    /// * `Err(TproxyError)` - Server encountered an error
    pub async fn start(
        self: Arc<Self>,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        shutdown_complete_tx: mpsc::Sender<()>,
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
    ) -> TproxyResult<(), error::Sv1Server> {
        info!("Starting SV1 server on {}", self.listener_addr);
        let mut shutdown_rx_main = notify_shutdown.subscribe();

        // get the first target for the first set difficulty message
        let first_target: Target = hash_rate_to_target(
            self.config
                .downstream_difficulty_config
                .min_individual_miner_hashrate as f64,
            self.config.downstream_difficulty_config.shares_per_minute as f64,
        )
        .unwrap();

        let vardiff_future = DifficultyManager::spawn_vardiff_loop(
            self.sv1_server_data.clone(),
            self.sv1_server_channel_state.channel_manager_sender.clone(),
            self.sv1_server_channel_state
                .sv1_server_to_downstream_sender
                .clone(),
            self.shares_per_minute,
            self.config.aggregate_channels,
            self.config.downstream_difficulty_config.enable_vardiff,
        );

        let keepalive_future = Self::spawn_job_keepalive_loop(Arc::clone(&self));

        let listener = TcpListener::bind(self.listener_addr).await.map_err(|e| {
            error!("Failed to bind to {}: {}", self.listener_addr, e);
            TproxyError::shutdown(e)
        })?;

        info!("Translator Proxy: listening on {}", self.listener_addr);

        let sv1_status_sender = StatusSender::Sv1Server(status_sender.clone());
        let task_manager_clone = task_manager.clone();
        let vardiff_enabled = self.config.downstream_difficulty_config.enable_vardiff;
        let keepalive_enabled = self
            .config
            .downstream_difficulty_config
            .job_keepalive_interval_secs
            > 0;
        task_manager_clone.spawn(async move {
            tokio::pin!(vardiff_future);
            tokio::pin!(keepalive_future);
            loop {
                tokio::select! {
                    message = shutdown_rx_main.recv() => {
                        match message {
                            Ok(ShutdownMessage::ShutdownAll) => {
                                debug!("SV1 Server: received shutdown signal. Exiting.");
                                self.sv1_server_channel_state.drop();
                                break;
                            }
                            Ok(ShutdownMessage::DownstreamShutdown(downstream_id)) => {
                                let current_downstream = self.sv1_server_data.super_safe_lock(|d| {
                                    // Only remove from vardiff map if vardiff is enabled
                                    if self.config.downstream_difficulty_config.enable_vardiff {
                                        d.vardiff.remove(&downstream_id);
                                    }
                                    d.downstreams.remove(&downstream_id)
                                });
                                if let Some(downstream) = current_downstream {
                                    info!("🔌 Downstream: {downstream_id} disconnected and removed from sv1 server downstreams");
                                    // In aggregated mode, send UpdateChannel to reflect the new state (only if vardiff enabled)
                                    if self.config.downstream_difficulty_config.enable_vardiff {
                                        DifficultyManager::send_update_channel_on_downstream_state_change(
                                            &self.sv1_server_data,
                                            &self.sv1_server_channel_state.channel_manager_sender,
                                            self.config.aggregate_channels,
                                        ).await;
                                    }

                                    let channel_id = downstream.downstream_data.super_safe_lock(|d| d.channel_id);
                                    if let Some(channel_id) = channel_id {
                                        if !self.config.aggregate_channels {
                                            info!("Sending CloseChannel message: {channel_id} for downstream: {downstream_id}");
                                            let reason_code =  Str0255::try_from("downstream disconnected".to_string()).unwrap();
                                            _ = self.sv1_server_channel_state
                                                .channel_manager_sender
                                                .send((Mining::CloseChannel(CloseChannel {
                                                    channel_id,
                                                    reason_code,
                                                }), None))
                                                .await;
                                        }
                                    }
                                }
                            }
                            Ok(ShutdownMessage::UpstreamFallback {tx}) => {
                                self.sv1_server_data.super_safe_lock(|d|{
                                    if self.config.downstream_difficulty_config.enable_vardiff {
                                        d.vardiff = HashMap::new();
                                    }
                                    d.downstreams = HashMap::new();
                                });
                                info!("Fallback in processing stopping sv1 server");
                                drop(tx);
                                break;
                            }
                            _ => {}
                        }
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                info!("New SV1 downstream connection from {}", addr);
                                let connection = ConnectionSV1::new(stream).await;
                                let downstream_id = self.sv1_server_data.super_safe_lock(|v| v.downstream_id_factory.fetch_add(1, Ordering::Relaxed));
                                let downstream = Arc::new(Downstream::new(
                                    downstream_id,
                                    connection.sender().clone(),
                                    connection.receiver().clone(),
                                    self.sv1_server_channel_state.downstream_to_sv1_server_sender.clone(),
                                    self.sv1_server_channel_state.sv1_server_to_downstream_sender.clone().subscribe(),
                                    first_target,
                                    Some(self.config.downstream_difficulty_config.min_individual_miner_hashrate),
                                    self.sv1_server_data.clone(),
                                ));
                                // vardiff initialization (only if enabled)
                                _ = self.sv1_server_data
                                    .safe_lock(|d| {
                                        d.downstreams.insert(downstream_id, downstream.clone());
                                        // Insert vardiff state for this downstream only if vardiff is enabled
                                        if self.config.downstream_difficulty_config.enable_vardiff {
                                            let vardiff = Arc::new(RwLock::new(VardiffState::new().expect("Failed to create vardiffstate")));
                                            d.vardiff.insert(downstream_id, vardiff);
                                        }
                                    });
                                info!("Downstream {} registered successfully (channel will be opened after first message)", downstream_id);
                                // Start downstream tasks immediately, but defer channel opening until first message
                                let status_sender = StatusSender::Downstream {
                                    downstream_id,
                                    tx: status_sender.clone(),
                                };
                                Downstream::run_downstream_tasks(
                                    downstream,
                                    notify_shutdown.clone(),
                                    shutdown_complete_tx.clone(),
                                    status_sender,
                                    task_manager.clone(),
                                );
                            }
                            Err(e) => {
                                warn!("Failed to accept new connection: {:?}", e);
                            }
                        }
                    }
                    res = Self::handle_downstream_message(
                        Arc::clone(&self)
                    ) => {
                        if let Err(e) = res {
                            if handle_error(&sv1_status_sender, e).await {
                                self.sv1_server_channel_state.drop();
                                break;
                            }
                        }
                    }
                    res = Self::handle_upstream_message(
                        Arc::clone(&self),
                        first_target,
                    ) => {
                        if let Err(e) = res {
                            if handle_error(&sv1_status_sender, e).await {
                                self.sv1_server_channel_state.drop();
                                break;
                            }
                        }
                    }
                    _ = &mut vardiff_future, if vardiff_enabled => {}
                    _ = &mut keepalive_future, if keepalive_enabled => {}
                }
            }
            drop(shutdown_complete_tx);
            debug!("SV1 Server main listener loop exited.");
        });

        Ok(())
    }

    /// Handles messages received from downstream SV1 miners.
    ///
    /// This method processes share submissions from miners by:
    /// - Updating variable difficulty counters
    /// - Extracting and validating share data
    /// - Converting SV1 share format to SV2 SubmitSharesExtended
    /// - Forwarding the share to the channel manager for upstream submission
    ///
    /// # Returns
    /// * `Ok(())` - Message processed successfully
    /// * `Err(TproxyError)` - Error processing the message
    pub async fn handle_downstream_message(self: Arc<Self>) -> TproxyResult<(), error::Sv1Server> {
        let downstream_message = self
            .sv1_server_channel_state
            .downstream_to_sv1_server_receiver
            .recv()
            .await
            .map_err(TproxyError::shutdown)?;

        match downstream_message {
            DownstreamMessages::SubmitShares(message) => {
                return self.handle_submit_shares(message).await;
            }
            DownstreamMessages::OpenChannel(downstream_id) => {
                return self.handle_open_channel_request(downstream_id).await;
            }
            DownstreamMessages::RegisterChannelPubkey(request) => {
                return self.handle_register_channel_pubkey(request).await;
            }
        }
    }

    /// Handles share submission messages from downstream.
    async fn handle_submit_shares(
        self: &Arc<Self>,
        message: crate::sv1::downstream::SubmitShareWithChannelId,
    ) -> TproxyResult<(), error::Sv1Server> {
        // Increment vardiff counter for this downstream (only if vardiff is enabled)
        if self.config.downstream_difficulty_config.enable_vardiff {
            self.sv1_server_data
                .safe_lock(|v| {
                    if let Some(vardiff_state) = v.vardiff.get(&message.downstream_id) {
                        vardiff_state
                            .write()
                            .unwrap()
                            .increment_shares_since_last_update();
                    }
                })
                .map_err(TproxyError::shutdown)?;
        }

        let job_version = match message.job_version {
            Some(version) => version,
            None => {
                warn!("Received share submission without valid job version, skipping");
                return Ok(());
            }
        };

        // If this is a keepalive job, extract the original upstream job_id from the job_id string
        let mut share = message.share;
        let job_id_str = share.job_id.clone();
        if Sv1ServerData::is_keepalive_job_id(&job_id_str) {
            if let Some(original_job_id) = Sv1ServerData::extract_original_job_id(&job_id_str) {
                debug!(
                    "Extracting original job_id {} from keepalive job_id {}",
                    original_job_id, job_id_str
                );
                share.job_id = original_job_id;
            } else {
                warn!(
                    "Failed to extract original job_id from keepalive job_id {}, rejecting share",
                    job_id_str
                );
                return Ok(());
            }
        }

        let submit_share_extended = build_sv2_submit_shares_extended_from_sv1_submit(
            &share,
            message.channel_id,
            self.sequence_counter.load(Ordering::SeqCst),
            job_version,
            message.version_rolling_mask,
        )
        .map_err(|_| TproxyError::shutdown(TproxyErrorKind::SV1Error))?;

        // Only add TLV fields with user identity in non-aggregated mode
        let tlv_fields = if !self.config.aggregate_channels {
            let user_identity_string = self.sv1_server_data.super_safe_lock(|d| {
                d.downstreams
                    .get(&message.downstream_id)
                    .unwrap()
                    .downstream_data
                    .super_safe_lock(|d| d.user_identity.clone())
            });
            // UserIdentity is limited to 32 bytes. If it's longer (e.g. hpub), skip the TLV.
            // For ehash, the miner's hpub is sent via RegisterChannelPubkey instead.
            UserIdentity::new(&user_identity_string)
                .ok()
                .and_then(|ui| ui.to_tlv().ok())
                .map(|tlv| vec![tlv])
        } else {
            None
        };

        self.sv1_server_channel_state
            .channel_manager_sender
            .send((
                Mining::SubmitSharesExtended(submit_share_extended),
                tlv_fields,
            ))
            .await
            .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;

        self.sequence_counter.fetch_add(1, Ordering::SeqCst);

        Ok(())
    }

    /// Handles channel opening requests from downstream when they send their first message.
    async fn handle_open_channel_request(
        self: &Arc<Self>,
        downstream_id: DownstreamId,
    ) -> TproxyResult<(), error::Sv1Server> {
        info!("SV1 Server: Opening extended mining channel for downstream {} after receiving first message", downstream_id);

        let (request_id, downstreams) = self.sv1_server_data.super_safe_lock(|v| {
            let request_id = v.request_id_factory.fetch_add(1, Ordering::Relaxed);
            v.request_id_to_downstream_id
                .insert(request_id, downstream_id);
            (request_id, v.downstreams.clone())
        });
        if let Some(downstream) = Self::get_downstream(downstream_id, downstreams) {
            self.open_extended_mining_channel(request_id, downstream)
                .await?;
        } else {
            error!(
                "Downstream {} not found when trying to open channel",
                downstream_id
            );
        }

        Ok(())
    }

    /// Handles RegisterChannelPubkey messages from downstream.
    ///
    /// This sends a RegisterChannelPubkey ehash extension message to the upstream JDC
    /// to associate the miner's hpub with their SV2 channel for ehash token issuance.
    async fn handle_register_channel_pubkey(
        self: &Arc<Self>,
        request: RegisterChannelPubkeyRequest,
    ) -> TproxyResult<(), error::Sv1Server> {
        use ehash_sv2::{EhashMessage, RegisterChannelPubkey};
        use stratum_apps::stratum_core::{
            buffer_sv2::Slice, framing_sv2::framing::Sv2Frame, parsers_sv2::IsSv2Message,
        };

        info!(
            "SV1 Server: Sending RegisterChannelPubkey for channel_id={}, pubkey={}",
            request.channel_id,
            request.pubkey.to_bech32()
        );

        // Convert EhashPubkey to CompressedPubKey
        let pubkey_bytes = request.pubkey.as_bytes().to_vec();
        let pubkey: ehash_sv2::binary_sv2::CompressedPubKey = pubkey_bytes
            .try_into()
            .map_err(|_| TproxyError::shutdown(TproxyErrorKind::SV1Error))?;

        // Create the RegisterChannelPubkey message
        let register_msg = RegisterChannelPubkey::new(request.channel_id, pubkey);
        let ehash_msg: EhashMessage<'static> = register_msg.into();

        // Create Sv2 frame
        let message_type = ehash_msg.message_type();
        let extension_type = ehash_msg.extension_type();
        let channel_bit = ehash_msg.channel_bit();

        let frame: Sv2Frame<EhashMessage<'static>, Slice> =
            Sv2Frame::from_message(ehash_msg, message_type, extension_type, channel_bit)
                .ok_or_else(|| TproxyError::shutdown(TproxyErrorKind::SV1Error))?;

        // Get frame size and serialize
        let frame_size = frame.encoded_length();
        let mut frame_bytes = vec![0u8; frame_size];
        frame
            .serialize(&mut frame_bytes)
            .map_err(|_| TproxyError::shutdown(TproxyErrorKind::SV1Error))?;

        // Convert to the upstream frame type and send
        let upstream_frame =
            stratum_apps::utils::types::Sv2Frame::from_bytes_unchecked(frame_bytes.into());

        self.sv1_server_channel_state
            .ehash_upstream_sender
            .send(upstream_frame)
            .await
            .map_err(|e| {
                error!("Failed to send RegisterChannelPubkey to upstream: {:?}", e);
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })?;

        info!(
            "RegisterChannelPubkey sent successfully for channel_id={}",
            request.channel_id
        );
        Ok(())
    }

    /// Handles messages received from the upstream SV2 server via the channel manager.
    ///
    /// This method processes various SV2 messages including:
    /// - OpenExtendedMiningChannelSuccess: Sets up downstream connections
    /// - NewExtendedMiningJob: Converts to SV1 notify messages
    /// - SetNewPrevHash: Updates block template information
    /// - Channel error messages (TODO: implement proper handling)
    ///
    /// # Arguments
    /// * `first_target` - Initial difficulty target for new connections
    /// * `notify_shutdown` - Broadcast channel for shutdown coordination
    /// * `shutdown_complete_tx` - Channel to signal shutdown completion
    /// * `status_sender` - Channel for sending status updates
    /// * `task_manager` - Manager for spawned async tasks
    ///
    /// # Returns
    /// * `Ok(())` - Message processed successfully
    /// * `Err(TproxyError)` - Error processing the message
    pub async fn handle_upstream_message(
        self: Arc<Self>,
        first_target: Target,
    ) -> TproxyResult<(), error::Sv1Server> {
        let (message, _tlv_fields) = self
            .sv1_server_channel_state
            .channel_manager_receiver
            .recv()
            .await
            .map_err(TproxyError::shutdown)?;

        match message {
            Mining::OpenExtendedMiningChannelSuccess(m) => {
                debug!(
                    "Received OpenExtendedMiningChannelSuccess for channel id: {}",
                    m.channel_id
                );
                let (downstream_id, downstreams) = self.sv1_server_data.super_safe_lock(|v| {
                    let downstream_id = v.request_id_to_downstream_id.remove(&m.request_id);
                    (downstream_id, v.downstreams.clone())
                });
                let Some(downstream_id) = downstream_id else {
                    return Err(TproxyError::log(TproxyErrorKind::DownstreamNotFound(
                        m.request_id,
                    )));
                };
                if let Some(downstream) = Self::get_downstream(downstream_id, downstreams) {
                    let initial_target =
                        Target::from_le_bytes(m.target.inner_as_ref().try_into().unwrap());
                    downstream
                        .downstream_data
                        .safe_lock(|d| {
                            d.extranonce1 = m.extranonce_prefix.to_vec();
                            d.extranonce2_len = m.extranonce_size.into();
                            d.channel_id = Some(m.channel_id);
                            // Set the initial upstream target from OpenExtendedMiningChannelSuccess
                            d.set_upstream_target(initial_target);
                        })
                        .map_err(TproxyError::shutdown)?;

                    // Process all queued messages now that channel is established
                    if let Ok(queued_messages) = downstream.downstream_data.safe_lock(|d| {
                        let messages = d.queued_sv1_handshake_messages.clone();
                        d.queued_sv1_handshake_messages.clear();
                        messages
                    }) {
                        if !queued_messages.is_empty() {
                            info!(
                                "Processing {} queued Sv1 messages for downstream {}",
                                queued_messages.len(),
                                downstream_id
                            );

                            // Set flag to indicate we're processing queued responses
                            downstream.downstream_data.super_safe_lock(|data| {
                                data.processing_queued_sv1_handshake_responses
                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                            });

                            for message in queued_messages {
                                if let Ok(Some(response_msg)) = downstream
                                    .downstream_data
                                    .super_safe_lock(|data| data.handle_message(None, message))
                                {
                                    self.sv1_server_channel_state
                                        .sv1_server_to_downstream_sender
                                        .send((
                                            m.channel_id,
                                            Some(downstream_id),
                                            response_msg.into(),
                                        ))
                                        .map_err(|_| {
                                            TproxyError::shutdown(
                                                TproxyErrorKind::ChannelErrorSender,
                                            )
                                        })?;
                                }
                            }
                        }
                    }

                    let set_difficulty = build_sv1_set_difficulty_from_sv2_target(first_target)
                        .map_err(|_| {
                            TproxyError::shutdown(TproxyErrorKind::General(
                                "Failed to generate set_difficulty".into(),
                            ))
                        })?;
                    // send the set_difficulty message to the downstream
                    self.sv1_server_channel_state
                        .sv1_server_to_downstream_sender
                        .send((m.channel_id, None, set_difficulty))
                        .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;
                } else {
                    error!("Downstream not found for downstream_id: {}", downstream_id);
                }
            }

            Mining::NewExtendedMiningJob(m) => {
                debug!(
                    "Received NewExtendedMiningJob for channel id: {}",
                    m.channel_id
                );
                if let Some(prevhash) = self.sv1_server_data.super_safe_lock(|v| v.prevhash.clone())
                {
                    let notify = build_sv1_notify_from_sv2(
                        prevhash,
                        m.clone().into_static(),
                        self.clean_job.load(Ordering::SeqCst),
                    )
                    .map_err(TproxyError::shutdown)?;
                    let clean_jobs = self.clean_job.load(Ordering::SeqCst);
                    self.clean_job.store(false, Ordering::SeqCst);

                    // Update job storage based on the configured mode
                    let notify_parsed = notify.clone();
                    self.sv1_server_data.super_safe_lock(|server_data| {
                        if let Some(ref mut aggregated_jobs) = server_data.aggregated_valid_jobs {
                            // Aggregated mode: all downstreams share the same jobs
                            if clean_jobs {
                                aggregated_jobs.clear();
                            }
                            aggregated_jobs.push(notify_parsed);
                        } else if let Some(ref mut non_aggregated_jobs) =
                            server_data.non_aggregated_valid_jobs
                        {
                            // Non-aggregated mode: per-downstream jobs
                            let channel_jobs = non_aggregated_jobs
                                .entry(m.channel_id)
                                .or_insert_with(Vec::new);
                            if clean_jobs {
                                channel_jobs.clear();
                            }
                            channel_jobs.push(notify_parsed);
                        }
                    });

                    let _ = self
                        .sv1_server_channel_state
                        .sv1_server_to_downstream_sender
                        .send((m.channel_id, None, notify.into()));
                }
            }

            Mining::SetNewPrevHash(m) => {
                debug!("Received SetNewPrevHash for channel id: {}", m.channel_id);
                self.clean_job.store(true, Ordering::SeqCst);
                self.sv1_server_data
                    .super_safe_lock(|v| v.prevhash = Some(m.clone().into_static()));
            }

            Mining::SetTarget(m) => {
                debug!("Received SetTarget for channel id: {}", m.channel_id);
                if self.config.downstream_difficulty_config.enable_vardiff {
                    // Vardiff enabled - use full difficulty management
                    DifficultyManager::handle_set_target_message(
                        m,
                        &self.sv1_server_data,
                        &self.sv1_server_channel_state.channel_manager_sender,
                        &self
                            .sv1_server_channel_state
                            .sv1_server_to_downstream_sender,
                        self.config.aggregate_channels,
                    )
                    .await;
                } else {
                    // Vardiff disabled - just forward the difficulty to downstreams
                    debug!("Vardiff disabled - forwarding SetTarget to downstreams");
                    self.handle_set_target_without_vardiff(m).await;
                }
            }
            // Guaranteed unreachable: the channel manager only forwards valid,
            // pre-filtered messages, so no other variants can arrive here.
            _ => unreachable!("Invalid message: should have been filtered earlier"),
        }

        Ok(())
    }

    /// Opens an extended mining channel for a downstream connection.
    ///
    /// This method initiates the SV2 channel setup process by:
    /// - Calculating the initial target based on configuration
    /// - Generating a unique user identity for the miner
    /// - Creating an OpenExtendedMiningChannel message
    /// - Sending the request to the channel manager
    ///
    /// # Arguments
    /// * `downstream` - The downstream connection to set up a channel for
    ///
    /// # Returns
    /// * `Ok(())` - Channel setup request sent successfully
    /// * `Err(TproxyError)` - Error setting up the channel
    pub async fn open_extended_mining_channel(
        &self,
        request_id: u32,
        downstream: Arc<Downstream>,
    ) -> TproxyResult<(), error::Sv1Server> {
        let config = &self.config.downstream_difficulty_config;

        let hashrate = config.min_individual_miner_hashrate as f64;
        let shares_per_min = config.shares_per_minute as f64;
        let min_extranonce_size = self.config.downstream_extranonce2_size;
        let vardiff_enabled = config.enable_vardiff;

        let max_target = if vardiff_enabled {
            hash_rate_to_target(hashrate, shares_per_min).unwrap()
        } else {
            // If translator doesn't manage vardiff, we rely on upstream to do that,
            // so we give it more freedom by setting max_target to maximum possible value
            Target::from_le_bytes([0xff; 32])
        };

        // Store the initial target for use when no downstreams remain
        self.sv1_server_data.super_safe_lock(|data| {
            if data.initial_target.is_none() {
                data.initial_target = Some(max_target);
            }
        });

        let miner_id = self.miner_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let user_identity = format!("{}.miner{}", self.config.user_identity, miner_id);

        downstream
            .downstream_data
            .safe_lock(|d| d.user_identity = user_identity.clone())
            .map_err(TproxyError::shutdown)?;

        if let Ok(open_channel_msg) = build_sv2_open_extended_mining_channel(
            request_id,
            user_identity.clone(),
            hashrate as Hashrate,
            max_target,
            min_extranonce_size,
        ) {
            self.sv1_server_channel_state
                .channel_manager_sender
                .send((Mining::OpenExtendedMiningChannel(open_channel_msg), None))
                .await
                .map_err(|_| TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender))?;
        } else {
            error!("Failed to build OpenExtendedMiningChannel message");
        }

        Ok(())
    }

    /// Retrieves a downstream connection by ID from the provided map.
    ///
    /// # Arguments
    /// * `downstream_id` - The ID of the downstream connection to find
    /// * `downstream` - HashMap containing downstream connections
    ///
    /// # Returns
    /// * `Some(Downstream)` - If a downstream with the given ID exists
    /// * `None` - If no downstream with the given ID is found
    pub fn get_downstream(
        downstream_id: DownstreamId,
        downstream: HashMap<DownstreamId, Arc<Downstream>>,
    ) -> Option<Arc<Downstream>> {
        downstream.get(&downstream_id).cloned()
    }

    /// Extracts the downstream ID from a Downstream instance.
    ///
    /// # Arguments
    /// * `downstream` - The downstream connection to get the ID from
    ///
    /// # Returns
    /// The downstream ID as a u32
    pub fn get_downstream_id(downstream: Downstream) -> DownstreamId {
        downstream
            .downstream_data
            .super_safe_lock(|s| s.downstream_id)
    }

    /// Handles SetTarget messages when vardiff is disabled.
    ///
    /// This method forwards difficulty changes from upstream directly to downstream miners
    /// without any variable difficulty logic. It respects the aggregated/non-aggregated
    /// channel configuration.
    async fn handle_set_target_without_vardiff(&self, set_target: SetTarget<'_>) {
        let new_target =
            Target::from_le_bytes(set_target.maximum_target.inner_as_ref().try_into().unwrap());
        debug!(
            "Forwarding SetTarget to downstreams: channel_id={}, target={:?}",
            set_target.channel_id, new_target
        );

        if self.config.aggregate_channels {
            // Aggregated mode: send set_difficulty to ALL downstreams
            self.send_set_difficulty_to_all_downstreams(new_target)
                .await;
        } else {
            // Non-aggregated mode: send set_difficulty to specific downstream for this channel
            self.send_set_difficulty_to_specific_downstream(set_target.channel_id, new_target)
                .await;
        }
    }

    /// Sends set_difficulty to all downstreams (aggregated mode).
    /// Used only when vardiff is disabled.
    async fn send_set_difficulty_to_all_downstreams(&self, target: Target) {
        let downstreams = self
            .sv1_server_data
            .super_safe_lock(|data| data.downstreams.clone());

        for (downstream_id, downstream) in downstreams {
            let channel_id = downstream.downstream_data.super_safe_lock(|d| d.channel_id);

            if let Some(channel_id) = channel_id {
                // Update the downstream's target
                _ = downstream.downstream_data.safe_lock(|d| {
                    d.set_upstream_target(target);
                    d.set_pending_target(target);
                });

                // Send set_difficulty message
                if let Ok(set_difficulty_msg) = build_sv1_set_difficulty_from_sv2_target(target) {
                    if let Err(e) = self
                        .sv1_server_channel_state
                        .sv1_server_to_downstream_sender
                        .send((channel_id, Some(downstream_id), set_difficulty_msg))
                    {
                        error!(
                            "Failed to send SetDifficulty to downstream {}: {:?}",
                            downstream_id, e
                        );
                    } else {
                        debug!(
                            "Sent SetDifficulty to downstream {} (vardiff disabled)",
                            downstream_id
                        );
                    }
                }
            }
        }
    }

    /// Sends set_difficulty to the specific downstream associated with a channel (non-aggregated
    /// mode).
    /// Used only when vardiff is disabled.
    async fn send_set_difficulty_to_specific_downstream(&self, channel_id: u32, target: Target) {
        let affected_downstream = self.sv1_server_data.super_safe_lock(|data| {
            data.downstreams
                .iter()
                .find_map(|(downstream_id, downstream)| {
                    downstream.downstream_data.super_safe_lock(|d| {
                        if d.channel_id == Some(channel_id) {
                            Some((*downstream_id, downstream.clone()))
                        } else {
                            None
                        }
                    })
                })
        });

        if let Some((downstream_id, downstream)) = affected_downstream {
            // Update the downstream's target
            _ = downstream.downstream_data.safe_lock(|d| {
                d.set_upstream_target(target);
                d.set_pending_target(target);
            });

            // Send set_difficulty message
            if let Ok(set_difficulty_msg) = build_sv1_set_difficulty_from_sv2_target(target) {
                if let Err(e) = self
                    .sv1_server_channel_state
                    .sv1_server_to_downstream_sender
                    .send((channel_id, Some(downstream_id), set_difficulty_msg))
                {
                    error!(
                        "Failed to send SetDifficulty to downstream {}: {:?}",
                        downstream_id, e
                    );
                } else {
                    debug!(
                        "Sent SetDifficulty to downstream {} for channel {} (vardiff disabled)",
                        downstream_id, channel_id
                    );
                }
            }
        } else {
            warn!(
                "No downstream found for channel {} when vardiff disabled",
                channel_id
            );
        }
    }

    /// Spawns the job keepalive loop that sends periodic mining.notify messages.
    ///
    /// This prevents SV1 miners from timing out when there are no new jobs received from the
    /// upstream for a while.
    pub async fn spawn_job_keepalive_loop(self: Arc<Self>) {
        let keepalive_interval_secs = self
            .config
            .downstream_difficulty_config
            .job_keepalive_interval_secs;

        if keepalive_interval_secs == 0 {
            debug!("Job keepalive disabled (interval set to 0)");
            return;
        }

        let interval = Duration::from_secs(keepalive_interval_secs as u64);
        let check_interval =
            Duration::from_secs(keepalive_interval_secs as u64 / 2).max(Duration::from_secs(5));
        info!(
            "Starting job keepalive loop with interval of {} seconds",
            keepalive_interval_secs
        );

        loop {
            tokio::time::sleep(check_interval).await;

            let keepalive_targets: Vec<(DownstreamId, Option<u32>)> =
                self.sv1_server_data.super_safe_lock(|data| {
                    data.downstreams
                        .iter()
                        .filter_map(|(downstream_id, downstream)| {
                            downstream.downstream_data.super_safe_lock(|d| {
                                // Only send keepalive if:
                                // 1. Handshake is complete
                                // 2. Enough time has passed since last job
                                let handshake_complete =
                                    d.sv1_handshake_complete.load(Ordering::SeqCst);

                                if !handshake_complete {
                                    return None;
                                }

                                let needs_keepalive = match d.last_job_received_time {
                                    Some(last_time) => last_time.elapsed() >= interval,
                                    None => false, // No job received yet, don't send keepalive
                                };

                                if needs_keepalive {
                                    Some((*downstream_id, d.channel_id))
                                } else {
                                    None
                                }
                            })
                        })
                        .collect()
                });

            // Send keepalive to each downstream that needs one
            for (downstream_id, channel_id) in keepalive_targets {
                // Get the appropriate job for this downstream's channel and create keepalive
                let keepalive_job = self.sv1_server_data.super_safe_lock(|data| {
                    if let Some(last_job) = data.get_last_job(channel_id) {
                        // Extract the original upstream job_id from the last job
                        // If it's already a keepalive job, extract its original; otherwise use
                        // as-is
                        let original_job_id =
                            Sv1ServerData::extract_original_job_id(&last_job.job_id)
                                .unwrap_or_else(|| last_job.job_id.clone());

                        // Find the original upstream job to get its base time
                        let original_job = data.get_original_job(&original_job_id, channel_id);
                        let base_time = original_job
                            .as_ref()
                            .map(|j| j.time.0)
                            .unwrap_or(last_job.time.0);

                        // Increment the time by the keepalive interval, but cap at
                        // MAX_FUTURE_BLOCK_TIME from the original job's time to maintain consensus
                        // validity (see https://github.com/bitcoin/bitcoin/blob/cd6e4c9235f763b8077cece69c2e3b2025cc8d0f/src/chain.h#L29)
                        const MAX_FUTURE_BLOCK_TIME: u32 = 2 * 60 * 60;
                        let new_time = last_job
                            .time
                            .0
                            .saturating_add(keepalive_interval_secs as u32)
                            .min(base_time.saturating_add(MAX_FUTURE_BLOCK_TIME));

                        // If we've hit the cap, don't send another keepalive for this job
                        if new_time == last_job.time.0 {
                            return None;
                        }

                        // Generate new keepalive job_id: {original_job_id}#{counter}
                        let new_job_id = data.next_keepalive_job_id(&original_job_id);

                        let mut keepalive_notify = last_job;
                        keepalive_notify.job_id = new_job_id.clone();
                        keepalive_notify.time = HexU32Be(new_time);

                        // Add the keepalive job to valid jobs so shares can be validated
                        if let Some(ref mut aggregated_jobs) = data.aggregated_valid_jobs {
                            aggregated_jobs.push(keepalive_notify.clone());
                        }
                        if let Some(ref mut non_aggregated_jobs) = data.non_aggregated_valid_jobs {
                            if let Some(ch_id) = channel_id {
                                if let Some(channel_jobs) = non_aggregated_jobs.get_mut(&ch_id) {
                                    channel_jobs.push(keepalive_notify.clone());
                                }
                            }
                        }

                        Some(keepalive_notify)
                    } else {
                        None
                    }
                });

                if let Some(notify) = keepalive_job {
                    debug!(
                        "Sending keepalive job to downstream {} with job_id: {}, time: {}",
                        downstream_id, notify.job_id, notify.time.0
                    );

                    if let Err(e) = self
                        .sv1_server_channel_state
                        .sv1_server_to_downstream_sender
                        .send((channel_id.unwrap_or(0), Some(downstream_id), notify.into()))
                    {
                        warn!(
                            "Failed to send keepalive job to downstream {}: {:?}",
                            downstream_id, e
                        );
                    } else {
                        // Update the downstream's last job received time
                        self.sv1_server_data.super_safe_lock(|data| {
                            if let Some(downstream) = data.downstreams.get(&downstream_id) {
                                downstream.downstream_data.super_safe_lock(|d| {
                                    d.last_job_received_time = Some(Instant::now());
                                });
                            }
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DownstreamDifficultyConfig, TranslatorConfig, Upstream};
    use async_channel::unbounded;
    use std::{collections::HashMap, str::FromStr};
    use stratum_apps::key_utils::Secp256k1PublicKey;

    fn create_test_config() -> TranslatorConfig {
        let pubkey_str = "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan";
        let pubkey = Secp256k1PublicKey::from_str(pubkey_str).unwrap();

        let upstream = Upstream::new("127.0.0.1".to_string(), 4444, pubkey);
        let difficulty_config = DownstreamDifficultyConfig::new(100.0, 5.0, true, 60);

        TranslatorConfig::new(
            vec![upstream],
            "0.0.0.0".to_string(), // downstream_address
            3333,                  // downstream_port
            difficulty_config,     // downstream_difficulty_config
            2,                     // max_supported_version
            1,                     // min_supported_version
            4,                     // downstream_extranonce2_size
            "test_user".to_string(),
            true,   // aggregate_channels
            vec![], // supported_extensions
            vec![], // required_extensions
        )
    }

    fn create_test_sv1_server() -> Sv1Server {
        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let (ehash_sender, _ehash_receiver) = unbounded();
        let config = create_test_config();
        let addr = "127.0.0.1:3333".parse().unwrap();

        Sv1Server::new(addr, cm_receiver, cm_sender, ehash_sender, config)
    }

    #[test]
    fn test_sv1_server_creation() {
        let server = create_test_sv1_server();

        assert_eq!(server.shares_per_minute, 5.0);
        assert_eq!(server.listener_addr.ip().to_string(), "127.0.0.1");
        assert_eq!(server.listener_addr.port(), 3333);
        assert_eq!(server.config.user_identity, "test_user");
        assert!(server.config.aggregate_channels);
    }

    #[test]
    fn test_sv1_server_aggregated_config() {
        let mut config = create_test_config();
        config.aggregate_channels = true;
        config.downstream_difficulty_config.enable_vardiff = true;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let (ehash_sender, _ehash_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();

        let server = Sv1Server::new(addr, cm_receiver, cm_sender, ehash_sender, config);

        assert!(server.config.aggregate_channels);
        assert!(server.config.downstream_difficulty_config.enable_vardiff);
    }

    #[test]
    fn test_sv1_server_non_aggregated_config() {
        let mut config = create_test_config();
        config.aggregate_channels = false;
        config.downstream_difficulty_config.enable_vardiff = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let (ehash_sender, _ehash_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();

        let server = Sv1Server::new(addr, cm_receiver, cm_sender, ehash_sender, config);

        assert!(!server.config.aggregate_channels);
        assert!(!server.config.downstream_difficulty_config.enable_vardiff);
    }

    #[test]
    fn test_get_downstream_basic() {
        let downstreams = HashMap::new();

        // Test non-existing downstream
        let not_found = Sv1Server::get_downstream(999, downstreams);
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_send_set_difficulty_to_all_downstreams_empty() {
        let server = create_test_sv1_server();
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();

        // Test with empty downstreams
        server.send_set_difficulty_to_all_downstreams(target).await;

        // Should not crash with empty downstreams
    }

    #[tokio::test]
    async fn test_send_set_difficulty_to_specific_downstream_not_found() {
        let server = create_test_sv1_server();
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();
        let channel_id = 1u32;

        // Test with no downstreams
        server
            .send_set_difficulty_to_specific_downstream(channel_id, target)
            .await;

        // Should not crash when no downstreams are found
    }

    #[tokio::test]
    async fn test_handle_set_target_without_vardiff_aggregated() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;
        config.aggregate_channels = true;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let (ehash_sender, _ehash_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();

        let server = Sv1Server::new(addr, cm_receiver, cm_sender, ehash_sender, config);
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();

        let set_target = SetTarget {
            channel_id: 1,
            maximum_target: target.to_le_bytes().into(),
        };

        // Test should not panic and should handle the message
        server.handle_set_target_without_vardiff(set_target).await;
    }

    #[tokio::test]
    async fn test_handle_set_target_without_vardiff_non_aggregated() {
        let mut config = create_test_config();
        config.downstream_difficulty_config.enable_vardiff = false;
        config.aggregate_channels = false;

        let (cm_sender, _cm_receiver) = unbounded();
        let (_downstream_sender, cm_receiver) = unbounded();
        let (ehash_sender, _ehash_receiver) = unbounded();
        let addr = "127.0.0.1:3333".parse().unwrap();

        let server = Sv1Server::new(addr, cm_receiver, cm_sender, ehash_sender, config);
        let target: Target = hash_rate_to_target(200.0, 5.0).unwrap();

        let set_target = SetTarget {
            channel_id: 1,
            maximum_target: target.to_le_bytes().into(),
        };

        // Test should not panic and should handle the message
        server.handle_set_target_without_vardiff(set_target).await;
    }

    #[test]
    fn test_sv1_server_counters() {
        let server = create_test_sv1_server();

        // Test initial values
        assert_eq!(server.miner_counter.load(Ordering::SeqCst), 0);
        assert_eq!(server.sequence_counter.load(Ordering::SeqCst), 0);

        // Test incrementing
        let miner_id = server.miner_counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(miner_id, 0);
        assert_eq!(server.miner_counter.load(Ordering::SeqCst), 1);

        let seq_id = server.sequence_counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(seq_id, 0);
        assert_eq!(server.sequence_counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_sv1_server_clean_job_flag() {
        let server = create_test_sv1_server();

        // Test initial value
        assert!(server.clean_job.load(Ordering::SeqCst));

        // Test setting to false
        server.clean_job.store(false, Ordering::SeqCst);
        assert!(!server.clean_job.load(Ordering::SeqCst));

        // Test setting back to true
        server.clean_job.store(true, Ordering::SeqCst);
        assert!(server.clean_job.load(Ordering::SeqCst));
    }
}
