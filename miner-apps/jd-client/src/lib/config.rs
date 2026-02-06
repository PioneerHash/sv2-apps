use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};
use stratum_apps::{
    config_helpers::{opt_path_from_toml, CoinbaseRewardScript},
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    stratum_core::bitcoin::{Amount, TxOut},
    tp_type::TemplateProviderType,
    utils::types::{SharesBatchSize, SharesPerMinute},
};

/// ehash-mint connection configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct EhashMintConfig {
    /// Whether ehash-mint integration is enabled
    #[serde(default)]
    pub enabled: bool,
    /// Address of the ehash-mint Sv2 listener
    pub address: Option<String>,
    /// Port of the ehash-mint Sv2 listener
    pub port: Option<u16>,
    /// Authority public key of the ehash-mint (for Noise authentication)
    pub authority_pubkey: Option<Secp256k1PublicKey>,
    /// Optional path to fallback log file when connection is down and buffer is full.
    /// Defaults to /tmp/ehash-reports-fallback.log
    pub fallback_log_path: Option<PathBuf>,
    /// Timeout in seconds for receiving RegisterChannelPubkey after channel open.
    /// If exceeded, the downstream is disconnected. Defaults to 5 seconds.
    #[serde(default)]
    pub pubkey_timeout_secs: Option<u64>,
}

impl Default for EhashMintConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            address: None,
            port: None,
            authority_pubkey: None,
            fallback_log_path: None,
            pubkey_timeout_secs: Some(5),
        }
    }
}

impl EhashMintConfig {
    /// Returns the socket address for the ehash-mint if configured.
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        match (self.enabled, &self.address, self.port) {
            (true, Some(addr), Some(port)) => {
                let addr_str = format!("{}:{}", addr, port);
                addr_str.parse().ok()
            }
            _ => None,
        }
    }

    /// Check if ehash-mint integration is properly configured.
    pub fn is_configured(&self) -> bool {
        self.enabled
            && self.address.is_some()
            && self.port.is_some()
            && self.authority_pubkey.is_some()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct JobDeclaratorClientConfig {
    // The address on which the JDC will listen for incoming connections when acting as an
    // upstream.
    listening_address: SocketAddr,
    // The maximum supported SV2 protocol version.
    max_supported_version: u16,
    // The minimum supported SV2 protocol version.
    min_supported_version: u16,
    // The public key used by this JDC for noise encryption.
    authority_public_key: Secp256k1PublicKey,
    /// The secret key used by this JDC for noise encryption.
    authority_secret_key: Secp256k1SecretKey,
    /// The validity period (in seconds) for the certificate used in noise.
    cert_validity_sec: u64,
    /// The template provider type that this JDC will use.
    template_provider_type: TemplateProviderType,
    /// A list of upstream Job Declarator Servers (JDS) that this JDC can connect to.
    /// JDC can fallover between these upstreams.
    upstreams: Vec<Upstream>,
    /// This is only used during solo-mining.
    pub coinbase_reward_script: CoinbaseRewardScript,
    /// A signature string identifying this JDC instance.
    jdc_signature: String,
    /// The path to the log file where JDC will write logs.
    #[serde(default, deserialize_with = "opt_path_from_toml")]
    log_file: Option<PathBuf>,
    /// User Identity
    user_identity: String,
    /// Shares per minute
    shares_per_minute: SharesPerMinute,
    /// share batch size
    share_batch_size: SharesBatchSize,
    /// JDC mode: FullTemplate or CoinbaseOnly
    #[serde(deserialize_with = "deserialize_jdc_mode", default)]
    pub mode: ConfigJDCMode,
    /// Protocol extensions that the JDC supports (will accept if requested by downstream clients).
    supported_extensions: Vec<u16>,
    /// Protocol extensions that the JDC requires (downstream clients must support these).
    required_extensions: Vec<u16>,
    /// Optional monitoring server bind address
    #[serde(default)]
    monitoring_address: Option<SocketAddr>,
    /// ehash-mint configuration for ecash integration
    #[serde(default)]
    ehash_mint: EhashMintConfig,
}

impl JobDeclaratorClientConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        listening_address: SocketAddr,
        protocol_config: ProtocolConfig,
        user_identity: String,
        shares_per_minute: SharesPerMinute,
        share_batch_size: SharesBatchSize,
        pool_config: PoolConfig,
        cert_validity_sec: u64,
        template_provider_type: TemplateProviderType,
        upstreams: Vec<Upstream>,
        jdc_signature: String,
        jdc_mode: Option<String>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> Self {
        Self {
            listening_address,
            max_supported_version: protocol_config.max_supported_version,
            min_supported_version: protocol_config.min_supported_version,
            authority_public_key: pool_config.authority_public_key,
            authority_secret_key: pool_config.authority_secret_key,
            cert_validity_sec,
            template_provider_type,
            upstreams,
            coinbase_reward_script: protocol_config.coinbase_reward_script,
            jdc_signature,
            log_file: None,
            user_identity,
            shares_per_minute,
            share_batch_size,
            mode: jdc_mode
                .map(|s| s.parse::<ConfigJDCMode>().unwrap_or_default())
                .unwrap_or_default(),
            supported_extensions,
            required_extensions,
            monitoring_address: None,
            ehash_mint: EhashMintConfig::default(),
        }
    }

    /// Returns the monitoring server bind address (if enabled)
    pub fn monitoring_address(&self) -> Option<SocketAddr> {
        self.monitoring_address
    }

    /// Returns the ehash-mint configuration.
    pub fn ehash_mint(&self) -> &EhashMintConfig {
        &self.ehash_mint
    }

    /// Returns the timeout in seconds for receiving RegisterChannelPubkey.
    pub fn ehash_pubkey_timeout_secs(&self) -> Option<u64> {
        self.ehash_mint.pubkey_timeout_secs
    }

    /// Returns the listening address of the Job Declartor Client.
    pub fn listening_address(&self) -> &SocketAddr {
        &self.listening_address
    }

    /// Returns the list of upstreams.
    ///
    /// JDC will try to fallback to the next upstream in case of failure of the current one.
    pub fn upstreams(&self) -> &Vec<Upstream> {
        &self.upstreams
    }

    /// Returns the authority public key.
    pub fn authority_public_key(&self) -> &Secp256k1PublicKey {
        &self.authority_public_key
    }

    /// Returns the authority secret key.
    pub fn authority_secret_key(&self) -> &Secp256k1SecretKey {
        &self.authority_secret_key
    }

    /// Returns the certificate validity in seconds.
    pub fn cert_validity_sec(&self) -> u64 {
        self.cert_validity_sec
    }

    /// Returns the template provider type.
    pub fn template_provider_type(&self) -> &TemplateProviderType {
        &self.template_provider_type
    }

    /// Returns the minimum supported version.
    pub fn min_supported_version(&self) -> u16 {
        self.min_supported_version
    }

    /// Returns the maximum supported version.
    pub fn max_supported_version(&self) -> u16 {
        self.max_supported_version
    }

    /// Returns the JDC signature.
    pub fn jdc_signature(&self) -> &str {
        &self.jdc_signature
    }

    pub fn get_txout(&self) -> TxOut {
        TxOut {
            value: Amount::from_sat(0),
            script_pubkey: self.coinbase_reward_script.script_pubkey().to_owned(),
        }
    }

    pub fn log_file(&self) -> Option<&Path> {
        self.log_file.as_deref()
    }
    pub fn set_log_file(&mut self, log_file: Option<PathBuf>) {
        if let Some(log_file) = log_file {
            self.log_file = Some(log_file);
        }
    }
    pub fn user_identity(&self) -> &str {
        &self.user_identity
    }

    pub fn shares_per_minute(&self) -> SharesPerMinute {
        self.shares_per_minute
    }

    pub fn share_batch_size(&self) -> SharesBatchSize {
        self.share_batch_size
    }

    /// Returns the supported extensions.
    pub fn supported_extensions(&self) -> &[u16] {
        &self.supported_extensions
    }

    /// Returns the required extensions.
    pub fn required_extensions(&self) -> &[u16] {
        &self.required_extensions
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum ConfigJDCMode {
    #[default]
    FullTemplate,
    CoinbaseOnly,
}

impl std::str::FromStr for ConfigJDCMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "COINBASEONLY" => Ok(ConfigJDCMode::CoinbaseOnly),
            _ => Ok(ConfigJDCMode::FullTemplate),
        }
    }
}

fn deserialize_jdc_mode<'de, D>(deserializer: D) -> Result<ConfigJDCMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    Ok(ConfigJDCMode::from_str(&s).unwrap_or_default())
}

/// Represents pool specific encryption keys.
pub struct PoolConfig {
    authority_public_key: Secp256k1PublicKey,
    authority_secret_key: Secp256k1SecretKey,
}

impl PoolConfig {
    /// Creates a new instance of [`PoolConfig`].
    pub fn new(
        authority_public_key: Secp256k1PublicKey,
        authority_secret_key: Secp256k1SecretKey,
    ) -> Self {
        Self {
            authority_public_key,
            authority_secret_key,
        }
    }
}

/// Represent protocol versioning the JDC supports.
pub struct ProtocolConfig {
    // The maximum supported SV2 protocol version.
    max_supported_version: u16,
    // The minimum supported SV2 protocol version.
    min_supported_version: u16,
    // A coinbase output to be included in block templates.
    coinbase_reward_script: CoinbaseRewardScript,
}

impl ProtocolConfig {
    // Creates a new instance of [`ProtocolConfig`].
    pub fn new(
        max_supported_version: u16,
        min_supported_version: u16,
        coinbase_reward_script: CoinbaseRewardScript,
    ) -> Self {
        Self {
            max_supported_version,
            min_supported_version,
            coinbase_reward_script,
        }
    }
}

/// Represents necessary fields required to connect to JDS
#[derive(Debug, Deserialize, Clone)]
pub struct Upstream {
    // The public key of the upstream pool's authority for authentication.
    pub authority_pubkey: Secp256k1PublicKey,
    // The address of the upstream pool's main server.
    pub pool_address: String,
    pub pool_port: u16,
    // The network address of the JDS.
    pub jds_address: String,
    pub jds_port: u16,
}

impl Upstream {
    /// Creates a new instance of [`Upstream`].
    pub fn new(
        authority_pubkey: Secp256k1PublicKey,
        pool_address: String,
        pool_port: u16,
        jds_address: String,
        jds_port: u16,
    ) -> Self {
        Self {
            authority_pubkey,
            pool_address,
            pool_port,
            jds_address,
            jds_port,
        }
    }
}
