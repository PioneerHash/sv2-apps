use crate::assignment::types::{Assignment, DownstreamId};
use crate::config::MultiplexedUpstream;
use crate::error::{MultiplexerError, Result};
use hashbrown::HashMap;
use std::sync::{Arc, RwLock};

/// Manages assignments of downstreams to upstreams
#[derive(Debug)]
pub struct AssignmentManager {
    /// Map of downstream ID to current assignment
    assignments: Arc<RwLock<HashMap<DownstreamId, Assignment>>>,

    /// Upstream configurations (for priority-based assignment)
    upstreams: Vec<MultiplexedUpstream>,
}

impl AssignmentManager {
    /// Create a new assignment manager
    pub fn new(upstreams: Vec<MultiplexedUpstream>) -> Self {
        Self {
            assignments: Arc::new(RwLock::new(HashMap::new())),
            upstreams,
        }
    }

    /// Assign a new miner to an upstream based on priority and current distribution
    ///
    /// Strategy:
    /// 1. Sort upstreams by priority (ascending: 1, 2, 3...)
    /// 2. For each enabled upstream, check if current percentage < target
    /// 3. Assign to first upstream under target
    /// 4. If all at/above target, assign to highest priority (priority 1)
    pub fn assign_new_miner(
        &self,
        downstream_id: DownstreamId,
        channel_id: u32,
        extranonce_prefix: Vec<u8>,
    ) -> Result<String> {
        let current_distribution = self.calculate_current_distribution();

        // Sort upstreams by priority (ascending)
        let mut upstreams = self.upstreams.clone();
        upstreams.sort_by_key(|u| u.priority);

        // Find first enabled upstream under target percentage
        for upstream in &upstreams {
            if !upstream.enabled {
                continue;
            }

            let current_pct = current_distribution.get(&upstream.id).copied().unwrap_or(0.0);
            if current_pct < upstream.percentage {
                // Assign to this upstream
                let assignment = Assignment::new(
                    downstream_id,
                    upstream.id.clone(),
                    channel_id,
                    extranonce_prefix,
                );

                self.assignments
                    .write()
                    .unwrap()
                    .insert(downstream_id, assignment);

                return Ok(upstream.id.clone());
            }
        }

        // All at or above target - assign to highest priority enabled upstream
        for upstream in &upstreams {
            if upstream.enabled {
                let assignment = Assignment::new(
                    downstream_id,
                    upstream.id.clone(),
                    channel_id,
                    extranonce_prefix,
                );

                self.assignments
                    .write()
                    .unwrap()
                    .insert(downstream_id, assignment);

                return Ok(upstream.id.clone());
            }
        }

        Err(MultiplexerError::NoEnabledUpstreams)
    }

    /// Get assignment for a downstream
    pub fn get_assignment(&self, downstream_id: DownstreamId) -> Option<Assignment> {
        self.assignments.read().unwrap().get(&downstream_id).cloned()
    }

    /// Remove assignment (miner disconnected)
    pub fn remove_assignment(&self, downstream_id: DownstreamId) -> Option<Assignment> {
        self.assignments.write().unwrap().remove(&downstream_id)
    }

    /// Update assignment to new upstream
    pub fn update_assignment(
        &self,
        downstream_id: DownstreamId,
        new_upstream_id: String,
        new_extranonce_prefix: Vec<u8>,
    ) -> Result<()> {
        let mut assignments = self.assignments.write().unwrap();
        let assignment = assignments
            .get_mut(&downstream_id)
            .ok_or(MultiplexerError::UnknownDownstream(downstream_id))?;

        assignment.update(new_upstream_id, new_extranonce_prefix);
        Ok(())
    }

    /// Get all downstreams assigned to a specific upstream
    pub fn get_downstreams_for_upstream(&self, upstream_id: &str) -> Vec<DownstreamId> {
        self.assignments
            .read()
            .unwrap()
            .values()
            .filter(|a| a.upstream_id == upstream_id)
            .map(|a| a.downstream_id)
            .collect()
    }

    /// Get total number of miners
    pub fn total_miners(&self) -> usize {
        self.assignments.read().unwrap().len()
    }

    /// Calculate current distribution (percentage per upstream)
    pub fn calculate_current_distribution(&self) -> HashMap<String, f64> {
        let assignments = self.assignments.read().unwrap();
        let total = assignments.len() as f64;

        if total == 0.0 {
            return HashMap::new();
        }

        // Count miners per upstream
        let mut counts: HashMap<String, usize> = HashMap::new();
        for assignment in assignments.values() {
            *counts.entry(assignment.upstream_id.clone()).or_insert(0) += 1;
        }

        // Convert to percentages
        counts
            .into_iter()
            .map(|(upstream_id, count)| (upstream_id, (count as f64 / total) * 100.0))
            .collect()
    }

    /// Get all assignments (for debugging/metrics)
    pub fn all_assignments(&self) -> Vec<Assignment> {
        self.assignments.read().unwrap().values().cloned().collect()
    }

    /// Update upstream configurations
    pub fn update_upstreams(&mut self, upstreams: Vec<MultiplexedUpstream>) {
        self.upstreams = upstreams;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MultiplexedUpstream;

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
    fn test_initial_assignment_priority_order() {
        let upstreams = vec![
            create_test_upstream("pool_a", 60.0, 1),
            create_test_upstream("pool_b", 30.0, 2),
            create_test_upstream("pool_c", 10.0, 3),
        ];

        let manager = AssignmentManager::new(upstreams);

        // First miner should go to highest priority (pool_a)
        let upstream_id = manager
            .assign_new_miner(1, 1, vec![0x01])
            .unwrap();
        assert_eq!(upstream_id, "pool_a");
    }

    #[test]
    fn test_distribution_based_assignment() {
        let upstreams = vec![
            create_test_upstream("pool_a", 50.0, 1),
            create_test_upstream("pool_b", 50.0, 2),
        ];

        let manager = AssignmentManager::new(upstreams);

        // Assign 10 miners
        for i in 0..10 {
            manager
                .assign_new_miner(i, i, vec![i as u8])
                .unwrap();
        }

        // Check distribution
        let distribution = manager.calculate_current_distribution();

        // Should be approximately 50/50
        let pool_a_pct = distribution.get("pool_a").copied().unwrap_or(0.0);
        let pool_b_pct = distribution.get("pool_b").copied().unwrap_or(0.0);

        // With 10 miners and 50/50 target, expect 5 per pool
        assert_eq!(pool_a_pct, 50.0);
        assert_eq!(pool_b_pct, 50.0);
    }

    #[test]
    fn test_get_downstreams_for_upstream() {
        let upstreams = vec![
            create_test_upstream("pool_a", 60.0, 1),
            create_test_upstream("pool_b", 40.0, 2),
        ];

        let manager = AssignmentManager::new(upstreams);

        // Assign some miners
        manager
            .assign_new_miner(1, 1, vec![0x01])
            .unwrap();
        manager
            .assign_new_miner(2, 2, vec![0x02])
            .unwrap();
        manager
            .assign_new_miner(3, 3, vec![0x03])
            .unwrap();

        // Manually update one to pool_b for testing
        manager
            .update_assignment(2, "pool_b".to_string(), vec![0x02])
            .unwrap();

        let pool_a_miners = manager.get_downstreams_for_upstream("pool_a");
        let pool_b_miners = manager.get_downstreams_for_upstream("pool_b");

        assert_eq!(pool_a_miners.len(), 2);
        assert_eq!(pool_b_miners.len(), 1);
        assert!(pool_a_miners.contains(&1));
        assert!(pool_a_miners.contains(&3));
        assert!(pool_b_miners.contains(&2));
    }

    #[test]
    fn test_remove_assignment() {
        let upstreams = vec![create_test_upstream("pool_a", 100.0, 1)];
        let manager = AssignmentManager::new(upstreams);

        manager
            .assign_new_miner(1, 1, vec![0x01])
            .unwrap();
        assert_eq!(manager.total_miners(), 1);

        let removed = manager.remove_assignment(1);
        assert!(removed.is_some());
        assert_eq!(manager.total_miners(), 0);
    }

    #[test]
    fn test_no_enabled_upstreams() {
        let mut upstream = create_test_upstream("pool_a", 100.0, 1);
        upstream.enabled = false;

        let manager = AssignmentManager::new(vec![upstream]);

        let result = manager.assign_new_miner(1, 1, vec![0x01]);
        assert!(matches!(result, Err(MultiplexerError::NoEnabledUpstreams)));
    }
}
