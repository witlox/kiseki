//! Per-scenario state for `@multi-node` cluster steps.
//!
//! The 3-node cluster itself is a process-level singleton (see
//! `steps::cluster_harness`). This struct holds only the values a
//! single scenario needs to flow between its `When` and `Then` steps:
//! which bucket/key was written, what bytes went in, what ETag came
//! back, and any error captured for negative assertions.

#[derive(Default)]
pub struct ClusterState {
    pub bucket: Option<String>,
    pub key: Option<String>,
    pub expected_body: Option<Vec<u8>>,
    pub last_etag: Option<String>,
    pub last_error: Option<String>,
    /// Quorum-error sightings during a scenario — the scenario "the
    /// server did not report quorum errors" asserts this stays empty.
    pub quorum_errors: Vec<String>,
    /// Node id of the leader that was killed mid-scenario, if any.
    pub killed_leader: Option<u64>,
    /// Node ids killed by `Nfollower nodes are killed` — used by the EC
    /// failure-injection scenarios (kill 3 of 5 followers on a 6-node
    /// cluster). Restart-and-rejoin runs at scenario teardown via the
    /// `the killed nodes are restarted and rejoin the cluster` step.
    pub killed_nodes: Vec<u64>,
    /// Per-metric baselines snapshotted at scenario entry. Cluster
    /// singletons are shared across scenarios, so absolute counter
    /// asserts ("ticked at least 1") would be polluted by anything an
    /// earlier scenario incremented. Keyed by `node-{id}/<metric_label>`.
    pub metric_baselines: std::collections::BTreeMap<String, f64>,
    /// Records of any failed PUT/GET round-trips collected during a
    /// multi-cycle scenario — populated by `when_n_put_get_cycles`.
    /// The matching `then` step asserts this stays empty so the test
    /// fails on the first cycle that surfaces an issue (e.g. an
    /// AEAD verification miss on a single follower).
    pub round_trip_failures: Vec<String>,
    /// Owned lock on the cluster, held for the lifetime of the
    /// scenario. cucumber-rs runs scenarios concurrently by default
    /// and our destructive ops (`kill_node`) would interleave
    /// catastrophically with non-destructive ones if every step
    /// individually re-locked. The Given step takes this lock; the
    /// World's Drop releases it so the next scenario gets a clean
    /// (post-restart-if-any) cluster.
    pub cluster_guard:
        Option<tokio::sync::OwnedMutexGuard<crate::steps::cluster_harness::ClusterHarness>>,
}
