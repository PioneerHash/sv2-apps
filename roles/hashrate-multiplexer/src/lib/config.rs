use multiplexer_core::{
    ControllerConfig, FailoverConfig, MultiplexedUpstream, MultiplexerConfig,
};
use serde::{Deserialize, Serialize};

/// Configuration for the hashrate multiplexer daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashrateMultiplexerConfig {
    /// Multiplexer core configuration
    #[serde(flatten)]
    pub multiplexer: MultiplexerConfig,

    /// Downstream (miner) listening configuration
    pub downstream: DownstreamConfig,

    /// Authority keys for Noise protocol
    pub authority: AuthorityConfig,

    /// Optional API configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiConfig>,

    /// Optional metrics configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsConfig>,
}

/// Downstream listening configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownstreamConfig {
    /// Listen address
    pub address: String,

    /// Listen port
    pub port: u16,

    /// Max supported SV2 version
    #[serde(default = "default_max_version")]
    pub max_supported_version: u16,

    /// Min supported SV2 version
    #[serde(default = "default_min_version")]
    pub min_supported_version: u16,

    /// Supported extensions
    #[serde(default)]
    pub supported_extensions: Vec<u16>,

    /// Required extensions
    #[serde(default)]
    pub required_extensions: Vec<u16>,
}

fn default_max_version() -> u16 {
    2
}

fn default_min_version() -> u16 {
    2
}

/// Authority keys for Noise protocol
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorityConfig {
    /// Public key (hex-encoded)
    pub public_key: String,

    /// Secret key (hex-encoded)
    pub secret_key: String,
}

/// API configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Enable API
    #[serde(default)]
    pub enabled: bool,

    /// API listen address
    #[serde(default = "default_api_address")]
    pub address: String,

    /// API listen port
    #[serde(default = "default_api_port")]
    pub port: u16,

    /// Optional API key for authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

fn default_api_address() -> String {
    "127.0.0.1".to_string()
}

fn default_api_port() -> u16 {
    3000
}

/// Metrics configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Enable Prometheus metrics
    #[serde(default)]
    pub enable_prometheus: bool,

    /// Prometheus listen address
    #[serde(default = "default_prometheus_address")]
    pub prometheus_address: String,

    /// Log metrics interval (seconds)
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: u64,
}

fn default_prometheus_address() -> String {
    "0.0.0.0:9090".to_string()
}

fn default_log_interval() -> u64 {
    60
}

impl HashrateMultiplexerConfig {
    /// Load configuration from file
    pub fn load_from_file(path: &str) -> anyhow::Result<Self> {
        let config_str = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&config_str)?;

        // Validate multiplexer config
        config.multiplexer.validate()?;

        Ok(config)
    }
}
