use crate::error::{MultiplexerDaemonError, Result};
use async_channel::{Receiver, Sender};
use multiplexer_core::{DownstreamId, MultiplexerCore};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{debug, info, warn};

/// Message types for rebalancer communication
#[derive(Debug, Clone)]
pub enum RebalancerMessage {
    /// Request immediate rebalancing
    ForceRebalance,

    /// Shutdown rebalancer
    Shutdown,
}

/// Reassignment command to be executed
#[derive(Debug, Clone)]
pub struct ReassignmentCommand {
    /// Downstream ID to reassign
    pub downstream_id: DownstreamId,

    /// Current upstream ID
    pub from_upstream_id: String,

    /// New upstream ID
    pub to_upstream_id: String,

    /// New extranonce prefix to assign
    pub new_extranonce_prefix: Vec<u8>,
}

/// Rebalancer manages the PID control loop and miner reassignments
pub struct Rebalancer {
    /// Multiplexer core
    core: Arc<MultiplexerCore>,

    /// Sender for reassignment commands
    reassignment_tx: Sender<ReassignmentCommand>,

    /// Receiver for control messages
    control_rx: Receiver<RebalancerMessage>,
}

impl Rebalancer {
    pub fn new(
        core: Arc<MultiplexerCore>,
        reassignment_tx: Sender<ReassignmentCommand>,
        control_rx: Receiver<RebalancerMessage>,
    ) -> Self {
        Self {
            core,
            reassignment_tx,
            control_rx,
        }
    }

    /// Run the rebalancing loop
    pub async fn run(self) -> Result<()> {
        info!("Rebalancer started");

        let mut interval = time::interval(self.core.get_evaluation_interval());
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Periodic rebalancing
                _ = interval.tick() => {
                    if let Err(e) = self.rebalance().await {
                        warn!("Rebalancing error: {}", e);
                    }
                }

                // Control messages
                msg = self.control_rx.recv() => {
                    match msg {
                        Ok(RebalancerMessage::ForceRebalance) => {
                            info!("Force rebalance requested");
                            if let Err(e) = self.rebalance().await {
                                warn!("Force rebalancing error: {}", e);
                            }
                        }
                        Ok(RebalancerMessage::Shutdown) => {
                            info!("Rebalancer shutting down");
                            break;
                        }
                        Err(_) => {
                            warn!("Control channel closed, shutting down rebalancer");
                            break;
                        }
                    }
                }
            }
        }

        info!("Rebalancer stopped");
        Ok(())
    }

    /// Perform one rebalancing cycle
    async fn rebalance(&self) -> Result<()> {
        // Calculate adjustments from PID controllers
        let adjustments = self.core.calculate_rebalancing_adjustments();

        if adjustments.is_empty() {
            debug!("No rebalancing needed (within deadband)");
            return Ok(());
        }

        info!("Rebalancing adjustments: {:?}", adjustments);

        // Find upstreams that need more miners (positive adjustment)
        let mut need_more: Vec<_> = adjustments
            .iter()
            .filter(|(_, adj)| **adj > 0)
            .collect();

        // Find upstreams with too many miners (negative adjustment)
        let mut have_extra: Vec<_> = adjustments
            .iter()
            .filter(|(_, adj)| **adj < 0)
            .collect();

        // Match them up and generate reassignment commands
        let min_interval = self.core.get_min_reassignment_interval();

        while let (Some((to_upstream, needed)), Some((from_upstream, available))) =
            (need_more.pop(), have_extra.pop())
        {
            let count = (*needed).min(available.abs()) as usize;

            // Select miners to reassign from the source upstream
            let candidates = self.select_miners_to_reassign(
                from_upstream,
                count,
                min_interval,
            ).await?;

            info!(
                "Reassigning {} miners from {} to {}",
                candidates.len(),
                from_upstream,
                to_upstream
            );

            // Generate reassignment commands
            for downstream_id in candidates {
                // Allocate new extranonce prefix from target upstream
                // TODO: This needs integration with upstream connection management
                // For now, we'll use a placeholder
                let new_extranonce_prefix = self.allocate_extranonce(to_upstream).await?;

                let command = ReassignmentCommand {
                    downstream_id,
                    from_upstream_id: (*from_upstream).clone(),
                    to_upstream_id: (*to_upstream).clone(),
                    new_extranonce_prefix,
                };

                // Send command to execution layer
                self.reassignment_tx
                    .send(command)
                    .await
                    .map_err(|_| MultiplexerDaemonError::ChannelSend)?;
            }
        }

        Ok(())
    }

    /// Select miners to reassign from an upstream
    async fn select_miners_to_reassign(
        &self,
        upstream_id: &str,
        count: usize,
        min_interval: Duration,
    ) -> Result<Vec<DownstreamId>> {
        // Get all miners assigned to this upstream
        let assignments = self.core.get_config()
            .upstreams
            .iter()
            .find(|u| u.id == upstream_id)
            .map(|_| {
                // Get downstream IDs from assignment manager
                // For now, we need to track this in the daemon layer
                // TODO: This will be populated by downstream connection tracking
                vec![]
            })
            .unwrap_or_default();

        // Filter miners that can be reassigned (respect minimum interval)
        let mut candidates: Vec<DownstreamId> = Vec::new();
        for &downstream_id in &assignments {
            if let Some(assignment) = self.core.get_assignment(downstream_id) {
                if assignment.can_reassign(min_interval.as_secs()) {
                    candidates.push(downstream_id);
                }
            }
        }

        // Random selection (simple approach for now)
        // TODO: Could implement hashrate-weighted selection
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        candidates.shuffle(&mut rng);

        // Return up to 'count' miners
        Ok(candidates.into_iter().take(count).collect())
    }

    /// Allocate a new extranonce prefix from an upstream
    async fn allocate_extranonce(&self, _upstream_id: &str) -> Result<Vec<u8>> {
        // TODO: This needs integration with upstream connection management
        // For now, return a placeholder
        // In reality, this would request a new extranonce prefix from the upstream pool
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let prefix: Vec<u8> = (0..4).map(|_| rng.gen()).collect();
        Ok(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiplexer_core::{MultiplexerConfig, MultiplexedUpstream, ControllerConfig, FailoverConfig};

    fn create_test_config() -> MultiplexerConfig {
        MultiplexerConfig {
            upstreams: vec![
                MultiplexedUpstream {
                    id: "pool_a".to_string(),
                    address: "pool-a.example.com".to_string(),
                    port: 3333,
                    authority_pubkey: "02abc123".to_string(),
                    percentage: 60.0,
                    priority: 1,
                    enabled: true,
                },
                MultiplexedUpstream {
                    id: "pool_b".to_string(),
                    address: "pool-b.example.com".to_string(),
                    port: 3333,
                    authority_pubkey: "03def456".to_string(),
                    percentage: 40.0,
                    priority: 2,
                    enabled: true,
                },
            ],
            controller: ControllerConfig::default(),
            failover: FailoverConfig::default(),
        }
    }

    #[tokio::test]
    async fn test_rebalancer_creation() {
        let config = create_test_config();
        let core = Arc::new(MultiplexerCore::new(config).unwrap());

        let (reassignment_tx, _reassignment_rx) = async_channel::unbounded();
        let (_control_tx, control_rx) = async_channel::unbounded();

        let _rebalancer = Rebalancer::new(core, reassignment_tx, control_rx);
    }
}
