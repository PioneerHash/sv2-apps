pub mod assignment;
pub mod config;
pub mod controller;
pub mod error;
pub mod metrics;
pub mod upstream;

pub use assignment::{Assignment, AssignmentManager, DownstreamId};
pub use config::{
    ControllerConfig, FailoverConfig, MultiplexedUpstream, MultiplexerConfig, PidParams,
    TuningModel,
};
pub use controller::PidController;
pub use error::{MultiplexerError, Result};
pub use metrics::{MetricsCollector, UpstreamMetrics, UpstreamMetricsSnapshot};
pub use upstream::UpstreamRegistry;

use hashbrown::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Core multiplexer state and logic
pub struct MultiplexerCore {
    /// Configuration
    config: Arc<RwLock<MultiplexerConfig>>,

    /// Assignment manager
    assignment_manager: Arc<RwLock<AssignmentManager>>,

    /// PID controllers (one per upstream)
    controllers: Arc<RwLock<HashMap<String, PidController>>>,

    /// Metrics collector
    metrics: Arc<MetricsCollector>,

    /// Upstream registry
    upstream_registry: Arc<RwLock<UpstreamRegistry>>,
}

impl MultiplexerCore {
    /// Create a new multiplexer core
    pub fn new(config: MultiplexerConfig) -> Result<Self> {
        // Validate configuration
        config.validate()?;

        let upstreams = config.upstreams.clone();
        let upstream_ids: Vec<String> = upstreams.iter().map(|u| u.id.clone()).collect();

        // Create PID controllers for each upstream
        let pid_params = config.controller.get_pid_params();
        let mut controllers = HashMap::new();
        for upstream in &upstreams {
            controllers.insert(upstream.id.clone(), PidController::new(pid_params));
        }

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            assignment_manager: Arc::new(RwLock::new(AssignmentManager::new(upstreams.clone()))),
            controllers: Arc::new(RwLock::new(controllers)),
            metrics: Arc::new(MetricsCollector::new(upstream_ids.clone())),
            upstream_registry: Arc::new(RwLock::new(UpstreamRegistry::new(upstreams))),
        })
    }

    /// Assign a new downstream miner
    pub fn assign_new_downstream(
        &self,
        downstream_id: DownstreamId,
        channel_id: u32,
        extranonce_prefix: Vec<u8>,
    ) -> Result<String> {
        self.assignment_manager
            .write()
            .unwrap()
            .assign_new_miner(downstream_id, channel_id, extranonce_prefix)
    }

    /// Get assignment for a downstream
    pub fn get_assignment(&self, downstream_id: DownstreamId) -> Option<Assignment> {
        self.assignment_manager
            .read()
            .unwrap()
            .get_assignment(downstream_id)
    }

    /// Remove downstream (disconnected)
    pub fn remove_downstream(&self, downstream_id: DownstreamId) {
        self.assignment_manager
            .write()
            .unwrap()
            .remove_assignment(downstream_id);
    }

    /// Record share submission
    pub fn record_share_submitted(&self, upstream_id: &str) {
        self.metrics.record_share_submitted(upstream_id);
    }

    /// Record share acceptance
    pub fn record_share_accepted(&self, upstream_id: &str) {
        self.metrics.record_share_accepted(upstream_id);
    }

    /// Record share rejection
    pub fn record_share_rejected(&self, upstream_id: &str) {
        self.metrics.record_share_rejected(upstream_id);
    }

    /// Get current distribution (actual percentages)
    pub fn get_current_distribution(&self, use_accepted: bool) -> HashMap<String, f64> {
        if use_accepted {
            self.metrics.calculate_distribution_from_accepted()
        } else {
            self.metrics.calculate_distribution_from_submitted()
        }
    }

    /// Calculate rebalancing adjustments using PID controllers
    ///
    /// Returns map of upstream_id -> number of miners to add/remove
    /// Positive value = need more miners, negative = too many miners
    pub fn calculate_rebalancing_adjustments(&self) -> HashMap<String, i32> {
        let config = self.config.read().unwrap();
        let measure_on_accepted = config.controller.measure_on_accepted;
        let deadband = config.controller.deadband;

        // Get current distribution
        let actual_distribution = self.get_current_distribution(measure_on_accepted);

        // Get total miners
        let total_miners = self.assignment_manager.read().unwrap().total_miners();

        // Calculate adjustments per upstream
        let mut adjustments = HashMap::new();
        let mut controllers = self.controllers.write().unwrap();

        for upstream in &config.upstreams {
            if !upstream.enabled {
                continue;
            }

            let target = upstream.percentage;
            let actual = actual_distribution
                .get(&upstream.id)
                .copied()
                .unwrap_or(0.0);

            let error = (target - actual).abs();

            // Apply deadband - skip if within threshold
            if error < deadband {
                continue;
            }

            // Get PID controller for this upstream
            let controller = controllers
                .get_mut(&upstream.id)
                .expect("Controller should exist for upstream");

            // Calculate PID output (in percentage points)
            let adjustment_pct = controller.update(target, actual);

            // Convert to miner count
            let miner_adjustment = if total_miners > 0 {
                (adjustment_pct / 100.0 * total_miners as f64).round() as i32
            } else {
                0
            };

            if miner_adjustment != 0 {
                adjustments.insert(upstream.id.clone(), miner_adjustment);
            }
        }

        adjustments
    }

    /// Get metrics snapshot
    pub fn get_metrics_snapshot(&self) -> Vec<UpstreamMetricsSnapshot> {
        let config = self.config.read().unwrap();
        let measure_on_accepted = config.controller.measure_on_accepted;

        // Build target percentages map
        let targets: HashMap<String, f64> = config
            .upstreams
            .iter()
            .map(|u| (u.id.clone(), u.percentage))
            .collect();

        // Get actual distribution
        let actual_distribution = self.get_current_distribution(measure_on_accepted);

        // Count miners per upstream
        let assignment_manager = self.assignment_manager.read().unwrap();
        let mut miners_per_upstream = HashMap::new();
        for upstream in &config.upstreams {
            let count = assignment_manager
                .get_downstreams_for_upstream(&upstream.id)
                .len();
            miners_per_upstream.insert(upstream.id.clone(), count);
        }

        self.metrics
            .get_snapshot(&targets, &actual_distribution, &miners_per_upstream)
    }

    /// Update PID tuning model
    pub fn set_tuning_model(&self, model: TuningModel) -> Result<()> {
        let params = model.get_params();
        let mut controllers = self.controllers.write().unwrap();

        for controller in controllers.values_mut() {
            controller.set_params(params);
        }

        let mut config = self.config.write().unwrap();
        config.controller.tuning_model = model;
        config.controller.custom_pid = None;

        Ok(())
    }

    /// Set custom PID parameters
    pub fn set_custom_pid_params(&self, params: PidParams) -> Result<()> {
        params.validate()?;

        let mut controllers = self.controllers.write().unwrap();
        for controller in controllers.values_mut() {
            controller.set_params(params);
        }

        let mut config = self.config.write().unwrap();
        config.controller.custom_pid = Some(params);

        Ok(())
    }

    /// Update deadband
    pub fn set_deadband(&self, deadband: f64) -> Result<()> {
        if deadband < 0.0 {
            return Err(MultiplexerError::InvalidDeadband(deadband));
        }

        let mut config = self.config.write().unwrap();
        config.controller.deadband = deadband;

        Ok(())
    }

    /// Update evaluation interval
    pub fn set_evaluation_interval(&self, interval_secs: u64) -> Result<()> {
        if interval_secs == 0 {
            return Err(MultiplexerError::InvalidEvaluationInterval(interval_secs));
        }

        let mut config = self.config.write().unwrap();
        config.controller.evaluation_interval_secs = interval_secs;

        Ok(())
    }

    /// Get current configuration (clone)
    pub fn get_config(&self) -> MultiplexerConfig {
        self.config.read().unwrap().clone()
    }

    /// Get evaluation interval as Duration
    pub fn get_evaluation_interval(&self) -> Duration {
        let config = self.config.read().unwrap();
        Duration::from_secs(config.controller.evaluation_interval_secs)
    }

    /// Get minimum reassignment interval as Duration
    pub fn get_min_reassignment_interval(&self) -> Duration {
        let config = self.config.read().unwrap();
        Duration::from_secs(config.controller.min_reassignment_interval_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_create_multiplexer_core() {
        let config = create_test_config();
        let core = MultiplexerCore::new(config);
        assert!(core.is_ok());
    }

    #[test]
    fn test_assign_downstream() {
        let config = create_test_config();
        let core = MultiplexerCore::new(config).unwrap();

        let upstream_id = core
            .assign_new_downstream(1, 1, vec![0x01])
            .unwrap();
        assert_eq!(upstream_id, "pool_a"); // Should assign to highest priority
    }

    #[test]
    fn test_record_shares() {
        let config = create_test_config();
        let core = MultiplexerCore::new(config).unwrap();

        core.record_share_submitted("pool_a");
        core.record_share_accepted("pool_a");

        let distribution = core.get_current_distribution(true);
        assert_eq!(distribution.get("pool_a").copied().unwrap(), 100.0);
    }

    #[test]
    fn test_set_tuning_model() {
        let config = create_test_config();
        let core = MultiplexerCore::new(config).unwrap();

        assert!(core.set_tuning_model(TuningModel::Aggressive).is_ok());

        let config = core.get_config();
        assert_eq!(config.controller.tuning_model, TuningModel::Aggressive);
    }

    #[test]
    fn test_set_deadband() {
        let config = create_test_config();
        let core = MultiplexerCore::new(config).unwrap();

        assert!(core.set_deadband(1.5).is_ok());
        let config = core.get_config();
        assert_eq!(config.controller.deadband, 1.5);
    }

    #[test]
    fn test_invalid_deadband() {
        let config = create_test_config();
        let core = MultiplexerCore::new(config).unwrap();

        let result = core.set_deadband(-1.0);
        assert!(matches!(result, Err(MultiplexerError::InvalidDeadband(_))));
    }
}
