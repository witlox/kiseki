//! Raft configuration helpers.

use std::sync::Arc;

/// Build a default Raft config suitable for Kiseki.
///
/// Key settings:
/// - `heartbeat_interval`: 500ms (fast failure detection on fabric)
/// - `election_timeout_min/max`: 1500-3000ms
/// - `max_payload_entries`: 300
/// - `snapshot_policy`: after 1000 applied entries
pub struct KisekiRaftConfig;

impl KisekiRaftConfig {
    /// Build an `openraft::Config` with Kiseki defaults.
    #[must_use]
    pub fn default_config() -> Arc<openraft::Config> {
        let config = openraft::Config {
            heartbeat_interval: 500,
            election_timeout_min: 1500,
            election_timeout_max: 3000,
            max_payload_entries: 300,
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(1000),
            ..openraft::Config::default()
        };
        Arc::new(config.validate().unwrap_or_else(|e| {
            // Config validation should never fail with these values.
            // If it does, the defaults in openraft changed — use them.
            eprintln!("WARNING: raft config validation failed: {e}, using defaults");
            openraft::Config::default()
        }))
    }
}
