//! ehash-mint client module.
//!
//! This module handles communication with the ehash-mint daemon for ecash
//! token issuance based on mining shares.
//!
//! # Design
//! - Non-blocking: Share data sent through async channel to dedicated task
//! - Fire-and-forget: No acknowledgment from mint, shares already validated by pool
//! - Dedicated task handles Noise connection and message framing
//!
//! # Message Format
//! - ShareReport (0x00): Regular share (73 bytes)
//! - BlockFoundReport (0x01): Share that found a block (73 bytes)
//! - RegisterChannelPubkey (0x02): Register hpub for a channel (37 bytes)
//! - Share reports contain: pubkey(33) + share_hash(32) + difficulty_ratio(8)
//!
//! # Structured Logging
//! Key events are logged as JSON for easy parsing in integration tests:
//! - `ehash.report.queued` - Report entered channel
//! - `ehash.report.sent` - Report sent to mint
//! - `ehash.report.buffered` - Report buffered (connection down)
//! - `ehash.connection.established` - Noise connection succeeded
//! - `ehash.connection.failed` - Connection attempt failed
//! - `ehash.fallback.written` - Report written to fallback file
//! - `ehash.pubkey.registered` - RegisterChannelPubkey processed

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_channel::{bounded, Receiver, Sender};
use ehash_core::EhashPubkey;
use ehash_sv2::{
    binary_sv2::{CompressedPubKey, U256},
    BlockFoundReport, ChainTipUpdate, EhashMessage, ShareReport,
};
use stratum_apps::{
    custom_mutex::Mutex,
    key_utils::Secp256k1PublicKey,
    network_helpers::noise_stream::NoiseTcpWriteHalf,
    stratum_core::{
        buffer_sv2::Slice, codec_sv2::HandshakeRole, framing_sv2::framing::Sv2Frame,
        noise_sv2::Initiator, parsers_sv2::IsSv2Message,
    },
};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::config::EhashMintConfig;

/// Get current Unix timestamp in seconds.
fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Log a structured JSON event for ehash integration testing.
macro_rules! ehash_log {
    ($event:expr, $($key:tt : $value:expr),* $(,)?) => {
        {
            let json = serde_json::json!({
                "event": $event,
                "ts": unix_timestamp(),
                $($key: $value),*
            });
            info!(target: "ehash_json", "{}", json);
        }
    };
}

/// Maximum number of reports to buffer when connection is down.
const MAX_BUFFER_SIZE: usize = 10000;

/// Initial retry delay in milliseconds.
const INITIAL_RETRY_DELAY_MS: u64 = 1000;

/// Maximum retry delay in milliseconds.
const MAX_RETRY_DELAY_MS: u64 = 5000;

/// How long to wait with full buffer before starting file fallback.
const FILE_FALLBACK_AFTER_SECS: u64 = 30;

/// Default fallback log file path.
const DEFAULT_FALLBACK_LOG_PATH: &str = "/tmp/ehash-reports-fallback.log";

/// Channel ID type (downstream_id, channel_id).
pub type ChannelKey = (usize, u32);

/// Stores the mapping of channel → ehash pubkey.
#[derive(Debug, Clone, Default)]
pub struct EhashPubkeyStore {
    /// Maps (downstream_id, channel_id) → EhashPubkey
    pubkeys: HashMap<ChannelKey, EhashPubkey>,
}

impl EhashPubkeyStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self {
            pubkeys: HashMap::new(),
        }
    }

    /// Store a pubkey for a channel.
    pub fn insert(&mut self, downstream_id: usize, channel_id: u32, pubkey: EhashPubkey) {
        let key = (downstream_id, channel_id);
        debug!(
            downstream_id,
            channel_id,
            pubkey = %pubkey,
            "Stored ehash pubkey for channel"
        );
        self.pubkeys.insert(key, pubkey);
    }

    /// Get a pubkey for a channel.
    pub fn get(&self, downstream_id: usize, channel_id: u32) -> Option<&EhashPubkey> {
        self.pubkeys.get(&(downstream_id, channel_id))
    }

    /// Remove a pubkey for a channel.
    pub fn remove(&mut self, downstream_id: usize, channel_id: u32) -> Option<EhashPubkey> {
        self.pubkeys.remove(&(downstream_id, channel_id))
    }

    /// Remove all pubkeys for a downstream.
    pub fn remove_downstream(&mut self, downstream_id: usize) {
        self.pubkeys.retain(|(did, _), _| *did != downstream_id);
    }

    /// Get number of stored pubkeys.
    pub fn len(&self) -> usize {
        self.pubkeys.len()
    }

    /// Check if store is empty.
    pub fn is_empty(&self) -> bool {
        self.pubkeys.is_empty()
    }
}

/// Data for a share report to send to ehash-mint.
///
/// This is the internal representation sent through the async channel.
/// The dedicated task converts this to the wire format (ShareReport or BlockFoundReport).
#[derive(Debug, Clone)]
pub struct ShareReportData {
    /// Miner's ehash pubkey (33 bytes compressed)
    pub pubkey: [u8; 33],
    /// Unique share identifier (32 bytes) - the SHA256d hash from share validation
    pub share_hash: [u8; 32],
    /// Pre-computed difficulty ratio: channel_difficulty / network_difficulty
    pub difficulty_ratio: f64,
    /// Whether this share found a block
    pub block_found: bool,
}

/// Data for a chain tip update to send to ehash-mint.
///
/// Sent when JDC receives SetNewPrevHash from the Template Provider,
/// allowing the mint to track block confirmations for keyset lifecycle.
#[derive(Debug, Clone)]
pub struct ChainTipUpdateData {
    /// Previous block hash (32 bytes) - the new chain tip
    pub prev_hash: [u8; 32],
    /// Block height of the new chain tip
    pub height: u64,
}

/// Message types that can be sent to ehash-mint.
#[derive(Debug, Clone)]
pub enum MintMessage {
    /// Share report (regular or block found)
    ShareReport(ShareReportData),
    /// Chain tip update (new block on chain)
    ChainTipUpdate(ChainTipUpdateData),
}

/// Share data pending hpub registration.
///
/// When a share arrives for a channel that doesn't have a registered hpub yet,
/// we buffer the share data here until `RegisterChannelPubkey` arrives.
#[derive(Debug, Clone)]
pub struct PendingShareData {
    /// Unique share identifier (32 bytes) - the SHA256d hash from share validation
    pub share_hash: [u8; 32],
    /// Pre-computed difficulty ratio: channel_difficulty / network_difficulty
    pub difficulty_ratio: f64,
    /// Whether this share found a block
    pub block_found: bool,
}

impl PendingShareData {
    /// Convert to ShareReportData with the given pubkey.
    pub fn to_report(&self, pubkey: &EhashPubkey) -> ShareReportData {
        let mut pubkey_bytes = [0u8; 33];
        pubkey_bytes.copy_from_slice(pubkey.as_bytes());
        ShareReportData {
            pubkey: pubkey_bytes,
            share_hash: self.share_hash,
            difficulty_ratio: self.difficulty_ratio,
            block_found: self.block_found,
        }
    }
}

/// Buffered shares for a channel awaiting hpub registration.
#[derive(Debug)]
pub struct PendingChannelShares {
    /// Buffered shares
    pub shares: Vec<PendingShareData>,
    /// Deadline for receiving RegisterChannelPubkey (after this, disconnect)
    pub deadline: Instant,
}

impl PendingChannelShares {
    /// Create a new pending channel shares buffer with the given timeout.
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            shares: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(timeout_secs),
        }
    }

    /// Check if the deadline has passed.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Add a share to the buffer.
    pub fn push(&mut self, share: PendingShareData) {
        self.shares.push(share);
    }
}

/// Data for a RegisterChannelPubkey message received from downstream.
///
/// This is sent from the downstream handler to the channel manager when
/// a translator sends a RegisterChannelPubkey message after mining.authorize.
#[derive(Debug, Clone)]
pub struct RegisterChannelPubkeyData {
    /// The downstream connection ID
    pub downstream_id: usize,
    /// The channel ID to associate with this pubkey
    pub channel_id: u32,
    /// The miner's ehash pubkey (33 bytes compressed)
    pub pubkey: EhashPubkey,
}

/// A sender for mint messages that can be either active (sends to mint) or inactive (no-op).
///
/// This allows the share validation code to always call `try_send()` without checking
/// if ehash-mint is configured.
#[derive(Clone)]
pub enum MintMessageSender {
    /// Active sender - forwards messages to the ehash-mint task
    Active(Sender<MintMessage>),
    /// Inactive sender - discards messages (ehash-mint not configured)
    Inactive,
}

impl MintMessageSender {
    /// Send a share report (non-blocking).
    ///
    /// If ehash-mint is not configured (Inactive), this is a no-op.
    /// If the channel is full, logs an error and drops the report.
    pub fn try_send_share(&self, report: ShareReportData) {
        match self {
            MintMessageSender::Active(sender) => {
                match sender.try_send(MintMessage::ShareReport(report)) {
                    Ok(()) => {
                        debug!("Share report queued for ehash-mint");
                    }
                    Err(async_channel::TrySendError::Full(MintMessage::ShareReport(report))) => {
                        let share_hash = hex::encode(&report.share_hash[..8]);
                        error!(
                            share_hash = %share_hash,
                            "ehash-mint channel full, dropping share report"
                        );
                        ehash_log!("ehash.channel.full",
                            "share_hash": share_hash,
                            "block_found": report.block_found
                        );
                    }
                    Err(async_channel::TrySendError::Full(_)) => {
                        error!("ehash-mint channel full, dropping message");
                    }
                    Err(async_channel::TrySendError::Closed(_)) => {
                        error!("ehash-mint channel closed");
                    }
                }
            }
            MintMessageSender::Inactive => {
                // No-op when ehash-mint is not configured
            }
        }
    }

    /// Send a chain tip update (non-blocking).
    ///
    /// If ehash-mint is not configured (Inactive), this is a no-op.
    /// If the channel is full, logs a warning and drops the update (less critical than shares).
    pub fn try_send_chain_tip(&self, update: ChainTipUpdateData) {
        match self {
            MintMessageSender::Active(sender) => {
                match sender.try_send(MintMessage::ChainTipUpdate(update)) {
                    Ok(()) => {
                        debug!("Chain tip update queued for ehash-mint");
                    }
                    Err(async_channel::TrySendError::Full(_)) => {
                        warn!("ehash-mint channel full, dropping chain tip update");
                    }
                    Err(async_channel::TrySendError::Closed(_)) => {
                        error!("ehash-mint channel closed");
                    }
                }
            }
            MintMessageSender::Inactive => {
                // No-op when ehash-mint is not configured
            }
        }
    }
}

/// Backwards compatible alias for ShareReportSender
pub type ShareReportSender = MintMessageSender;

/// ehash-mint client that sends share reports and chain tip updates.
///
/// Messages are sent through an async channel to a dedicated task,
/// ensuring the mining hot path is not blocked by network I/O.
pub struct EhashMintClient {
    /// Configuration
    config: EhashMintConfig,
    /// Sender channel for mint messages (non-blocking)
    message_sender: Sender<MintMessage>,
    /// Pubkey store (shared with channel manager)
    pubkey_store: Arc<Mutex<EhashPubkeyStore>>,
}

impl EhashMintClient {
    /// Create a new ehash-mint client.
    ///
    /// Returns None if ehash-mint is not configured.
    ///
    /// # Arguments
    /// * `config` - ehash-mint configuration
    ///
    /// # Returns
    /// A tuple of (client, receiver) where the receiver is passed to the dedicated task.
    pub fn new(config: &EhashMintConfig) -> Option<(Self, Receiver<MintMessage>)> {
        if !config.is_configured() {
            info!("ehash-mint integration disabled (not configured)");
            return None;
        }

        // Bounded channel to provide backpressure if mint connection is slow
        // Channel size of 10000 provides buffer for ~10 seconds at 1000 shares/sec
        let (message_sender, message_receiver) = bounded(10000);
        let pubkey_store = Arc::new(Mutex::new(EhashPubkeyStore::new()));

        let client = Self {
            config: config.clone(),
            message_sender,
            pubkey_store,
        };

        info!(
            address = ?config.socket_addr(),
            "ehash-mint client created"
        );

        Some((client, message_receiver))
    }

    /// Get a reference to the pubkey store for use by channel manager.
    pub fn pubkey_store(&self) -> Arc<Mutex<EhashPubkeyStore>> {
        Arc::clone(&self.pubkey_store)
    }

    /// Get a sender for submitting messages to the mint.
    ///
    /// This sender can be cloned and used from multiple places.
    pub fn message_sender(&self) -> MintMessageSender {
        MintMessageSender::Active(self.message_sender.clone())
    }

    /// Get a sender for submitting share reports.
    ///
    /// Alias for message_sender() for backwards compatibility.
    pub fn report_sender(&self) -> ShareReportSender {
        self.message_sender()
    }

    /// Get the mint socket address if configured.
    pub fn socket_addr(&self) -> Option<std::net::SocketAddr> {
        self.config.socket_addr()
    }
}

/// Start the dedicated task that handles mint communication.
///
/// This task:
/// 1. Receives messages (share reports, chain tip updates) from the async channel
/// 2. Connects to ehash-mint over Noise-encrypted TCP
/// 3. Sends ShareReport, BlockFoundReport, or ChainTipUpdate messages
///
/// The task runs until the shutdown signal is received or the channel is closed.
pub fn start_mint_task(
    config: EhashMintConfig,
    message_receiver: Receiver<MintMessage>,
    shutdown: broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        run_mint_task(config, message_receiver, shutdown).await;
    });
}

/// Connection state for the mint task.
enum ConnectionState {
    /// Not connected, will attempt to connect.
    Disconnected,
    /// Connected with write half of the stream.
    Connected(NoiseTcpWriteHalf<EhashMessage<'static>>),
}

/// State for tracking file fallback.
struct FallbackState {
    /// When the buffer became full with connection down.
    buffer_full_since: Option<Instant>,
    /// Whether we're currently writing to fallback file.
    using_fallback: bool,
    /// Path to the fallback log file.
    fallback_path: PathBuf,
    /// Number of reports written to fallback file.
    fallback_count: u64,
}

impl FallbackState {
    fn new(fallback_path: Option<PathBuf>) -> Self {
        Self {
            buffer_full_since: None,
            using_fallback: false,
            fallback_path: fallback_path
                .unwrap_or_else(|| PathBuf::from(DEFAULT_FALLBACK_LOG_PATH)),
            fallback_count: 0,
        }
    }

    /// Check if we should start using fallback file.
    fn should_use_fallback(&self, buffer_full: bool, connected: bool) -> bool {
        if connected {
            return false;
        }
        if !buffer_full {
            return false;
        }
        if let Some(since) = self.buffer_full_since {
            since.elapsed() > Duration::from_secs(FILE_FALLBACK_AFTER_SECS)
        } else {
            false
        }
    }

    /// Update buffer full tracking.
    fn update_buffer_full(&mut self, buffer_full: bool) {
        if buffer_full {
            if self.buffer_full_since.is_none() {
                self.buffer_full_since = Some(Instant::now());
            }
        } else {
            self.buffer_full_since = None;
            if self.using_fallback {
                info!(
                    fallback_count = self.fallback_count,
                    "Connection restored, stopping fallback file writes"
                );
                self.using_fallback = false;
            }
        }
    }

    /// Write a report to the fallback file.
    fn write_to_fallback(&mut self, report: &ShareReportData) -> Result<(), std::io::Error> {
        if !self.using_fallback {
            self.using_fallback = true;
            warn!(
                path = %self.fallback_path.display(),
                "Buffer full and connection down too long, writing to fallback file"
            );
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.fallback_path)?;

        // Write as JSON line
        let json = serde_json::json!({
            "pubkey": hex::encode(&report.pubkey),
            "share_hash": hex::encode(&report.share_hash),
            "difficulty_ratio": report.difficulty_ratio,
            "block_found": report.block_found,
            "timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        writeln!(file, "{}", json)?;
        self.fallback_count += 1;

        Ok(())
    }
}

/// Attempt to establish a Noise connection to the ehash-mint.
async fn connect_to_mint(
    addr: std::net::SocketAddr,
    authority_pubkey: &Secp256k1PublicKey,
) -> Result<NoiseTcpWriteHalf<EhashMessage<'static>>, String> {
    // Connect TCP with timeout
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| "Connection timeout".to_string())?
        .map_err(|e| format!("TCP connect failed: {}", e))?;

    // Create Noise initiator with mint's public key
    let initiator = Initiator::from_raw_k(authority_pubkey.into_bytes())
        .map_err(|e| format!("Failed to create initiator: {:?}", e))?;

    // Perform Noise handshake
    // We need to import the NoiseTcpStream type for EhashMessage
    use stratum_apps::network_helpers::noise_stream::NoiseTcpStream;

    let noise_stream =
        NoiseTcpStream::<EhashMessage<'static>>::new(stream, HandshakeRole::Initiator(initiator))
            .await
            .map_err(|e| format!("Noise handshake failed: {:?}", e))?;

    // We only need the write half for fire-and-forget
    let (_read_half, write_half) = noise_stream.into_split();

    Ok(write_half)
}

/// Convert ShareReportData to an EhashMessage and send it.
async fn send_share_report(
    writer: &mut NoiseTcpWriteHalf<EhashMessage<'static>>,
    report: &ShareReportData,
) -> Result<(), String> {
    // Create the wire-format message
    let pubkey: CompressedPubKey = report
        .pubkey
        .to_vec()
        .try_into()
        .map_err(|_| "Invalid pubkey length")?;
    let share_hash: U256 = report.share_hash.into();

    let message: EhashMessage<'static> = if report.block_found {
        BlockFoundReport::new(pubkey, share_hash, report.difficulty_ratio).into()
    } else {
        ShareReport::new(pubkey, share_hash, report.difficulty_ratio).into()
    };

    send_ehash_message(writer, message).await
}

/// Convert ChainTipUpdateData to an EhashMessage and send it.
async fn send_chain_tip_update(
    writer: &mut NoiseTcpWriteHalf<EhashMessage<'static>>,
    update: &ChainTipUpdateData,
) -> Result<(), String> {
    let prev_hash: U256 = update.prev_hash.into();
    let message: EhashMessage<'static> = ChainTipUpdate::new(prev_hash, update.height).into();
    send_ehash_message(writer, message).await
}

/// Send an EhashMessage over the Noise connection.
async fn send_ehash_message(
    writer: &mut NoiseTcpWriteHalf<EhashMessage<'static>>,
    message: EhashMessage<'static>,
) -> Result<(), String> {
    // Get message metadata using IsSv2Message trait
    let message_type = message.message_type();
    let extension_type = message.extension_type();
    let channel_bit = message.channel_bit();

    // Create frame with Slice buffer type to match StandardEitherFrame
    let frame: Sv2Frame<EhashMessage<'static>, Slice> =
        Sv2Frame::from_message(message, message_type, extension_type, channel_bit)
            .ok_or("Failed to create frame: payload too large")?;

    // Send - Frame<T, B> implements From<Sv2Frame<T, B>>
    writer
        .write_frame(frame.into())
        .await
        .map_err(|e| format!("Write failed: {:?}", e))?;

    Ok(())
}

/// Run the mint communication task.
async fn run_mint_task(
    config: EhashMintConfig,
    message_receiver: Receiver<MintMessage>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let addr = match config.socket_addr() {
        Some(addr) => addr,
        None => {
            error!("ehash-mint not properly configured, task exiting");
            return;
        }
    };

    let authority_pubkey = match &config.authority_pubkey {
        Some(pk) => pk.clone(),
        None => {
            error!("ehash-mint authority pubkey not configured, task exiting");
            return;
        }
    };

    info!(
        address = ?addr,
        "ehash-mint task started"
    );

    let mut connection_state = ConnectionState::Disconnected;
    let mut buffer: VecDeque<ShareReportData> = VecDeque::with_capacity(MAX_BUFFER_SIZE);
    let mut fallback_state = FallbackState::new(config.fallback_log_path.clone());

    let mut report_count: u64 = 0;
    let mut block_count: u64 = 0;
    let mut chain_tip_count: u64 = 0;
    let mut sent_count: u64 = 0;
    let mut retry_delay = Duration::from_millis(INITIAL_RETRY_DELAY_MS);
    let mut last_connect_attempt: Option<Instant> = None;

    loop {
        // Check if we should attempt to connect
        let should_connect = matches!(connection_state, ConnectionState::Disconnected)
            && last_connect_attempt
                .map(|t| t.elapsed() >= retry_delay)
                .unwrap_or(true);

        if should_connect {
            last_connect_attempt = Some(Instant::now());
            info!(address = ?addr, "Attempting to connect to ehash-mint");

            match connect_to_mint(addr, &authority_pubkey).await {
                Ok(writer) => {
                    info!(address = ?addr, "Connected to ehash-mint");
                    ehash_log!("ehash.connection.established",
                        "address": addr.to_string(),
                        "buffered_count": buffer.len()
                    );
                    connection_state = ConnectionState::Connected(writer);
                    retry_delay = Duration::from_millis(INITIAL_RETRY_DELAY_MS);

                    // Drain buffer
                    let buffered_count = buffer.len();
                    if buffered_count > 0 {
                        info!(count = buffered_count, "Draining buffered reports");
                    }
                }
                Err(e) => {
                    warn!(
                        address = ?addr,
                        error = %e,
                        retry_in_ms = retry_delay.as_millis(),
                        "Failed to connect to ehash-mint"
                    );
                    ehash_log!("ehash.connection.failed",
                        "address": addr.to_string(),
                        "error": e,
                        "retry_in_ms": retry_delay.as_millis() as u64
                    );
                    // Exponential backoff with cap
                    retry_delay =
                        std::cmp::min(retry_delay * 2, Duration::from_millis(MAX_RETRY_DELAY_MS));
                }
            }
        }

        // If connected, try to drain buffer first
        if let ConnectionState::Connected(ref mut writer) = connection_state {
            while let Some(report) = buffer.front() {
                match send_share_report(writer, report).await {
                    Ok(()) => {
                        buffer.pop_front();
                        sent_count += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, "Connection lost while draining buffer");
                        connection_state = ConnectionState::Disconnected;
                        break;
                    }
                }
            }
        }

        // Update fallback state
        let is_connected = matches!(connection_state, ConnectionState::Connected(_));
        let buffer_full = buffer.len() >= MAX_BUFFER_SIZE;
        fallback_state.update_buffer_full(buffer_full);

        tokio::select! {
            biased;

            _ = shutdown.recv() => {
                info!(
                    report_count,
                    block_count,
                    chain_tip_count,
                    sent_count,
                    buffered = buffer.len(),
                    fallback_count = fallback_state.fallback_count,
                    "ehash-mint task shutting down"
                );
                break;
            }

            result = message_receiver.recv() => {
                match result {
                    Ok(MintMessage::ShareReport(report)) => {
                        report_count += 1;
                        let share_hash_hex = hex::encode(&report.share_hash[..8]);
                        let pubkey_hex = hex::encode(&report.pubkey[..8]);

                        if report.block_found {
                            block_count += 1;
                            info!(
                                share_hash = %share_hash_hex,
                                pubkey = %pubkey_hex,
                                difficulty_ratio = report.difficulty_ratio,
                                "BlockFoundReport received"
                            );
                            ehash_log!("ehash.report.queued",
                                "share_hash": share_hash_hex,
                                "pubkey": pubkey_hex,
                                "difficulty_ratio": report.difficulty_ratio,
                                "block_found": true
                            );
                        } else {
                            debug!(
                                share_hash = %share_hash_hex,
                                pubkey = %pubkey_hex,
                                difficulty_ratio = report.difficulty_ratio,
                                "ShareReport received"
                            );
                            ehash_log!("ehash.report.queued",
                                "share_hash": share_hash_hex,
                                "pubkey": pubkey_hex,
                                "difficulty_ratio": report.difficulty_ratio,
                                "block_found": false
                            );
                        }

                        // Try to send immediately if connected
                        if let ConnectionState::Connected(ref mut writer) = connection_state {
                            match send_share_report(writer, &report).await {
                                Ok(()) => {
                                    sent_count += 1;
                                    ehash_log!("ehash.report.sent",
                                        "share_hash": share_hash_hex,
                                        "block_found": report.block_found
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    warn!(error = %e, "Connection lost while sending report");
                                    connection_state = ConnectionState::Disconnected;
                                    // Fall through to buffer the report
                                }
                            }
                        }

                        // Buffer the report
                        if buffer.len() < MAX_BUFFER_SIZE {
                            buffer.push_back(report.clone());
                            ehash_log!("ehash.report.buffered",
                                "share_hash": share_hash_hex,
                                "block_found": report.block_found,
                                "buffer_size": buffer.len()
                            );
                        } else if fallback_state.should_use_fallback(true, is_connected) {
                            // Write to fallback file
                            if let Err(e) = fallback_state.write_to_fallback(&report) {
                                error!(error = %e, "Failed to write to fallback file");
                            } else {
                                ehash_log!("ehash.fallback.written",
                                    "share_hash": share_hash_hex,
                                    "block_found": report.block_found,
                                    "fallback_count": fallback_state.fallback_count
                                );
                            }
                        } else {
                            // Buffer full but not yet time for fallback - drop oldest
                            buffer.pop_front();
                            buffer.push_back(report);
                            debug!("Buffer full, dropped oldest report");
                        }
                    }
                    Ok(MintMessage::ChainTipUpdate(update)) => {
                        chain_tip_count += 1;
                        let prev_hash_hex = hex::encode(&update.prev_hash[..8]);

                        debug!(
                            height = update.height,
                            prev_hash = %prev_hash_hex,
                            "ChainTipUpdate received"
                        );
                        ehash_log!("ehash.chain_tip.queued",
                            "height": update.height,
                            "prev_hash": prev_hash_hex
                        );

                        // Try to send immediately if connected
                        // Chain tip updates are not buffered - they're informational
                        if let ConnectionState::Connected(ref mut writer) = connection_state {
                            match send_chain_tip_update(writer, &update).await {
                                Ok(()) => {
                                    ehash_log!("ehash.chain_tip.sent",
                                        "height": update.height,
                                        "prev_hash": prev_hash_hex
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        height = update.height,
                                        "Failed to send chain tip update (connection lost)"
                                    );
                                    connection_state = ConnectionState::Disconnected;
                                }
                            }
                        } else {
                            // Not connected - drop chain tip update (non-critical)
                            debug!(
                                height = update.height,
                                "Dropping chain tip update (not connected)"
                            );
                        }
                    }
                    Err(_) => {
                        info!("ehash-mint message channel closed");
                        break;
                    }
                }
            }

            // Small sleep to prevent busy-looping when disconnected
            _ = tokio::time::sleep(Duration::from_millis(10)), if matches!(connection_state, ConnectionState::Disconnected) => {
                // Just continue the loop to check connection
            }
        }
    }

    info!(
        report_count,
        block_count,
        chain_tip_count,
        sent_count,
        fallback_count = fallback_state.fallback_count,
        "ehash-mint task stopped"
    );
}

/// Create a share report from share validation data.
///
/// This helper is called from the share validation code to create a report
/// that can be sent through the async channel.
///
/// # Arguments
/// * `pubkey` - Miner's ehash pubkey (from the channel's stored hpub)
/// * `share_hash` - SHA256d hash of the share (from validation result)
/// * `difficulty_ratio` - Pre-computed channel_difficulty / network_difficulty
/// * `block_found` - Whether this share found a valid block
pub fn create_share_report(
    pubkey: &EhashPubkey,
    share_hash: [u8; 32],
    difficulty_ratio: f64,
    block_found: bool,
) -> ShareReportData {
    let mut pubkey_bytes = [0u8; 33];
    pubkey_bytes.copy_from_slice(pubkey.as_bytes());

    ShareReportData {
        pubkey: pubkey_bytes,
        share_hash,
        difficulty_ratio,
        block_found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pubkey_store() {
        let mut store = EhashPubkeyStore::new();
        assert!(store.is_empty());

        // Create a dummy pubkey
        let pubkey_bytes = [0x02u8; 33];
        let pubkey = EhashPubkey::from_bytes(&pubkey_bytes).unwrap();

        // Insert and retrieve
        store.insert(1, 10, pubkey.clone());
        assert_eq!(store.len(), 1);
        assert!(store.get(1, 10).is_some());
        assert!(store.get(1, 11).is_none());
        assert!(store.get(2, 10).is_none());

        // Remove single
        store.remove(1, 10);
        assert!(store.is_empty());

        // Remove by downstream
        store.insert(1, 10, pubkey.clone());
        store.insert(1, 11, pubkey.clone());
        store.insert(2, 10, pubkey.clone());
        assert_eq!(store.len(), 3);

        store.remove_downstream(1);
        assert_eq!(store.len(), 1);
        assert!(store.get(2, 10).is_some());
    }

    #[test]
    fn test_create_share_report() {
        let pubkey_bytes = [0x02u8; 33];
        let pubkey = EhashPubkey::from_bytes(&pubkey_bytes).unwrap();
        let share_hash = [0xab; 32];

        let report = create_share_report(&pubkey, share_hash, 0.001, false);

        assert_eq!(report.pubkey, pubkey_bytes);
        assert_eq!(report.share_hash, share_hash);
        assert!((report.difficulty_ratio - 0.001).abs() < 1e-10);
        assert!(!report.block_found);
    }

    #[test]
    fn test_create_block_found_report() {
        let pubkey_bytes = [0x03u8; 33];
        let pubkey = EhashPubkey::from_bytes(&pubkey_bytes).unwrap();
        let share_hash = [0xcd; 32];

        let report = create_share_report(&pubkey, share_hash, 1.5, true);

        assert!(report.block_found);
        assert!((report.difficulty_ratio - 1.5).abs() < 1e-10);
    }
}
