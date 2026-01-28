use std::net::SocketAddr;

use stratum_apps::{
    custom_mutex::Mutex,
    key_utils::Secp256k1PublicKey,
    stratum_core::{
        binary_sv2::{Sv2DataType, U256},
        bitcoin::{
            block::{Header, Version},
            hashes::Hash,
            CompactTarget, Target, TxMerkleNode,
        },
        channels_sv2::{
            merkle_root::merkle_root_from_path,
            target::{bytes_to_hex, u256_to_block_hash},
        },
        sv1_api::{client_to_server, utils::HexU32Be},
    },
    utils::types::{ChannelId, DownstreamId},
};

use tokio::sync::mpsc;
use tracing::debug;

use crate::error::TproxyErrorKind;

/// Validates an SV1 share against the target difficulty and job parameters.
///
/// This function performs complete share validation by:
/// 1. Finding the corresponding job from the valid jobs storage
/// 2. Constructing the full extranonce from extranonce1 and extranonce2
/// 3. Calculating the merkle root from the coinbase transaction and merkle path
/// 4. Building the block header with the share's nonce and timestamp
/// 5. Hashing the header and comparing against the target difficulty
///
/// # Arguments
/// * `share` - The SV1 submit message containing the share data
/// * `target` - The target difficulty for this share
/// * `extranonce1` - The first part of the extranonce (from server)
/// * `version_rolling_mask` - Optional mask for version rolling
/// * `sv1_server_data` - Reference to shared SV1 server data for accessing valid jobs
/// * `channel_id` - Channel ID for job lookup
///
/// # Returns
/// * `Ok(true)` if the share is valid and meets the target
/// * `Ok(false)` if the share is valid but doesn't meet the target
/// * `Err(TproxyError)` if validation fails due to missing job or invalid data
pub fn validate_sv1_share(
    share: &client_to_server::Submit<'static>,
    target: Target,
    extranonce1: Vec<u8>,
    version_rolling_mask: Option<HexU32Be>,
    sv1_server_data: std::sync::Arc<Mutex<crate::sv1::sv1_server::data::Sv1ServerData>>,
    channel_id: ChannelId,
) -> Result<bool, TproxyErrorKind> {
    let job_id = share.job_id.clone();

    // Access valid jobs based on the configured mode
    let job = sv1_server_data
        .super_safe_lock(|server_data| {
            if let Some(ref aggregated_jobs) = server_data.aggregated_valid_jobs {
                // Aggregated mode: search in shared jobs
                debug!(
                    "Looking for job_id '{}' in aggregated_valid_jobs ({} jobs)",
                    job_id,
                    aggregated_jobs.len()
                );
                aggregated_jobs
                    .iter()
                    .find(|job| job.job_id == job_id)
                    .cloned()
            } else if let Some(ref non_aggregated_jobs) = server_data.non_aggregated_valid_jobs {
                // Non-aggregated mode: search in channel-specific jobs
                let channel_jobs = non_aggregated_jobs.get(&channel_id);
                debug!(
                    "Looking for job_id '{}' in non_aggregated_valid_jobs for channel_id {}: found {} jobs, available channels: {:?}",
                    job_id,
                    channel_id,
                    channel_jobs.map(|j| j.len()).unwrap_or(0),
                    non_aggregated_jobs.keys().collect::<Vec<_>>()
                );
                if let Some(jobs) = channel_jobs {
                    debug!("Available job_ids for channel {}: {:?}", channel_id, jobs.iter().map(|j| &j.job_id).collect::<Vec<_>>());
                }
                channel_jobs
                    .and_then(|channel_jobs| channel_jobs.iter().find(|job| job.job_id == job_id))
                    .cloned()
            } else {
                debug!("No valid_jobs storage configured!");
                None
            }
        })
        .ok_or(TproxyErrorKind::JobNotFound)?;

    let mut full_extranonce = vec![];
    full_extranonce.extend_from_slice(extranonce1.as_slice());
    full_extranonce.extend_from_slice(share.extra_nonce2.0.as_ref());

    let share_version = share
        .version_bits
        .clone()
        .map(|vb| vb.0)
        .unwrap_or(job.version.0);
    let mask = version_rolling_mask.unwrap_or(HexU32Be(0x1FFFE000_u32)).0;
    let version = (job.version.0 & !mask) | (share_version & mask);

    let prev_hash_vec: Vec<u8> = job.prev_hash.clone().into();
    let prev_hash = U256::from_vec_(prev_hash_vec).map_err(TproxyErrorKind::BinarySv2)?;

    // calculate the merkle root from:
    // - job coinbase_tx_prefix
    // - full extranonce
    // - job coinbase_tx_suffix
    // - job merkle_path
    let merkle_root: [u8; 32] = merkle_root_from_path(
        job.coin_base1.as_ref(),
        job.coin_base2.as_ref(),
        full_extranonce.as_ref(),
        job.merkle_branch.as_ref(),
    )
    .ok_or(TproxyErrorKind::InvalidMerkleRoot)?
    .try_into()
    .map_err(|_| TproxyErrorKind::InvalidMerkleRoot)?;

    // create the header for validation
    let header = Header {
        version: Version::from_consensus(version as i32),
        prev_blockhash: u256_to_block_hash(prev_hash),
        merkle_root: TxMerkleNode::from_byte_array(merkle_root),
        time: share.time.0,
        bits: CompactTarget::from_consensus(job.bits.0),
        nonce: share.nonce.0,
    };

    // convert the header hash to a target type for easy comparison
    let hash = header.block_hash();
    let raw_hash: [u8; 32] = *hash.to_raw_hash().as_ref();
    let hash_as_target = Target::from_le_bytes(raw_hash);

    // print hash_as_target and self.target as human readable hex
    let hash_bytes = hash_as_target.to_be_bytes();
    let target_bytes = target.to_be_bytes();

    debug!(
        "share validation \nshare:\t\t{}\ndownstream target:\t{}\n",
        bytes_to_hex(&hash_bytes),
        bytes_to_hex(&target_bytes),
    );
    // check if the share hash meets the downstream target
    if hash_as_target < target {
        /*if self.share_accounting.is_share_seen(hash.to_raw_hash()) {
            return Err(ShareValidationError::DuplicateShare);
        }*/

        return Ok(true);
    }

    Ok(false)
}

/// Calculates the required length of the proxy's extranonce prefix.
///
/// This function determines how many bytes the proxy needs to reserve for its own
/// extranonce prefix, based on the difference between the channel's rollable extranonce
/// size and the downstream miner's rollable extranonce size.
///
/// # Arguments
/// * `channel_rollable_extranonce_size` - Size of the rollable extranonce from the channel
/// * `downstream_rollable_extranonce_size` - Size of the rollable extranonce for downstream
///
/// # Returns
/// The number of bytes needed for the proxy's extranonce prefix
pub fn proxy_extranonce_prefix_len(
    channel_rollable_extranonce_size: usize,
    downstream_rollable_extranonce_size: usize,
) -> usize {
    channel_rollable_extranonce_size - downstream_rollable_extranonce_size
}

/// Messages used for coordinating shutdown across different components.
///
/// This enum defines the different types of shutdown signals that can be sent
/// through the broadcast channel to coordinate graceful shutdown of the translator.
#[derive(Debug, Clone)]
pub enum ShutdownMessage {
    /// Shutdown all components immediately
    ShutdownAll,
    /// Shutdown a specific downstream connection by ID
    DownstreamShutdown(DownstreamId),
    /// Reset channel manager state and shutdown downstreams due to upstream reconnection
    UpstreamFallback { tx: mpsc::Sender<()> },
}

#[derive(Debug)]
pub struct UpstreamEntry {
    pub addr: SocketAddr,
    pub authority_pubkey: Secp256k1PublicKey,
    pub tried_or_flagged: bool,
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn test_proxy_extranonce_prefix_len() {
        assert_eq!(proxy_extranonce_prefix_len(8, 4), 4);
        assert_eq!(proxy_extranonce_prefix_len(10, 6), 4);
        assert_eq!(proxy_extranonce_prefix_len(4, 4), 0);
    }

    #[test]
    fn test_shutdown_message_debug() {
        let msg1 = ShutdownMessage::ShutdownAll;
        let msg2 = ShutdownMessage::DownstreamShutdown(123);
        let (tx, _rx) = mpsc::channel(1);
        let msg3 = ShutdownMessage::UpstreamFallback { tx };

        // Test Debug implementation
        assert!(format!("{:?}", msg1).contains("ShutdownAll"));
        assert!(format!("{:?}", msg2).contains("DownstreamShutdown"));
        assert!(format!("{:?}", msg2).contains("123"));
        assert!(format!("{:?}", msg3).contains("UpstreamFallback"));
    }

    #[test]
    fn test_shutdown_message_clone() {
        let msg = ShutdownMessage::DownstreamShutdown(456);
        let cloned = msg.clone();

        match (msg, cloned) {
            (
                ShutdownMessage::DownstreamShutdown(id1),
                ShutdownMessage::DownstreamShutdown(id2),
            ) => {
                assert_eq!(id1, id2);
            }
            _ => panic!("Clone failed"),
        }
    }
}
