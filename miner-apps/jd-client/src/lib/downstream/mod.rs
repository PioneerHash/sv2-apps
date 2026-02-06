use std::{
    collections::HashMap,
    sync::{atomic::AtomicU32, Arc},
};

use async_channel::{unbounded, Receiver, Sender};
use stratum_apps::{
    custom_mutex::Mutex,
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        channels_sv2::server::{
            extended::ExtendedChannel,
            group::GroupChannel,
            jobs::{extended::ExtendedJob, job_store::DefaultJobStore, standard::StandardJob},
            standard::StandardChannel,
        },
        common_messages_sv2::MESSAGE_TYPE_SETUP_CONNECTION,
        handlers_sv2::{HandleCommonMessagesFromClientAsync, HandleExtensionsFromClientAsync},
        parsers_sv2::{parse_message_frame_with_tlvs, AnyMessage, Mining, Tlv},
    },
    task_manager::TaskManager,
    utils::types::{DownstreamId, Message, Sv2Frame},
};

use tokio::sync::broadcast;
use tracing::{debug, error, warn};

use crate::{
    ehash_mint::RegisterChannelPubkeyData,
    error::{self, JDCError, JDCErrorKind, JDCResult},
    io_task::spawn_io_tasks,
    status::{handle_error, Status, StatusSender},
    utils::ShutdownMessage,
};

use stratum_apps::utils::types::ChannelId;

mod common_message_handler;
mod extensions_message_handler;

/// Holds state related to a downstream connection's mining channels.
///
/// This includes:
/// - Whether the downstream requires a standard job (`require_std_job`).
/// - An optional [`GroupChannel`] if group channeling is used.
/// - Active [`ExtendedChannel`]s keyed by channel ID.
/// - Active [`StandardChannel`]s keyed by channel ID.
pub struct DownstreamData {
    pub require_std_job: bool,
    pub group_channel: GroupChannel<'static, DefaultJobStore<ExtendedJob<'static>>>,
    pub extended_channels:
        HashMap<ChannelId, ExtendedChannel<'static, DefaultJobStore<ExtendedJob<'static>>>>,
    pub standard_channels:
        HashMap<ChannelId, StandardChannel<'static, DefaultJobStore<StandardJob<'static>>>>,
    pub channel_id_factory: AtomicU32,
    /// Extensions that have been successfully negotiated with this client
    pub negotiated_extensions: Vec<u16>,
    /// Extensions that the JDC supports
    pub supported_extensions: Vec<u16>,
    /// Extensions that the JDC requires
    pub required_extensions: Vec<u16>,
}

/// Communication layer for a downstream connection.
///
/// Provides the messaging primitives for interacting with the
/// channel manager and the downstream peer:
/// - `channel_manager_sender`: sends frames to the channel manager.
/// - `channel_manager_receiver`: receives messages from the channel manager.
/// - `downstream_sender`: sends frames to the downstream.
/// - `downstream_receiver`: receives frames from the downstream.
#[derive(Clone)]
pub struct DownstreamChannel {
    channel_manager_sender: Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
    channel_manager_receiver: broadcast::Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
    downstream_sender: Sender<Sv2Frame>,
    downstream_receiver: Receiver<Sv2Frame>,
    /// Sender for ehash extension messages (RegisterChannelPubkey) to channel manager.
    ehash_sender: Sender<RegisterChannelPubkeyData>,
}

/// Represents a downstream client connected to this node.
#[derive(Clone)]
pub struct Downstream {
    pub downstream_data: Arc<Mutex<DownstreamData>>,
    downstream_channel: DownstreamChannel,
    pub downstream_id: DownstreamId,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    /// Creates a new [`Downstream`] instance and spawns the necessary I/O tasks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_id: DownstreamId,
        channel_id_factory: AtomicU32,
        group_channel: GroupChannel<'static, DefaultJobStore<ExtendedJob<'static>>>,
        channel_manager_sender: Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        channel_manager_receiver: broadcast::Sender<(
            DownstreamId,
            Mining<'static>,
            Option<Vec<Tlv>>,
        )>,
        ehash_sender: Sender<RegisterChannelPubkeyData>,
        noise_stream: NoiseTcpStream<Message>,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        task_manager: Arc<TaskManager>,
        status_sender: Sender<Status>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> Self {
        let (noise_stream_reader, noise_stream_writer) = noise_stream.into_split();
        let status_sender = StatusSender::Downstream {
            downstream_id,
            tx: status_sender,
        };
        let (inbound_tx, inbound_rx) = unbounded::<Sv2Frame>();
        let (outbound_tx, outbound_rx) = unbounded::<Sv2Frame>();
        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            notify_shutdown,
            status_sender,
        );

        let downstream_channel = DownstreamChannel {
            channel_manager_receiver,
            channel_manager_sender,
            downstream_sender: outbound_tx,
            downstream_receiver: inbound_rx,
            ehash_sender,
        };

        let downstream_data = Arc::new(Mutex::new(DownstreamData {
            require_std_job: false,
            extended_channels: HashMap::new(),
            standard_channels: HashMap::new(),
            group_channel,
            channel_id_factory,
            negotiated_extensions: vec![],
            supported_extensions,
            required_extensions,
        }));

        Downstream {
            downstream_channel,
            downstream_data,
            downstream_id,
        }
    }

    /// Starts the downstream loop.
    ///
    /// Responsibilities:
    /// - Performs the initial `SetupConnection` handshake with the downstream.
    /// - Forwards mining-related messages to the channel manager.
    /// - Forwards channel manager messages back to the downstream peer.
    pub async fn start(
        mut self,
        notify_shutdown: broadcast::Sender<ShutdownMessage>,
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
    ) {
        let status_sender = StatusSender::Downstream {
            downstream_id: self.downstream_id,
            tx: status_sender,
        };

        let mut shutdown_rx = notify_shutdown.subscribe();

        // Setup initial connection
        if let Err(e) = self.setup_connection_with_downstream().await {
            error!(?e, "Failed to set up downstream connection");

            // sleep to make sure SetupConnectionError is sent
            // before we break the TCP connection
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            handle_error(&status_sender, e).await;
            return;
        }

        let mut receiver = self.downstream_channel.channel_manager_receiver.subscribe();
        task_manager.spawn(async move {
            loop {
                let self_clone_1 = self.clone();
                let downstream_id = self_clone_1.downstream_id;
                let self_clone_2 = self.clone();
                tokio::select! {
                    message = shutdown_rx.recv() => {
                        match message {
                            Ok(ShutdownMessage::ShutdownAll) => {
                                debug!("Downstream {downstream_id}: Received global shutdown");
                                break;
                            }
                            Ok(ShutdownMessage::DownstreamShutdown(id)) if downstream_id == id => {
                                debug!("Downstream {downstream_id}: Received downstream {id} shutdown");
                                break;
                            }
                            Ok(ShutdownMessage::JobDeclaratorShutdownFallback(_))  => {
                                debug!("Downstream {downstream_id}: Received job declaratorShutdown shutdown");
                                break;
                            }
                            Ok(ShutdownMessage::UpstreamShutdownFallback(_))  => {
                                debug!("Downstream {downstream_id}: Received job Upstream shutdown");
                                break;
                            }
                            _ => {}
                        }
                    }
                    res = self_clone_1.handle_downstream_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling downstream message for {downstream_id}");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_channel_manager_message(&mut receiver) => {
                        if let Err(e) = res {
                            error!(?e, "Error handling channel manager message for {downstream_id}");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }

                }
            }
            warn!("Downstream: unified message loop exited.");
        });
    }

    // Performs the initial handshake with a downstream peer.
    async fn setup_connection_with_downstream(&mut self) -> JDCResult<(), error::Downstream> {
        let mut frame = self
            .downstream_channel
            .downstream_receiver
            .recv()
            .await
            .map_err(|error| JDCError::disconnect(error, self.downstream_id))?;
        let header = frame.get_header().expect("frame header must be present");
        if header.msg_type() == MESSAGE_TYPE_SETUP_CONNECTION {
            self.handle_common_message_frame_from_client(None, header, frame.payload())
                .await?;
            return Ok(());
        }
        Err(JDCError::disconnect(
            JDCErrorKind::UnexpectedMessage(header.ext_type(), header.msg_type()),
            self.downstream_id,
        ))
    }

    // Handles messages sent from the channel manager to this downstream.
    async fn handle_channel_manager_message(
        self,
        receiver: &mut broadcast::Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
    ) -> JDCResult<(), error::Downstream> {
        let (downstream_id, message, _tlv_fields) = match receiver.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                warn!(?e, "Broadcast receive failed");
                return Err(JDCError::shutdown(
                    JDCErrorKind::BroadcastChannelErrorReceiver(e),
                ));
            }
        };

        if downstream_id != self.downstream_id {
            debug!(
                ?downstream_id,
                "Message ignored for non-matching downstream"
            );
            return Ok(());
        }

        let message = AnyMessage::Mining(message);
        let sv2_frame: Sv2Frame = message.try_into().map_err(JDCError::shutdown)?;

        self.downstream_channel
            .downstream_sender
            .send(sv2_frame)
            .await
            .map_err(|e| {
                error!(?e, "Downstream send failed");
                JDCError::disconnect(JDCErrorKind::ChannelErrorSender, self.downstream_id)
            })?;

        Ok(())
    }

    // Handles incoming messages from the downstream peer.
    async fn handle_downstream_message(mut self) -> JDCResult<(), error::Downstream> {
        let mut sv2_frame = self
            .downstream_channel
            .downstream_receiver
            .recv()
            .await
            .map_err(|error| JDCError::disconnect(error, self.downstream_id))?;
        let header = sv2_frame
            .get_header()
            .expect("frame header must be present");
        let payload = sv2_frame.payload();

        // Check for ehash extension messages before standard parsing
        // ehash messages have extension_type = 0x0100 (EHASH_EXTENSION_TYPE)
        let extension_type = header.ext_type_without_channel_msg();
        if extension_type == ehash_sv2::EHASH_EXTENSION_TYPE {
            return self.handle_ehash_message(header, payload).await;
        }

        let negotiated_extensions = self
            .downstream_data
            .super_safe_lock(|data| data.negotiated_extensions.clone());
        let (any_message, tlv_fields) =
            parse_message_frame_with_tlvs(header, payload, &negotiated_extensions)
                .map_err(|error| JDCError::disconnect(error, self.downstream_id))?;
        match any_message {
            AnyMessage::Mining(message) => {
                self.downstream_channel
                    .channel_manager_sender
                    .send((self.downstream_id, message, tlv_fields))
                    .await
                    .map_err(|e| {
                        error!(?e, "Failed to send mining message to channel manager.");
                        JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                    })?;
            }
            AnyMessage::Extensions(message) => {
                self.handle_extensions_message_from_client(None, message, tlv_fields.as_deref())
                    .await?;
            }
            _ => {
                warn!(
                    "Received unsupported message type from downstream: {}",
                    header.msg_type()
                );
                return Ok(());
            }
        }
        Ok(())
    }

    /// Handle ehash extension messages (extension_type = 0x0100).
    ///
    /// Currently handles:
    /// - RegisterChannelPubkey (0x02): Register hpub for a channel after mining.authorize
    async fn handle_ehash_message(
        &mut self,
        header: stratum_apps::stratum_core::framing_sv2::header::Header,
        payload: &mut [u8],
    ) -> JDCResult<(), error::Downstream> {
        use ehash_sv2::{EhashMessage, MSG_TYPE_REGISTER_CHANNEL_PUBKEY};

        let msg_type = header.msg_type();

        match msg_type {
            MSG_TYPE_REGISTER_CHANNEL_PUBKEY => {
                // Parse RegisterChannelPubkey message
                let ehash_msg: EhashMessage = (header, payload).try_into().map_err(|e| {
                    error!(?e, "Failed to parse RegisterChannelPubkey message");
                    JDCError::disconnect(
                        JDCErrorKind::UnexpectedMessage(ehash_sv2::EHASH_EXTENSION_TYPE, msg_type),
                        self.downstream_id,
                    )
                })?;

                if let EhashMessage::RegisterChannelPubkey(register_msg) = ehash_msg {
                    debug!(
                        downstream_id = self.downstream_id,
                        channel_id = register_msg.channel_id,
                        pubkey = %register_msg.pubkey_hex(),
                        "Received RegisterChannelPubkey"
                    );

                    // Convert to EhashPubkey
                    let pubkey = ehash_core::EhashPubkey::from_bytes(register_msg.pubkey_bytes())
                        .map_err(|e| {
                        error!(?e, "Invalid pubkey in RegisterChannelPubkey");
                        JDCError::disconnect(
                            JDCErrorKind::UnexpectedMessage(
                                ehash_sv2::EHASH_EXTENSION_TYPE,
                                msg_type,
                            ),
                            self.downstream_id,
                        )
                    })?;

                    // Send to channel manager via the ehash channel
                    let data = crate::ehash_mint::RegisterChannelPubkeyData {
                        downstream_id: self.downstream_id,
                        channel_id: register_msg.channel_id,
                        pubkey,
                    };

                    self.downstream_channel
                        .ehash_sender
                        .send(data)
                        .await
                        .map_err(|e| {
                            error!(
                                ?e,
                                "Failed to send RegisterChannelPubkey to channel manager"
                            );
                            JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
                        })?;
                } else {
                    error!("Expected RegisterChannelPubkey but got different ehash message");
                    return Err(JDCError::disconnect(
                        JDCErrorKind::UnexpectedMessage(ehash_sv2::EHASH_EXTENSION_TYPE, msg_type),
                        self.downstream_id,
                    ));
                }
            }
            _ => {
                warn!(
                    downstream_id = self.downstream_id,
                    msg_type, "Received unknown ehash message type"
                );
                // Don't disconnect for unknown messages - just ignore them
            }
        }

        Ok(())
    }
}
