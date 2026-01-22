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
//! - Both contain: pubkey(33) + share_hash(32) + difficulty_ratio(8)

use std::collections::HashMap;
use std::sync::Arc;

use async_channel::{bounded, Receiver, Sender};
use ehash_core::EhashPubkey;
use stratum_apps::custom_mutex::Mutex;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::config::EhashMintConfig;

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
    /// Unique share identifier (32 bytes)
    pub share_hash: [u8; 32],
    /// Pre-computed difficulty ratio: share_difficulty / network_difficulty
    pub difficulty_ratio: f64,
    /// Whether this share found a block
    pub block_found: bool,
}

/// ehash-mint client that sends share reports.
///
/// Share reports are sent through an async channel to a dedicated task,
/// ensuring the mining hot path is not blocked by network I/O.
pub struct EhashMintClient {
    /// Configuration
    config: EhashMintConfig,
    /// Sender channel for share reports (non-blocking)
    report_sender: Sender<ShareReportData>,
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
    pub fn new(config: &EhashMintConfig) -> Option<(Self, Receiver<ShareReportData>)> {
        if !config.is_configured() {
            info!("ehash-mint integration disabled (not configured)");
            return None;
        }

        // Bounded channel to provide backpressure if mint connection is slow
        // Channel size of 10000 provides buffer for ~10 seconds at 1000 shares/sec
        let (report_sender, report_receiver) = bounded(10000);
        let pubkey_store = Arc::new(Mutex::new(EhashPubkeyStore::new()));

        let client = Self {
            config: config.clone(),
            report_sender,
            pubkey_store,
        };

        info!(
            address = ?config.socket_addr(),
            "ehash-mint client created"
        );

        Some((client, report_receiver))
    }

    /// Get a reference to the pubkey store for use by channel manager.
    pub fn pubkey_store(&self) -> Arc<Mutex<EhashPubkeyStore>> {
        Arc::clone(&self.pubkey_store)
    }

    /// Get a sender for submitting share reports.
    ///
    /// This sender can be cloned and used from multiple places (e.g., share validation handlers).
    pub fn report_sender(&self) -> Sender<ShareReportData> {
        self.report_sender.clone()
    }

    /// Get the mint socket address if configured.
    pub fn socket_addr(&self) -> Option<std::net::SocketAddr> {
        self.config.socket_addr()
    }

    /// Send a share report (non-blocking).
    ///
    /// This uses try_send to avoid blocking the caller if the channel is full.
    /// If the channel is full, the share is dropped (logged as warning).
    pub fn try_send(&self, report: ShareReportData) {
        match self.report_sender.try_send(report) {
            Ok(()) => {
                debug!("Share report queued for ehash-mint");
            }
            Err(async_channel::TrySendError::Full(report)) => {
                warn!(
                    share_hash = hex::encode(&report.share_hash[..8]),
                    "ehash-mint channel full, dropping share report"
                );
            }
            Err(async_channel::TrySendError::Closed(_)) => {
                error!("ehash-mint channel closed");
            }
        }
    }
}

/// Start the dedicated task that handles mint communication.
///
/// This task:
/// 1. Receives share reports from the async channel
/// 2. Connects to ehash-mint over Noise-encrypted TCP
/// 3. Sends ShareReport or BlockFoundReport messages
///
/// The task runs until the shutdown signal is received or the channel is closed.
pub fn start_mint_task(
    config: EhashMintConfig,
    report_receiver: Receiver<ShareReportData>,
    shutdown: broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        run_mint_task(config, report_receiver, shutdown).await;
    });
}

/// Run the mint communication task.
async fn run_mint_task(
    config: EhashMintConfig,
    report_receiver: Receiver<ShareReportData>,
    mut shutdown: broadcast::Receiver<()>,
) {
    info!(
        address = ?config.socket_addr(),
        "ehash-mint task started"
    );

    // TODO: Establish Noise connection to ehash-mint
    // For now, just log the reports
    let mut report_count: u64 = 0;
    let mut block_count: u64 = 0;

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!(
                    report_count,
                    block_count,
                    "ehash-mint task shutting down"
                );
                break;
            }
            result = report_receiver.recv() => {
                match result {
                    Ok(report) => {
                        report_count += 1;
                        if report.block_found {
                            block_count += 1;
                            info!(
                                share_hash = hex::encode(&report.share_hash[..8]),
                                pubkey = hex::encode(&report.pubkey[..8]),
                                difficulty_ratio = report.difficulty_ratio,
                                "BlockFoundReport queued for ehash-mint"
                            );
                        } else {
                            debug!(
                                share_hash = hex::encode(&report.share_hash[..8]),
                                pubkey = hex::encode(&report.pubkey[..8]),
                                difficulty_ratio = report.difficulty_ratio,
                                "ShareReport queued for ehash-mint"
                            );
                        }

                        // TODO: Actually send to ehash-mint over Noise connection
                        // let message = if report.block_found {
                        //     EhashMessage::BlockFoundReport(BlockFoundReport::new(...))
                        // } else {
                        //     EhashMessage::ShareReport(ShareReport::new(...))
                        // };
                        // stream.write_frame(message.try_into()?).await?;
                    }
                    Err(_) => {
                        info!("ehash-mint report channel closed");
                        break;
                    }
                }
            }
        }
    }

    info!(report_count, block_count, "ehash-mint task stopped");
}

/// Create a share report from share validation data.
///
/// This helper is called from the share validation code to create a report
/// that can be sent through the async channel.
///
/// # Arguments
/// * `pubkey` - Miner's ehash pubkey (from the channel's stored hpub)
/// * `share_hash` - SHA256d hash of the share
/// * `difficulty_ratio` - Pre-computed share_difficulty / network_difficulty
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
