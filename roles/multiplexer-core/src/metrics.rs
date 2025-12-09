use hashbrown::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Metrics for a single upstream
#[derive(Debug)]
pub struct UpstreamMetrics {
    /// Total shares submitted to this upstream
    pub shares_submitted: Arc<AtomicU64>,

    /// Total shares accepted by this upstream
    pub shares_accepted: Arc<AtomicU64>,

    /// Total shares rejected by this upstream
    pub shares_rejected: Arc<AtomicU64>,
}

impl UpstreamMetrics {
    pub fn new() -> Self {
        Self {
            shares_submitted: Arc::new(AtomicU64::new(0)),
            shares_accepted: Arc::new(AtomicU64::new(0)),
            shares_rejected: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Increment submitted shares
    pub fn inc_submitted(&self) {
        self.shares_submitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment accepted shares
    pub fn inc_accepted(&self) {
        self.shares_accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment rejected shares
    pub fn inc_rejected(&self) {
        self.shares_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Get current counts
    pub fn get_counts(&self) -> (u64, u64, u64) {
        (
            self.shares_submitted.load(Ordering::Relaxed),
            self.shares_accepted.load(Ordering::Relaxed),
            self.shares_rejected.load(Ordering::Relaxed),
        )
    }

    /// Calculate acceptance rate (0.0 - 100.0)
    pub fn acceptance_rate(&self) -> f64 {
        let submitted = self.shares_submitted.load(Ordering::Relaxed);
        if submitted == 0 {
            return 100.0;
        }

        let accepted = self.shares_accepted.load(Ordering::Relaxed);
        (accepted as f64 / submitted as f64) * 100.0
    }

    /// Reset all counters
    pub fn reset(&self) {
        self.shares_submitted.store(0, Ordering::Relaxed);
        self.shares_accepted.store(0, Ordering::Relaxed);
        self.shares_rejected.store(0, Ordering::Relaxed);
    }
}

impl Default for UpstreamMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of upstream metrics at a point in time
#[derive(Debug, Clone)]
pub struct UpstreamMetricsSnapshot {
    pub upstream_id: String,
    pub target_percentage: f64,
    pub actual_percentage: f64,
    pub shares_submitted: u64,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub acceptance_rate: f64,
    pub num_miners: usize,
}

/// Metrics collector for the multiplexer
#[derive(Debug)]
pub struct MetricsCollector {
    /// Per-upstream metrics
    upstream_metrics: HashMap<String, UpstreamMetrics>,
}

impl MetricsCollector {
    pub fn new(upstream_ids: Vec<String>) -> Self {
        let mut upstream_metrics = HashMap::new();
        for id in upstream_ids {
            upstream_metrics.insert(id, UpstreamMetrics::new());
        }

        Self { upstream_metrics }
    }

    /// Get metrics for an upstream
    pub fn get_upstream_metrics(&self, upstream_id: &str) -> Option<&UpstreamMetrics> {
        self.upstream_metrics.get(upstream_id)
    }

    /// Record share submission
    pub fn record_share_submitted(&self, upstream_id: &str) {
        if let Some(metrics) = self.upstream_metrics.get(upstream_id) {
            metrics.inc_submitted();
        }
    }

    /// Record share acceptance
    pub fn record_share_accepted(&self, upstream_id: &str) {
        if let Some(metrics) = self.upstream_metrics.get(upstream_id) {
            metrics.inc_accepted();
        }
    }

    /// Record share rejection
    pub fn record_share_rejected(&self, upstream_id: &str) {
        if let Some(metrics) = self.upstream_metrics.get(upstream_id) {
            metrics.inc_rejected();
        }
    }

    /// Calculate distribution based on accepted shares
    pub fn calculate_distribution_from_accepted(&self) -> HashMap<String, f64> {
        // Sum total accepted shares across all upstreams
        let total_accepted: u64 = self
            .upstream_metrics
            .values()
            .map(|m| m.shares_accepted.load(Ordering::Relaxed))
            .sum();

        if total_accepted == 0 {
            return HashMap::new();
        }

        // Calculate percentage for each upstream
        self.upstream_metrics
            .iter()
            .map(|(id, metrics)| {
                let accepted = metrics.shares_accepted.load(Ordering::Relaxed);
                let percentage = (accepted as f64 / total_accepted as f64) * 100.0;
                (id.clone(), percentage)
            })
            .collect()
    }

    /// Calculate distribution based on submitted shares
    pub fn calculate_distribution_from_submitted(&self) -> HashMap<String, f64> {
        // Sum total submitted shares across all upstreams
        let total_submitted: u64 = self
            .upstream_metrics
            .values()
            .map(|m| m.shares_submitted.load(Ordering::Relaxed))
            .sum();

        if total_submitted == 0 {
            return HashMap::new();
        }

        // Calculate percentage for each upstream
        self.upstream_metrics
            .iter()
            .map(|(id, metrics)| {
                let submitted = metrics.shares_submitted.load(Ordering::Relaxed);
                let percentage = (submitted as f64 / total_submitted as f64) * 100.0;
                (id.clone(), percentage)
            })
            .collect()
    }

    /// Get snapshot of all metrics
    pub fn get_snapshot(
        &self,
        targets: &HashMap<String, f64>,
        actual_distribution: &HashMap<String, f64>,
        miners_per_upstream: &HashMap<String, usize>,
    ) -> Vec<UpstreamMetricsSnapshot> {
        self.upstream_metrics
            .iter()
            .map(|(id, metrics)| {
                let (submitted, accepted, rejected) = metrics.get_counts();
                UpstreamMetricsSnapshot {
                    upstream_id: id.clone(),
                    target_percentage: targets.get(id).copied().unwrap_or(0.0),
                    actual_percentage: actual_distribution.get(id).copied().unwrap_or(0.0),
                    shares_submitted: submitted,
                    shares_accepted: accepted,
                    shares_rejected: rejected,
                    acceptance_rate: metrics.acceptance_rate(),
                    num_miners: miners_per_upstream.get(id).copied().unwrap_or(0),
                }
            })
            .collect()
    }

    /// Reset all metrics
    pub fn reset_all(&self) {
        for metrics in self.upstream_metrics.values() {
            metrics.reset();
        }
    }

    /// Add metrics for a new upstream
    pub fn add_upstream(&mut self, upstream_id: String) {
        self.upstream_metrics
            .insert(upstream_id, UpstreamMetrics::new());
    }

    /// Remove metrics for an upstream
    pub fn remove_upstream(&mut self, upstream_id: &str) {
        self.upstream_metrics.remove(upstream_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upstream_metrics() {
        let metrics = UpstreamMetrics::new();

        metrics.inc_submitted();
        metrics.inc_submitted();
        metrics.inc_submitted();

        metrics.inc_accepted();
        metrics.inc_accepted();

        metrics.inc_rejected();

        let (submitted, accepted, rejected) = metrics.get_counts();
        assert_eq!(submitted, 3);
        assert_eq!(accepted, 2);
        assert_eq!(rejected, 1);

        let acceptance_rate = metrics.acceptance_rate();
        assert!((acceptance_rate - 66.666).abs() < 0.01);
    }

    #[test]
    fn test_acceptance_rate_zero_submitted() {
        let metrics = UpstreamMetrics::new();
        assert_eq!(metrics.acceptance_rate(), 100.0);
    }

    #[test]
    fn test_metrics_collector() {
        let collector = MetricsCollector::new(vec![
            "pool_a".to_string(),
            "pool_b".to_string(),
        ]);

        // Record some shares
        collector.record_share_submitted("pool_a");
        collector.record_share_submitted("pool_a");
        collector.record_share_accepted("pool_a");
        collector.record_share_accepted("pool_a");

        collector.record_share_submitted("pool_b");
        collector.record_share_accepted("pool_b");

        // Calculate distribution
        let distribution = collector.calculate_distribution_from_accepted();

        // pool_a: 2 accepted, pool_b: 1 accepted
        // pool_a: 2/3 * 100 = 66.67%
        // pool_b: 1/3 * 100 = 33.33%
        let pool_a_pct = distribution.get("pool_a").unwrap();
        let pool_b_pct = distribution.get("pool_b").unwrap();

        assert!((pool_a_pct - 66.666).abs() < 0.01);
        assert!((pool_b_pct - 33.333).abs() < 0.01);
    }

    #[test]
    fn test_distribution_zero_shares() {
        let collector = MetricsCollector::new(vec!["pool_a".to_string()]);
        let distribution = collector.calculate_distribution_from_accepted();
        assert!(distribution.is_empty());
    }

    #[test]
    fn test_reset() {
        let metrics = UpstreamMetrics::new();

        metrics.inc_submitted();
        metrics.inc_accepted();

        metrics.reset();

        let (submitted, accepted, rejected) = metrics.get_counts();
        assert_eq!(submitted, 0);
        assert_eq!(accepted, 0);
        assert_eq!(rejected, 0);
    }
}
