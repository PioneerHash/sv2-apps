pub(super) mod channel;
pub(super) mod data;
pub mod downstream;
mod message_handler;

use ehash_core::EhashPubkey;
use stratum_apps::{
    stratum_core::sv1_api::{client_to_server::Submit, utils::HexU32Be},
    utils::types::{ChannelId, DownstreamId},
};

/// Messages sent from downstream handling logic to the SV1 server.
///
/// This enum defines the types of messages that downstream connections can send
/// to the central SV1 server for processing and forwarding to upstream.
#[derive(Debug)]
pub enum DownstreamMessages {
    /// Represents a submitted share from a downstream miner,
    /// wrapped with the relevant channel ID.
    SubmitShares(SubmitShareWithChannelId),
    /// Request to open an extended mining channel for a downstream that just sent its first
    /// message.
    OpenChannel(DownstreamId), // downstream_id
    /// Register an ehash pubkey for a channel after mining.authorize is received.
    /// Sent from downstream to SV1 server, which forwards it to JDC via SV2.
    RegisterChannelPubkey(RegisterChannelPubkeyRequest),
}

/// Request to register an ehash pubkey for a channel.
///
/// This is sent from downstream after mining.authorize to associate the miner's
/// hpub with their SV2 channel for ehash token issuance.
#[derive(Debug, Clone)]
pub struct RegisterChannelPubkeyRequest {
    /// The SV2 channel ID to associate with this pubkey
    pub channel_id: ChannelId,
    /// The miner's ehash public key (parsed from mining.authorize username)
    pub pubkey: EhashPubkey,
}

/// A wrapper around a `mining.submit` message with additional channel information.
///
/// This struct contains all the necessary information to process a share submission
/// from an SV1 miner, including the share data itself and metadata needed for
/// proper routing and validation.
#[derive(Debug, Clone)]
pub struct SubmitShareWithChannelId {
    /// The SV2 channel ID this share belongs to
    pub channel_id: ChannelId,
    /// The downstream connection ID that submitted this share
    pub downstream_id: DownstreamId,
    /// The actual SV1 share submission data
    pub share: Submit<'static>,
    /// The complete extranonce used for this share
    pub extranonce: Vec<u8>,
    /// The length of the extranonce2 field
    pub extranonce2_len: usize,
    /// Optional version rolling mask for the share
    pub version_rolling_mask: Option<HexU32Be>,
    /// The version field from the job, used for validation
    pub job_version: Option<u32>,
}
