use std::time::Instant;

/// Unique identifier for a downstream connection
pub type DownstreamId = u32;

/// Assignment of a downstream miner to an upstream
#[derive(Debug, Clone)]
pub struct Assignment {
    /// Downstream (miner) ID
    pub downstream_id: DownstreamId,

    /// Current upstream ID
    pub upstream_id: String,

    /// Channel ID for this connection
    pub channel_id: u32,

    /// Current extranonce prefix assigned
    pub extranonce_prefix: Vec<u8>,

    /// Timestamp of last reassignment
    pub last_reassignment: Instant,

    /// Estimated hashrate (if available)
    pub hashrate: Option<f64>,
}

impl Assignment {
    pub fn new(
        downstream_id: DownstreamId,
        upstream_id: String,
        channel_id: u32,
        extranonce_prefix: Vec<u8>,
    ) -> Self {
        Self {
            downstream_id,
            upstream_id,
            channel_id,
            extranonce_prefix,
            last_reassignment: Instant::now(),
            hashrate: None,
        }
    }

    /// Check if reassignment is allowed (respects minimum interval)
    pub fn can_reassign(&self, min_interval_secs: u64) -> bool {
        self.last_reassignment.elapsed().as_secs() >= min_interval_secs
    }

    /// Update assignment to new upstream
    pub fn update(&mut self, new_upstream_id: String, new_extranonce_prefix: Vec<u8>) {
        self.upstream_id = new_upstream_id;
        self.extranonce_prefix = new_extranonce_prefix;
        self.last_reassignment = Instant::now();
    }
}
