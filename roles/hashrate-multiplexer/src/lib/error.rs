use thiserror::Error;

#[derive(Debug, Error)]
pub enum MultiplexerDaemonError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Multiplexer core error: {0}")]
    Core(#[from] multiplexer_core::MultiplexerError),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Upstream connection error: {0}")]
    UpstreamConnection(String),

    #[error("Downstream connection error: {0}")]
    DownstreamConnection(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel send error")]
    ChannelSend,

    #[error("Channel receive error")]
    ChannelReceive,

    #[error("Unknown downstream: {0}")]
    UnknownDownstream(u32),

    #[error("Unknown upstream: {0}")]
    UnknownUpstream(String),
}

pub type Result<T> = std::result::Result<T, MultiplexerDaemonError>;
