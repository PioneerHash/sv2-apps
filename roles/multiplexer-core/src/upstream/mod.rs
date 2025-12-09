use crate::config::MultiplexedUpstream;
use hashbrown::HashMap;

/// Registry for tracking upstream connections and health
/// Note: This is a placeholder for now. Full upstream connection management
/// will be implemented in the daemon layer.
#[derive(Debug)]
pub struct UpstreamRegistry {
    upstreams: HashMap<String, MultiplexedUpstream>,
}

impl UpstreamRegistry {
    pub fn new(upstreams: Vec<MultiplexedUpstream>) -> Self {
        let mut registry = HashMap::new();
        for upstream in upstreams {
            registry.insert(upstream.id.clone(), upstream);
        }

        Self {
            upstreams: registry,
        }
    }

    pub fn get(&self, upstream_id: &str) -> Option<&MultiplexedUpstream> {
        self.upstreams.get(upstream_id)
    }

    pub fn all(&self) -> Vec<&MultiplexedUpstream> {
        self.upstreams.values().collect()
    }

    pub fn enabled(&self) -> Vec<&MultiplexedUpstream> {
        self.upstreams
            .values()
            .filter(|u| u.enabled)
            .collect()
    }
}
