pub mod config;
pub mod error;
pub mod rebalancer;

pub use config::HashrateMultiplexerConfig;
pub use error::{MultiplexerDaemonError, Result};
pub use rebalancer::{Rebalancer, ReassignmentCommand, RebalancerMessage};

use multiplexer_core::MultiplexerCore;
use std::sync::Arc;
use tracing::info;

/// Main hashrate multiplexer daemon
pub struct HashrateMultiplexerDaemon {
    /// Configuration
    config: HashrateMultiplexerConfig,

    /// Multiplexer core
    core: Arc<MultiplexerCore>,
}

impl HashrateMultiplexerDaemon {
    /// Create a new daemon from configuration
    pub fn new(config: HashrateMultiplexerConfig) -> Result<Self> {
        let core = Arc::new(MultiplexerCore::new(config.multiplexer.clone())?);

        Ok(Self { config, core })
    }

    /// Run the daemon
    pub async fn run(self) -> Result<()> {
        info!("Starting Hashrate Multiplexer Daemon");
        info!("Downstream: {}:{}", self.config.downstream.address, self.config.downstream.port);
        info!("Upstreams: {}", self.config.multiplexer.upstreams.len());

        // Create communication channels
        let (reassignment_tx, reassignment_rx) = async_channel::unbounded();
        let (rebalancer_control_tx, rebalancer_control_rx) = async_channel::unbounded();

        // Spawn rebalancer task
        let rebalancer = Rebalancer::new(
            self.core.clone(),
            reassignment_tx,
            rebalancer_control_rx,
        );

        let rebalancer_handle = tokio::spawn(async move {
            if let Err(e) = rebalancer.run().await {
                tracing::error!("Rebalancer error: {}", e);
            }
        });

        // TODO: Spawn downstream listener task
        // TODO: Spawn upstream connector tasks
        // TODO: Spawn reassignment executor task
        // TODO: Spawn API server task (if enabled)

        info!("Hashrate Multiplexer Daemon running");
        info!("NOTE: This is a Phase 1 implementation - SV2 protocol integration pending");

        // For now, just wait for rebalancer
        // In full implementation, this would coordinate all tasks
        let _ = rebalancer_handle.await;

        info!("Hashrate Multiplexer Daemon stopped");
        Ok(())
    }

    /// Get reference to multiplexer core
    pub fn core(&self) -> &Arc<MultiplexerCore> {
        &self.core
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiplexer_core::{MultiplexerConfig, MultiplexedUpstream, ControllerConfig, FailoverConfig};
    use crate::config::*;

    fn create_test_config() -> HashrateMultiplexerConfig {
        HashrateMultiplexerConfig {
            multiplexer: MultiplexerConfig {
                upstreams: vec![
                    MultiplexedUpstream {
                        id: "pool_a".to_string(),
                        address: "pool-a.example.com".to_string(),
                        port: 3333,
                        authority_pubkey: "02abc123".to_string(),
                        percentage: 100.0,
                        priority: 1,
                        enabled: true,
                    },
                ],
                controller: ControllerConfig::default(),
                failover: FailoverConfig::default(),
            },
            downstream: DownstreamConfig {
                address: "0.0.0.0".to_string(),
                port: 34255,
                max_supported_version: 2,
                min_supported_version: 2,
                supported_extensions: vec![],
                required_extensions: vec![],
            },
            authority: AuthorityConfig {
                public_key: "02abc123".to_string(),
                secret_key: "secret123".to_string(),
            },
            api: None,
            metrics: None,
        }
    }

    #[test]
    fn test_daemon_creation() {
        let config = create_test_config();
        let daemon = HashrateMultiplexerDaemon::new(config);
        assert!(daemon.is_ok());
    }
}
