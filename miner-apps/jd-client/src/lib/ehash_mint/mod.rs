//! ehash-mint client module.
//!
//! This module handles communication with the ehash-mint daemon for ecash
//! token issuance based on mining shares.
//!
//! Current status: Stubbed out with data types only. Full implementation
//! pending proper Sv2 framing integration for the mint protocol.

use std::collections::HashMap;
use std::sync::Arc;

use async_channel::{unbounded, Receiver, Sender};
use ehash_core::EhashPubkey;
use stratum_apps::custom_mutex::Mutex;
use tracing::{debug, info, warn};

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
        info!(
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
}

/// Data for an EhashShareReport message.
///
/// This matches the EhashShareReport message format expected by ehash-mint.
#[derive(Debug, Clone)]
pub struct ShareReportData {
    /// Miner's ehash pubkey (33 bytes compressed)
    pub pubkey: [u8; 33],
    /// Unique share identifier (32 bytes)
    pub share_hash: [u8; 32],
    /// Share difficulty (scaled by 1e9)
    pub share_difficulty: u64,
    /// Network difficulty (scaled by 1e9)
    pub network_difficulty: u64,
    /// Whether this share found a block
    pub block_found: bool,
    /// Unix timestamp
    pub timestamp: u64,
    /// Channel ID
    pub channel_id: u32,
}

/// ehash-mint client that sends share reports.
///
/// This is currently a stub that logs share reports but doesn't send them.
/// Full implementation requires proper Sv2 framing for the mint protocol.
pub struct EhashMintClient {
    /// Configuration
    #[allow(dead_code)]
    config: EhashMintConfig,
    /// Sender channel for share reports
    report_sender: Sender<ShareReportData>,
    /// Pubkey store (shared with channel manager)
    pubkey_store: Arc<Mutex<EhashPubkeyStore>>,
}

impl EhashMintClient {
    /// Create a new ehash-mint client.
    ///
    /// Returns None if ehash-mint is not configured.
    pub fn new(config: &EhashMintConfig) -> Option<(Self, Receiver<ShareReportData>)> {
        if !config.is_configured() {
            info!("ehash-mint integration disabled (not configured)");
            return None;
        }

        let (report_sender, report_receiver) = unbounded();
        let pubkey_store = Arc::new(Mutex::new(EhashPubkeyStore::new()));

        let client = Self {
            config: config.clone(),
            report_sender,
            pubkey_store,
        };

        Some((client, report_receiver))
    }

    /// Get a reference to the pubkey store for use by channel manager.
    pub fn pubkey_store(&self) -> Arc<Mutex<EhashPubkeyStore>> {
        Arc::clone(&self.pubkey_store)
    }

    /// Get a sender for submitting share reports.
    pub fn report_sender(&self) -> Sender<ShareReportData> {
        self.report_sender.clone()
    }

    /// Start processing share reports.
    ///
    /// Currently this just logs the reports. Full implementation will
    /// connect to ehash-mint and send reports over Sv2 Noise.
    pub fn start_logging(report_receiver: Receiver<ShareReportData>) {
        tokio::spawn(async move {
            while let Ok(report) = report_receiver.recv().await {
                // TODO: Send to ehash-mint over Sv2 Noise connection
                // For now, just log the report
                info!(
                    share_hash = hex::encode(&report.share_hash[..8]),
                    pubkey = hex::encode(&report.pubkey[..8]),
                    share_difficulty = report.share_difficulty,
                    block_found = report.block_found,
                    "Share report received (ehash-mint connection TODO)"
                );
            }
            warn!("Share report channel closed");
        });
    }
}

/// Create a share report from share validation data.
///
/// This helper converts the share validation result into a ShareReportData
/// that can be sent to the ehash-mint.
pub fn create_share_report(
    pubkey: &EhashPubkey,
    share_hash: [u8; 32],
    share_difficulty: u64,
    network_difficulty: u64,
    block_found: bool,
    channel_id: u32,
) -> ShareReportData {
    let mut pubkey_bytes = [0u8; 33];
    pubkey_bytes.copy_from_slice(pubkey.as_bytes());

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    ShareReportData {
        pubkey: pubkey_bytes,
        share_hash,
        share_difficulty,
        network_difficulty,
        block_found,
        timestamp,
        channel_id,
    }
}
