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
        // #226 100k attempt: snapshot cadence raised 1,000 → 4,000
        // logs (override: KISEKI_RAFT_SNAPSHOT_LOGS). Kiseki's
        // build_snapshot serializes the retained deltas + reads back
        // every inline offload UNDER THE SAME MUTEX AS APPLY, so at
        // batched-entry write rates the old cadence stalled applies
        // every few seconds — a measured component of the
        // with-volume throughput decay. Catch-up replication is
        // byte-budgeted post-#255 and the #220 over-cap alarm still
        // fires on bloated snapshots; the binding constraint on the
        // cadence is RETAINED-LOG DISK (LogsSinceLast is count-only;
        // intent-path entries carry inline payloads at ~0.3–1.3
        // MiB/entry, so 4k logs ≈ 1–5 GiB/shard between snapshots —
        // 10k would be 3–13 GiB/shard, too hot for dev/CI disks as
        // the adversary review flagged).
        let snapshot_logs = std::env::var("KISEKI_RAFT_SNAPSHOT_LOGS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n: &u64| *n >= 1)
            .unwrap_or(4_000);
        let config = openraft::Config {
            heartbeat_interval: 500,
            election_timeout_min: 1500,
            election_timeout_max: 3000,
            max_payload_entries: 300,
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(snapshot_logs),
            ..openraft::Config::default()
        };
        Arc::new(config.validate().unwrap_or_else(|e| {
            // Config validation should never fail with these values.
            // If it does, the defaults in openraft changed — use them.
            tracing::warn!(error = %e, "raft config validation failed, using defaults");
            openraft::Config::default()
        }))
    }
}
