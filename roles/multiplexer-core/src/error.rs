use thiserror::Error;

#[derive(Debug, Error)]
pub enum MultiplexerError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Invalid percentage total: {0:.2}% (must equal 100%)")]
    InvalidPercentageTotal(f64),

    #[error("Invalid percentage for upstream {upstream_id}: {percentage:.2}% (must be 0-100)")]
    InvalidPercentage {
        upstream_id: String,
        percentage: f64,
    },

    #[error("Duplicate upstream ID: {0}")]
    DuplicateUpstreamId(String),

    #[error("Duplicate priority: {0}")]
    DuplicatePriority(u8),

    #[error("No enabled upstreams")]
    NoEnabledUpstreams,

    #[error("Unknown upstream: {0}")]
    UnknownUpstream(String),

    #[error("Unknown downstream: {0}")]
    UnknownDownstream(u32),

    #[error("Upstream {0} not assigned to any downstream")]
    NoDownstreamsForUpstream(String),

    #[error("Invalid deadband: {0:.2}% (must be >= 0)")]
    InvalidDeadband(f64),

    #[error("Invalid evaluation interval: {0}s (must be > 0)")]
    InvalidEvaluationInterval(u64),

    #[error("Assignment error: {0}")]
    Assignment(String),

    #[error("Rebalancing error: {0}")]
    Rebalancing(String),
}

pub type Result<T> = std::result::Result<T, MultiplexerError>;
