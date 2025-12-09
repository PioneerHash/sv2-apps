use crate::error::{MultiplexerError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Configuration for the hashrate multiplexer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiplexerConfig {
    /// List of upstream pools
    pub upstreams: Vec<MultiplexedUpstream>,

    /// Controller configuration
    pub controller: ControllerConfig,

    /// Failover configuration
    pub failover: FailoverConfig,
}

impl MultiplexerConfig {
    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Check for at least one upstream
        if self.upstreams.is_empty() {
            return Err(MultiplexerError::Config(
                "At least one upstream required".to_string(),
            ));
        }

        // Check for enabled upstreams
        if !self.upstreams.iter().any(|u| u.enabled) {
            return Err(MultiplexerError::NoEnabledUpstreams);
        }

        // Check for duplicate IDs
        let mut seen_ids = HashSet::new();
        for upstream in &self.upstreams {
            if !seen_ids.insert(&upstream.id) {
                return Err(MultiplexerError::DuplicateUpstreamId(
                    upstream.id.clone(),
                ));
            }
        }

        // Check for duplicate priorities
        let mut seen_priorities = HashSet::new();
        for upstream in &self.upstreams {
            if !seen_priorities.insert(upstream.priority) {
                return Err(MultiplexerError::DuplicatePriority(upstream.priority));
            }
        }

        // Validate individual upstreams
        for upstream in &self.upstreams {
            upstream.validate()?;
        }

        // Validate percentage total for enabled upstreams
        let total_percentage: f64 = self
            .upstreams
            .iter()
            .filter(|u| u.enabled)
            .map(|u| u.percentage)
            .sum();

        const TOLERANCE: f64 = 0.01;
        if (total_percentage - 100.0).abs() > TOLERANCE {
            return Err(MultiplexerError::InvalidPercentageTotal(total_percentage));
        }

        // Validate controller config
        self.controller.validate()?;

        Ok(())
    }

    /// Get sorted upstreams by priority (ascending)
    pub fn upstreams_by_priority(&self) -> Vec<MultiplexedUpstream> {
        let mut upstreams = self.upstreams.clone();
        upstreams.sort_by_key(|u| u.priority);
        upstreams
    }

    /// Get enabled upstreams only
    pub fn enabled_upstreams(&self) -> Vec<&MultiplexedUpstream> {
        self.upstreams.iter().filter(|u| u.enabled).collect()
    }
}

/// Upstream pool configuration with percentage allocation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiplexedUpstream {
    /// Unique identifier
    pub id: String,

    /// Pool address
    pub address: String,

    /// Pool port
    pub port: u16,

    /// SV2 authority public key (hex-encoded)
    pub authority_pubkey: String,

    /// Target percentage allocation (0-100)
    pub percentage: f64,

    /// Priority for initial assignment (lower = higher priority)
    pub priority: u8,

    /// Initially enabled?
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl MultiplexedUpstream {
    fn validate(&self) -> Result<()> {
        // Validate percentage range
        if self.percentage < 0.0 || self.percentage > 100.0 {
            return Err(MultiplexerError::InvalidPercentage {
                upstream_id: self.id.clone(),
                percentage: self.percentage,
            });
        }

        // Validate ID is not empty
        if self.id.is_empty() {
            return Err(MultiplexerError::Config("Upstream ID cannot be empty".to_string()));
        }

        // Validate address is not empty
        if self.address.is_empty() {
            return Err(MultiplexerError::Config(format!(
                "Address cannot be empty for upstream {}",
                self.id
            )));
        }

        Ok(())
    }
}

/// Controller configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerConfig {
    /// PID tuning model
    pub tuning_model: TuningModel,

    /// Custom PID parameters (overrides tuning_model if set)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_pid: Option<PidParams>,

    /// How often to run rebalancing loop (seconds)
    pub evaluation_interval_secs: u64,

    /// Deadband: don't rebalance if error < threshold (percentage points)
    pub deadband: f64,

    /// Minimum time between reassignments for same miner (seconds)
    pub min_reassignment_interval_secs: u64,

    /// Base measurement on accepted shares (not submitted)
    #[serde(default = "default_true")]
    pub measure_on_accepted: bool,
}

impl ControllerConfig {
    fn validate(&self) -> Result<()> {
        if self.deadband < 0.0 {
            return Err(MultiplexerError::InvalidDeadband(self.deadband));
        }

        if self.evaluation_interval_secs == 0 {
            return Err(MultiplexerError::InvalidEvaluationInterval(
                self.evaluation_interval_secs,
            ));
        }

        if let Some(ref params) = self.custom_pid {
            params.validate()?;
        }

        Ok(())
    }

    /// Get effective PID parameters (from model or custom)
    pub fn get_pid_params(&self) -> PidParams {
        if let Some(ref custom) = self.custom_pid {
            custom.clone()
        } else {
            self.tuning_model.get_params()
        }
    }
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            tuning_model: TuningModel::Balanced,
            custom_pid: None,
            evaluation_interval_secs: 30,
            deadband: 0.5,
            min_reassignment_interval_secs: 30,
            measure_on_accepted: true,
        }
    }
}

/// PID tuning model presets
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TuningModel {
    /// Aggressive: Fast convergence, may overshoot (Kp=1.0, Ki=0.1, Kd=0.05)
    Aggressive,

    /// Balanced: Moderate response (Kp=0.5, Ki=0.05, Kd=0.02)
    Balanced,

    /// Conservative: Slow, stable convergence (Kp=0.2, Ki=0.01, Kd=0.005)
    Conservative,

    /// Proportional only: Simple P controller (Kp=0.3)
    ProportionalOnly,
}

impl TuningModel {
    pub fn get_params(self) -> PidParams {
        match self {
            TuningModel::Aggressive => PidParams {
                kp: 1.0,
                ki: 0.1,
                kd: 0.05,
            },
            TuningModel::Balanced => PidParams {
                kp: 0.5,
                ki: 0.05,
                kd: 0.02,
            },
            TuningModel::Conservative => PidParams {
                kp: 0.2,
                ki: 0.01,
                kd: 0.005,
            },
            TuningModel::ProportionalOnly => PidParams {
                kp: 0.3,
                ki: 0.0,
                kd: 0.0,
            },
        }
    }
}

/// PID controller parameters
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PidParams {
    /// Proportional gain
    pub kp: f64,

    /// Integral gain
    pub ki: f64,

    /// Derivative gain
    pub kd: f64,
}

impl PidParams {
    pub fn validate(&self) -> Result<()> {
        if self.kp < 0.0 || self.ki < 0.0 || self.kd < 0.0 {
            return Err(MultiplexerError::Config(
                "PID parameters must be non-negative".to_string(),
            ));
        }
        Ok(())
    }
}

/// Failover configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverConfig {
    /// Automatically redistribute percentages when upstream fails
    #[serde(default = "default_true")]
    pub auto_redistribute: bool,

    /// Automatically restore percentages when upstream recovers
    #[serde(default = "default_true")]
    pub auto_restore: bool,

    /// Health check interval (seconds)
    #[serde(default = "default_health_check_interval")]
    pub health_check_interval_secs: u64,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            auto_redistribute: true,
            auto_restore: true,
            health_check_interval_secs: 30,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_health_check_interval() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_upstream(id: &str, percentage: f64, priority: u8) -> MultiplexedUpstream {
        MultiplexedUpstream {
            id: id.to_string(),
            address: "pool.example.com".to_string(),
            port: 3333,
            authority_pubkey: "02abc123".to_string(),
            percentage,
            priority,
            enabled: true,
        }
    }

    #[test]
    fn test_valid_config() {
        let config = MultiplexerConfig {
            upstreams: vec![
                create_test_upstream("pool_a", 60.0, 1),
                create_test_upstream("pool_b", 30.0, 2),
                create_test_upstream("pool_c", 10.0, 3),
            ],
            controller: ControllerConfig::default(),
            failover: FailoverConfig::default(),
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_percentage_total() {
        let config = MultiplexerConfig {
            upstreams: vec![
                create_test_upstream("pool_a", 60.0, 1),
                create_test_upstream("pool_b", 30.0, 2),
            ],
            controller: ControllerConfig::default(),
            failover: FailoverConfig::default(),
        };

        let result = config.validate();
        assert!(matches!(
            result,
            Err(MultiplexerError::InvalidPercentageTotal(_))
        ));
    }

    #[test]
    fn test_duplicate_upstream_id() {
        let config = MultiplexerConfig {
            upstreams: vec![
                create_test_upstream("pool_a", 50.0, 1),
                create_test_upstream("pool_a", 50.0, 2),
            ],
            controller: ControllerConfig::default(),
            failover: FailoverConfig::default(),
        };

        let result = config.validate();
        assert!(matches!(
            result,
            Err(MultiplexerError::DuplicateUpstreamId(_))
        ));
    }

    #[test]
    fn test_duplicate_priority() {
        let config = MultiplexerConfig {
            upstreams: vec![
                create_test_upstream("pool_a", 50.0, 1),
                create_test_upstream("pool_b", 50.0, 1),
            ],
            controller: ControllerConfig::default(),
            failover: FailoverConfig::default(),
        };

        let result = config.validate();
        assert!(matches!(
            result,
            Err(MultiplexerError::DuplicatePriority(_))
        ));
    }

    #[test]
    fn test_upstreams_by_priority() {
        let config = MultiplexerConfig {
            upstreams: vec![
                create_test_upstream("pool_c", 10.0, 3),
                create_test_upstream("pool_a", 60.0, 1),
                create_test_upstream("pool_b", 30.0, 2),
            ],
            controller: ControllerConfig::default(),
            failover: FailoverConfig::default(),
        };

        let sorted = config.upstreams_by_priority();
        assert_eq!(sorted[0].id, "pool_a");
        assert_eq!(sorted[1].id, "pool_b");
        assert_eq!(sorted[2].id, "pool_c");
    }

    #[test]
    fn test_tuning_model_params() {
        let aggressive = TuningModel::Aggressive.get_params();
        assert_eq!(aggressive.kp, 1.0);
        assert_eq!(aggressive.ki, 0.1);
        assert_eq!(aggressive.kd, 0.05);

        let balanced = TuningModel::Balanced.get_params();
        assert_eq!(balanced.kp, 0.5);

        let conservative = TuningModel::Conservative.get_params();
        assert_eq!(conservative.kp, 0.2);

        let p_only = TuningModel::ProportionalOnly.get_params();
        assert_eq!(p_only.kp, 0.3);
        assert_eq!(p_only.ki, 0.0);
        assert_eq!(p_only.kd, 0.0);
    }
}
