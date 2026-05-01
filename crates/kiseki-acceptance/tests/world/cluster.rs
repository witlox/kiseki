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
