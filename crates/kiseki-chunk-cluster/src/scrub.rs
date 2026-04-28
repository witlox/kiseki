//! Orphan-fragment scrub (Phase 16b step 4).
//!
//! Mitigates **F-D7** — leader crashes mid-write so fragments land on
//! 2-of-3 peers but the `CombinedProposal` never commits. The
//! fragments are durable bytes on the receiving peers but no
//! `cluster_chunk_state` row references them. After the configured
//! TTL the scrub reclaims that storage.
//!
//! Layered for testability:
//!
//! - [`OrphanScrubPolicy`] is a pure function of `(age, has_metadata)`
//!   with no I/O. Unit-testable exhaustively.
//! - [`OrphanScrub::run`] orchestrates the policy across a list of
//!   chunk ids, a metadata oracle, and a delete sink. Each piece is a
//!   trait object so the runtime can wire production implementations
//!   in separately (step 5+ territory).
//!
//! Spec: `specs/failure-modes.md#F-D7-leader-crash-mid-write-orphan-window`,
//! `specs/implementation/phase-16-cross-node-chunks.md` Risk #5.
//!
//! Phase 16b step 5 layers a sibling scrub: [`UnderReplicationScrub`]
//! probes the placement set with `HasFragment` and re-replicates from
//! a healthy peer when fewer than `copies` peers report present. Same
//! pure-policy + orchestrator-with-trait-objects layering — both
//! scrubs share the same iteration plumbing once the runtime wires
//! them in.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kiseki_common::ids::{ChunkId, OrgId, ShardId};

/// Default TTL — chunks younger than this are kept regardless of
/// metadata state. Per the plan's Risk #5 the leader-crash-mid-write
/// orphan window is bounded at 24h.
pub const DEFAULT_ORPHAN_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Pure-function policy: should this chunk be kept or reclaimed?
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrphanDecision {
    /// Chunk is referenced by `cluster_chunk_state` — never delete.
    Keep,
    /// Chunk has no metadata but is younger than the TTL — give the
    /// in-flight write a chance to commit.
    KeepYoung,
    /// Chunk has no metadata and exceeds the TTL — reclaim.
    Delete,
}

/// Configuration for the scrub policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrphanScrubPolicy {
    /// Window before a metadata-less chunk is considered orphaned.
    pub ttl: Duration,
}

impl Default for OrphanScrubPolicy {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_ORPHAN_TTL,
        }
    }
}

impl OrphanScrubPolicy {
    /// Decide whether to keep or delete a single chunk given its
    /// local age + cluster metadata state.
    #[must_use]
    pub fn evaluate(&self, age: Duration, has_cluster_metadata: bool) -> OrphanDecision {
        if has_cluster_metadata {
            OrphanDecision::Keep
        } else if age < self.ttl {
            OrphanDecision::KeepYoung
        } else {
            OrphanDecision::Delete
        }
    }
}

/// Trait for asking "does `cluster_chunk_state` reference this
/// chunk?". Production wiring queries the per-shard Raft state
/// machine; tests use a HashSet-backed mock.
#[async_trait]
pub trait ClusterChunkOracle: Send + Sync {
    /// Returns the chunk's age relative to *this node's* clock and
    /// whether `cluster_chunk_state` has any (`tenant`, `chunk_id`) row
    /// referencing it. `None` for the chunk's age means "unknown" —
    /// the scrub treats unknown ages as zero (keep).
    async fn check(&self, chunk_id: ChunkId) -> ChunkScrubInfo;
}

/// Per-chunk scrub input gathered by the oracle.
#[derive(Clone, Copy, Debug)]
pub struct ChunkScrubInfo {
    /// Local age of the chunk.
    pub age: Duration,
    /// Whether `cluster_chunk_state` has any row for this chunk.
    pub has_cluster_metadata: bool,
}

/// Trait for the side-effect of deleting a confirmed-orphan chunk
/// from the local store.
#[async_trait]
pub trait OrphanDeleter: Send + Sync {
    /// Delete the chunk's local fragment(s). Idempotent; an already-
    /// absent chunk returns `Ok(false)`.
    async fn delete(&self, chunk_id: ChunkId) -> Result<bool, String>;
}

/// Result of one scrub pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OrphanScrubReport {
    /// Chunks the oracle was queried for.
    pub scanned: u64,
    /// Chunks left in place because cluster metadata exists.
    pub kept_metadata: u64,
    /// Chunks left in place because they were younger than the TTL.
    pub kept_young: u64,
    /// Chunks deleted (confirmed orphan + over TTL).
    pub deleted: u64,
    /// Delete-side errors (oracle errors propagate, deleter errors
    /// are counted here so the loop continues for the rest of the
    /// candidate set).
    pub delete_errors: u64,
}

/// Orchestrator. Wraps the policy + the two trait objects.
pub struct OrphanScrub {
    policy: OrphanScrubPolicy,
}

impl OrphanScrub {
    /// Build a scrub with the given policy.
    #[must_use]
    pub fn new(policy: OrphanScrubPolicy) -> Self {
        Self { policy }
    }

    /// Run a single scrub pass over `candidates`. The oracle answers
    /// per-chunk metadata + age questions; the deleter is invoked
    /// only for confirmed orphans.
    pub async fn run<O, D>(
        &self,
        candidates: &[ChunkId],
        oracle: &O,
        deleter: &D,
    ) -> OrphanScrubReport
    where
        O: ClusterChunkOracle + ?Sized,
        D: OrphanDeleter + ?Sized,
    {
        let mut report = OrphanScrubReport::default();
        for cid in candidates {
            report.scanned += 1;
            let info = oracle.check(*cid).await;
            match self.policy.evaluate(info.age, info.has_cluster_metadata) {
                OrphanDecision::Keep => report.kept_metadata += 1,
                OrphanDecision::KeepYoung => report.kept_young += 1,
                OrphanDecision::Delete => match deleter.delete(*cid).await {
                    Ok(_) => report.deleted += 1,
                    Err(_) => report.delete_errors += 1,
                },
            }
        }
        report
    }
}

// === Phase 16c step 3: LogOps-backed oracle adapter =======================

/// Adapter that turns an `Arc<dyn LogOps>` into a [`ClusterChunkOracle`].
/// Production wiring uses this against the per-shard Raft state machine;
/// tests pass any [`LogOps`] mock that returns the desired
/// `cluster_chunk_state` rows.
pub struct LogChunkOracle {
    log: Arc<dyn kiseki_log::traits::LogOps>,
    shard_id: ShardId,
    tenant_id: OrgId,
    /// Wall-clock at construction.
    started_at: std::time::Instant,
}

impl LogChunkOracle {
    /// Build an oracle scoped to one shard + tenant.
    #[must_use]
    pub fn new(
        log: Arc<dyn kiseki_log::traits::LogOps>,
        shard_id: ShardId,
        tenant_id: OrgId,
    ) -> Self {
        Self {
            log,
            shard_id,
            tenant_id,
            started_at: std::time::Instant::now(),
        }
    }
}

#[async_trait]
impl ClusterChunkOracle for LogChunkOracle {
    async fn check(&self, chunk_id: ChunkId) -> ChunkScrubInfo {
        let entry = self
            .log
            .cluster_chunk_state_get(self.shard_id, self.tenant_id, chunk_id)
            .await
            .ok()
            .flatten();
        // Phase 16c step 3 tombstone semantics: a tombstoned entry
        // counts as "no cluster metadata" — the orphan scrub treats
        // it as eligible for reclaim.
        let has_cluster_metadata = entry.as_ref().is_some_and(|e| !e.tombstoned);
        // Age: for chunks the oracle has metadata for, use the
        // process-uptime delta as a proxy (the apply-log-index stamp
        // in `created_ms` is a monotonic counter, not wall-clock,
        // so we can't subtract it from `now`). For chunks with no
        // metadata, also use uptime — that gives the orphan scrub
        // a real bound on "how long has this been waiting?". A
        // smarter age accounting (per-chunk wall-clock stamping)
        // is a Phase 16c step 4/5 concern.
        let age = self.started_at.elapsed();
        ChunkScrubInfo {
            age,
            has_cluster_metadata,
        }
    }
}

// === Phase 16b step 5: under-replication scrub ===========================

/// Decision for a single chunk's repair status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplicationDecision {
    /// Every placement peer holds the fragment — no action.
    Healthy,
    /// Fewer than `copies` peers hold the fragment but at least
    /// one does. Repair from a healthy peer.
    Repair,
    /// `min_acks` not satisfied — durability invariant broken.
    /// Repair if possible; otherwise the chunk is at risk of loss.
    Critical,
    /// No peer reports present. F-D5 territory: cannot repair from
    /// the cluster; the data may be permanently lost.
    Lost,
}

/// Pure-function policy for the under-replication scrub.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnderReplicationPolicy {
    /// Replication factor (`copies` from [`ClusterDurabilityDefaults`]).
    pub target_copies: u8,
    /// Minimum healthy copies for the durability invariant
    /// (`min_acks` from [`ClusterDurabilityDefaults`]).
    pub min_acks: usize,
}

impl UnderReplicationPolicy {
    /// Decide what to do given the per-peer presence vector. The
    /// length of `present` is the placement-list size (already
    /// reduced to peers that should hold the fragment).
    #[must_use]
    pub fn evaluate(&self, present: &[bool]) -> ReplicationDecision {
        let healthy = present.iter().filter(|p| **p).count();
        if healthy == 0 {
            ReplicationDecision::Lost
        } else if healthy < self.min_acks {
            ReplicationDecision::Critical
        } else if healthy < usize::from(self.target_copies)
            && healthy < present.len()
        {
            ReplicationDecision::Repair
        } else {
            ReplicationDecision::Healthy
        }
    }
}

/// Trait for asking each placement peer "do you hold this chunk?".
/// One call per (chunk, peer); production wiring uses the gRPC
/// `HasFragment` RPC, tests use a HashMap-backed fake.
#[async_trait]
pub trait FragmentAvailabilityOracle: Send + Sync {
    /// Returns one bool per peer in `peer_ids`, in order. `false`
    /// covers both "peer reachable + reports absent" and "peer
    /// unreachable" — both are signals the chunk needs repair.
    async fn check(&self, chunk_id: ChunkId, peer_ids: &[u64]) -> Vec<bool>;
}

/// Trait for re-replicating a chunk from a known-good peer onto a
/// known-missing peer. Production wiring fetches via `GetFragment`
/// from the source and `PutFragment` on the destination.
#[async_trait]
pub trait Repairer: Send + Sync {
    /// Re-replicate `chunk_id` from `from_peer` to `to_peer`.
    async fn repair(
        &self,
        chunk_id: ChunkId,
        from_peer: u64,
        to_peer: u64,
    ) -> Result<(), String>;
}

/// Result of a repair-scrub pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnderReplicationReport {
    /// Chunks scanned.
    pub scanned: u64,
    /// Chunks at full `target_copies`.
    pub healthy: u64,
    /// Chunks repaired (re-replicated to at least one missing peer).
    pub repaired: u64,
    /// Chunks at risk: too few healthy copies but >0.
    pub critical: u64,
    /// Chunks with zero healthy copies — F-D5.
    pub lost: u64,
    /// Repair-side errors (counted, not aborted).
    pub repair_errors: u64,
}

/// Per-chunk input to the scrub.
#[derive(Clone, Debug)]
pub struct ChunkPlacement {
    /// Chunk id.
    pub chunk_id: ChunkId,
    /// Peer ids that should hold this chunk's fragment.
    pub placement: Vec<u64>,
}

/// Orchestrator for the under-replication scrub.
pub struct UnderReplicationScrub {
    policy: UnderReplicationPolicy,
}

impl UnderReplicationScrub {
    /// Build a scrub with the given policy.
    #[must_use]
    pub fn new(policy: UnderReplicationPolicy) -> Self {
        Self { policy }
    }

    /// Run a scrub pass over `candidates`. The oracle answers per-
    /// peer presence; the repairer is invoked for chunks that need
    /// repair AND have at least one healthy source.
    pub async fn run<O, R>(
        &self,
        candidates: &[ChunkPlacement],
        oracle: &O,
        repairer: &R,
    ) -> UnderReplicationReport
    where
        O: FragmentAvailabilityOracle + ?Sized,
        R: Repairer + ?Sized,
    {
        let mut report = UnderReplicationReport::default();
        for cp in candidates {
            report.scanned += 1;
            let presence = oracle.check(cp.chunk_id, &cp.placement).await;
            match self.policy.evaluate(&presence) {
                ReplicationDecision::Healthy => report.healthy += 1,
                ReplicationDecision::Lost => report.lost += 1,
                ReplicationDecision::Repair | ReplicationDecision::Critical => {
                    let healthy_peers: Vec<u64> = cp
                        .placement
                        .iter()
                        .zip(&presence)
                        .filter(|(_, p)| **p)
                        .map(|(id, _)| *id)
                        .collect();
                    let missing_peers: Vec<u64> = cp
                        .placement
                        .iter()
                        .zip(&presence)
                        .filter(|(_, p)| !**p)
                        .map(|(id, _)| *id)
                        .collect();
                    if let (Some(&src), Some(&dst)) =
                        (healthy_peers.first(), missing_peers.first())
                    {
                        match repairer.repair(cp.chunk_id, src, dst).await {
                            Ok(()) => {
                                if matches!(
                                    self.policy.evaluate(&presence),
                                    ReplicationDecision::Critical
                                ) {
                                    report.critical += 1;
                                }
                                report.repaired += 1;
                            }
                            Err(_) => report.repair_errors += 1,
                        }
                    } else {
                        report.lost += 1;
                    }
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::*;

    // --- pure-policy tests ---------------------------------------------------

    #[test]
    fn metadata_present_always_keeps() {
        let p = OrphanScrubPolicy::default();
        // Even a chunk older than the TTL is kept if cluster_chunk_state
        // references it (the cluster says "we still need this one").
        assert_eq!(
            p.evaluate(Duration::from_secs(48 * 3600), true),
            OrphanDecision::Keep,
        );
        // Young chunks with metadata are also kept (trivial case).
        assert_eq!(
            p.evaluate(Duration::from_secs(60), true),
            OrphanDecision::Keep,
        );
    }

    #[test]
    fn metadata_absent_under_ttl_keeps_young() {
        let p = OrphanScrubPolicy::default();
        assert_eq!(
            p.evaluate(Duration::from_secs(60), false),
            OrphanDecision::KeepYoung,
            "an in-flight write that hasn't committed yet must NOT be reclaimed",
        );
    }

    #[test]
    fn metadata_absent_over_ttl_deletes() {
        let p = OrphanScrubPolicy::default();
        assert_eq!(
            p.evaluate(Duration::from_secs(25 * 3600), false),
            OrphanDecision::Delete,
        );
    }

    #[test]
    fn ttl_boundary_is_inclusive_keep() {
        let p = OrphanScrubPolicy {
            ttl: Duration::from_secs(60),
        };
        // Exactly at the TTL: per evaluate `age < ttl` → false → Delete.
        // We pin the contract here so a future "off-by-one" tweak is
        // explicit.
        assert_eq!(
            p.evaluate(Duration::from_secs(60), false),
            OrphanDecision::Delete,
            "age == ttl ⇒ Delete (boundary is exclusive on the keep side)",
        );
        // One nanosecond before: KeepYoung.
        assert_eq!(
            p.evaluate(Duration::from_secs(60) - Duration::from_nanos(1), false),
            OrphanDecision::KeepYoung,
        );
    }

    // --- orchestrator tests --------------------------------------------------

    /// Mock oracle: a chunk has metadata iff its id is in `present`,
    /// and the age is `default_age` for every chunk.
    struct FakeOracle {
        present: HashSet<ChunkId>,
        default_age: Duration,
    }

    #[async_trait]
    impl ClusterChunkOracle for FakeOracle {
        async fn check(&self, chunk_id: ChunkId) -> ChunkScrubInfo {
            ChunkScrubInfo {
                age: self.default_age,
                has_cluster_metadata: self.present.contains(&chunk_id),
            }
        }
    }

    #[derive(Default)]
    struct FakeDeleter {
        deleted: Mutex<Vec<ChunkId>>,
        fail_on: Option<ChunkId>,
    }

    #[async_trait]
    impl OrphanDeleter for FakeDeleter {
        async fn delete(&self, chunk_id: ChunkId) -> Result<bool, String> {
            if Some(chunk_id) == self.fail_on {
                return Err("simulated delete error".into());
            }
            self.deleted.lock().unwrap().push(chunk_id);
            Ok(true)
        }
    }

    fn cid(b: u8) -> ChunkId {
        ChunkId([b; 32])
    }

    #[tokio::test]
    async fn scrub_keeps_metadata_chunks_and_deletes_orphans() {
        let policy = OrphanScrubPolicy {
            ttl: Duration::from_secs(60),
        };
        let scrub = OrphanScrub::new(policy);
        // Chunk A has metadata; B and C don't. All ages over TTL.
        let oracle = FakeOracle {
            present: [cid(0xA1)].into_iter().collect(),
            default_age: Duration::from_secs(120),
        };
        let deleter = FakeDeleter::default();

        let report = scrub
            .run(&[cid(0xA1), cid(0xB2), cid(0xC3)], &oracle, &deleter)
            .await;
        assert_eq!(report.scanned, 3);
        assert_eq!(report.kept_metadata, 1, "A is kept (metadata)");
        assert_eq!(report.kept_young, 0);
        assert_eq!(report.deleted, 2, "B and C are reclaimed");
        assert_eq!(report.delete_errors, 0);

        let deleted_log = deleter.deleted.lock().unwrap();
        assert!(deleted_log.contains(&cid(0xB2)));
        assert!(deleted_log.contains(&cid(0xC3)));
        assert!(!deleted_log.contains(&cid(0xA1)));
    }

    #[tokio::test]
    async fn scrub_does_not_delete_young_orphans() {
        let policy = OrphanScrubPolicy {
            ttl: Duration::from_secs(60),
        };
        let scrub = OrphanScrub::new(policy);
        // No metadata anywhere, all younger than TTL: every chunk
        // should be KeepYoung. This is the leader-mid-write window.
        let oracle = FakeOracle {
            present: HashSet::new(),
            default_age: Duration::from_secs(10),
        };
        let deleter = FakeDeleter::default();

        let report = scrub
            .run(&[cid(0x01), cid(0x02), cid(0x03)], &oracle, &deleter)
            .await;
        assert_eq!(report.scanned, 3);
        assert_eq!(report.kept_metadata, 0);
        assert_eq!(report.kept_young, 3);
        assert_eq!(report.deleted, 0);
        assert!(deleter.deleted.lock().unwrap().is_empty());
    }

    /// Delete errors don't abort the scan — the rest of the candidate
    /// set still gets evaluated. Errors are counted in the report so
    /// ops can alarm on persistent delete failures.
    #[tokio::test]
    async fn scrub_continues_past_delete_errors() {
        let policy = OrphanScrubPolicy {
            ttl: Duration::from_secs(60),
        };
        let scrub = OrphanScrub::new(policy);
        let oracle = FakeOracle {
            present: HashSet::new(),
            default_age: Duration::from_secs(120),
        };
        let deleter = FakeDeleter {
            fail_on: Some(cid(0xB2)),
            ..Default::default()
        };

        let report = scrub
            .run(&[cid(0xA1), cid(0xB2), cid(0xC3)], &oracle, &deleter)
            .await;
        assert_eq!(report.scanned, 3);
        assert_eq!(report.deleted, 2, "A and C succeeded");
        assert_eq!(report.delete_errors, 1, "B failed");
    }

    // === Phase 16b step 5: under-replication scrub ===========================

    #[test]
    fn replication_policy_all_present_is_healthy() {
        let p = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        assert_eq!(p.evaluate(&[true, true, true]), ReplicationDecision::Healthy);
    }

    #[test]
    fn replication_policy_one_missing_is_repair() {
        let p = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        // 2 of 3 healthy: still meets min_acks but below target → Repair.
        assert_eq!(
            p.evaluate(&[true, true, false]),
            ReplicationDecision::Repair,
        );
    }

    #[test]
    fn replication_policy_below_min_acks_is_critical() {
        let p = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        // 1 of 3 healthy: under min_acks, still recoverable.
        assert_eq!(
            p.evaluate(&[true, false, false]),
            ReplicationDecision::Critical,
        );
    }

    #[test]
    fn replication_policy_all_missing_is_lost() {
        let p = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        assert_eq!(
            p.evaluate(&[false, false, false]),
            ReplicationDecision::Lost,
        );
    }

    #[test]
    fn replication_policy_target_met_below_placement_size_is_healthy() {
        // EC-style edge case: placement might list 4 peers but only
        // target_copies=3 are required healthy. Today (Replication-N)
        // placement.len() == target_copies, but the policy must still
        // return Healthy when healthy >= target_copies.
        let p = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        assert_eq!(
            p.evaluate(&[true, true, true, false]),
            ReplicationDecision::Healthy,
        );
    }

    /// Mock availability oracle: returns the bool vector exactly as
    /// stored, indexed by `chunk_id`'s first byte.
    struct FakeAvailability {
        responses: std::collections::HashMap<ChunkId, Vec<bool>>,
    }

    #[async_trait]
    impl FragmentAvailabilityOracle for FakeAvailability {
        async fn check(&self, chunk_id: ChunkId, _peer_ids: &[u64]) -> Vec<bool> {
            self.responses.get(&chunk_id).cloned().unwrap_or_default()
        }
    }

    #[derive(Default)]
    struct FakeRepairer {
        repairs: Mutex<Vec<(ChunkId, u64, u64)>>,
        fail_on: Option<ChunkId>,
    }

    #[async_trait]
    impl Repairer for FakeRepairer {
        async fn repair(
            &self,
            chunk_id: ChunkId,
            from_peer: u64,
            to_peer: u64,
        ) -> Result<(), String> {
            if Some(chunk_id) == self.fail_on {
                return Err("simulated repair failure".into());
            }
            self.repairs
                .lock()
                .unwrap()
                .push((chunk_id, from_peer, to_peer));
            Ok(())
        }
    }

    #[tokio::test]
    async fn under_replication_scrub_repairs_missing_peers() {
        let policy = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        let scrub = UnderReplicationScrub::new(policy);

        let healthy_chunk = cid(0x10);
        let one_missing = cid(0x20);
        let all_missing = cid(0x30);

        let oracle = FakeAvailability {
            responses: [
                (healthy_chunk, vec![true, true, true]),
                (one_missing, vec![true, true, false]),
                (all_missing, vec![false, false, false]),
            ]
            .into_iter()
            .collect(),
        };
        let repairer = FakeRepairer::default();

        let candidates = vec![
            ChunkPlacement {
                chunk_id: healthy_chunk,
                placement: vec![1, 2, 3],
            },
            ChunkPlacement {
                chunk_id: one_missing,
                placement: vec![1, 2, 3],
            },
            ChunkPlacement {
                chunk_id: all_missing,
                placement: vec![1, 2, 3],
            },
        ];

        let report = scrub.run(&candidates, &oracle, &repairer).await;
        assert_eq!(report.scanned, 3);
        assert_eq!(report.healthy, 1, "fully-replicated chunk left alone");
        assert_eq!(report.repaired, 1, "one-missing chunk got repaired");
        assert_eq!(report.lost, 1, "all-missing chunk reported as lost");
        assert_eq!(report.repair_errors, 0);

        let repairs = repairer.repairs.lock().unwrap();
        assert_eq!(repairs.len(), 1);
        let (cid_, from, to) = repairs[0];
        assert_eq!(cid_, one_missing);
        assert_eq!(from, 1, "first healthy peer drives the repair");
        assert_eq!(to, 3, "first missing peer is the destination");
    }

    #[tokio::test]
    async fn under_replication_scrub_counts_critical_separately() {
        // critical = healthy < min_acks. The chunk is repaired, but
        // the report flags it for ops attention.
        let policy = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        let scrub = UnderReplicationScrub::new(policy);

        let chunk = cid(0x77);
        let oracle = FakeAvailability {
            responses: [(chunk, vec![true, false, false])].into_iter().collect(),
        };
        let repairer = FakeRepairer::default();

        let report = scrub
            .run(
                &[ChunkPlacement {
                    chunk_id: chunk,
                    placement: vec![1, 2, 3],
                }],
                &oracle,
                &repairer,
            )
            .await;
        assert_eq!(report.repaired, 1, "still repaired (1 source available)");
        assert_eq!(
            report.critical, 1,
            "but flagged as critical because healthy < min_acks"
        );
    }

    // === Phase 16c step 3: LogOps-backed oracle ==========================

    /// Minimal `LogOps` mock that only answers
    /// `cluster_chunk_state_get`. Everything else returns
    /// `Unavailable`. Used to drive the `LogChunkOracle` through
    /// the orphan scrub.
    struct FakeLog {
        responses: std::collections::HashMap<
            ChunkId,
            kiseki_log::raft::state_machine::ClusterChunkStateEntry,
        >,
    }

    #[async_trait]
    impl kiseki_log::traits::LogOps for FakeLog {
        async fn append_delta(
            &self,
            _req: kiseki_log::traits::AppendDeltaRequest,
        ) -> Result<kiseki_common::ids::SequenceNumber, kiseki_log::error::LogError> {
            Err(kiseki_log::error::LogError::Unavailable)
        }
        async fn read_deltas(
            &self,
            _req: kiseki_log::traits::ReadDeltasRequest,
        ) -> Result<Vec<kiseki_log::delta::Delta>, kiseki_log::error::LogError> {
            Err(kiseki_log::error::LogError::Unavailable)
        }
        async fn shard_health(
            &self,
            shard_id: ShardId,
        ) -> Result<kiseki_log::shard::ShardInfo, kiseki_log::error::LogError> {
            Err(kiseki_log::error::LogError::ShardNotFound(shard_id))
        }
        async fn set_maintenance(
            &self,
            _shard_id: ShardId,
            _enabled: bool,
        ) -> Result<(), kiseki_log::error::LogError> {
            Ok(())
        }
        async fn truncate_log(
            &self,
            _shard_id: ShardId,
        ) -> Result<kiseki_common::ids::SequenceNumber, kiseki_log::error::LogError> {
            Ok(kiseki_common::ids::SequenceNumber(0))
        }
        async fn compact_shard(
            &self,
            _shard_id: ShardId,
        ) -> Result<u64, kiseki_log::error::LogError> {
            Ok(0)
        }
        fn create_shard(
            &self,
            _shard_id: ShardId,
            _tenant_id: OrgId,
            _node_id: kiseki_common::ids::NodeId,
            _config: kiseki_log::shard::ShardConfig,
        ) {
        }
        fn update_shard_range(
            &self,
            _shard_id: ShardId,
            _range_start: [u8; 32],
            _range_end: [u8; 32],
        ) {
        }
        fn set_shard_state(
            &self,
            _shard_id: ShardId,
            _state: kiseki_log::shard::ShardState,
        ) {
        }
        fn set_shard_config(
            &self,
            _shard_id: ShardId,
            _config: kiseki_log::shard::ShardConfig,
        ) {
        }
        async fn register_consumer(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: kiseki_common::ids::SequenceNumber,
        ) -> Result<(), kiseki_log::error::LogError> {
            Ok(())
        }
        async fn advance_watermark(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: kiseki_common::ids::SequenceNumber,
        ) -> Result<(), kiseki_log::error::LogError> {
            Ok(())
        }

        async fn cluster_chunk_state_get(
            &self,
            _shard_id: ShardId,
            _tenant_id: OrgId,
            chunk_id: ChunkId,
        ) -> Result<
            Option<kiseki_log::raft::state_machine::ClusterChunkStateEntry>,
            kiseki_log::error::LogError,
        > {
            Ok(self.responses.get(&chunk_id).cloned())
        }
    }

    fn shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }

    fn tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(2))
    }

    /// Phase 16c step 3: a chunk with a live `cluster_chunk_state`
    /// row reports `has_cluster_metadata = true` to the orphan scrub.
    #[tokio::test]
    async fn log_oracle_reports_metadata_for_present_rows() {
        let entry = kiseki_log::raft::state_machine::ClusterChunkStateEntry {
            refcount: 1,
            placement: vec![1, 2, 3],
            tombstoned: false,
            created_ms: 0,
        };
        let log: Arc<dyn kiseki_log::traits::LogOps> = Arc::new(FakeLog {
            responses: [(cid(0xC1), entry)].into_iter().collect(),
        });
        let oracle = LogChunkOracle::new(log, shard(), tenant());
        let info = oracle.check(cid(0xC1)).await;
        assert!(info.has_cluster_metadata, "live row → metadata present");
    }

    /// Phase 16c step 3: tombstoned rows count as "no metadata" so
    /// the orphan scrub considers them reclaimable once the TTL
    /// expires.
    #[tokio::test]
    async fn log_oracle_treats_tombstoned_as_no_metadata() {
        let entry = kiseki_log::raft::state_machine::ClusterChunkStateEntry {
            refcount: 0,
            placement: vec![1, 2, 3],
            tombstoned: true,
            created_ms: 0,
        };
        let log: Arc<dyn kiseki_log::traits::LogOps> = Arc::new(FakeLog {
            responses: [(cid(0xC2), entry)].into_iter().collect(),
        });
        let oracle = LogChunkOracle::new(log, shard(), tenant());
        let info = oracle.check(cid(0xC2)).await;
        assert!(
            !info.has_cluster_metadata,
            "tombstoned row → no metadata (eligible for orphan scrub once age ≥ TTL)"
        );
    }

    /// Phase 16c step 3: chunks with no row at all also report
    /// `has_cluster_metadata = false` — these are F-D7 orphans
    /// (leader crashed mid-write).
    #[tokio::test]
    async fn log_oracle_treats_missing_row_as_no_metadata() {
        let log: Arc<dyn kiseki_log::traits::LogOps> = Arc::new(FakeLog {
            responses: std::collections::HashMap::new(),
        });
        let oracle = LogChunkOracle::new(log, shard(), tenant());
        let info = oracle.check(cid(0xC3)).await;
        assert!(!info.has_cluster_metadata);
    }

    #[tokio::test]
    async fn under_replication_scrub_continues_past_repair_errors() {
        let policy = UnderReplicationPolicy {
            target_copies: 3,
            min_acks: 2,
        };
        let scrub = UnderReplicationScrub::new(policy);

        let bad = cid(0xBA);
        let good = cid(0x60);
        let oracle = FakeAvailability {
            responses: [
                (bad, vec![true, true, false]),
                (good, vec![true, true, false]),
            ]
            .into_iter()
            .collect(),
        };
        let repairer = FakeRepairer {
            fail_on: Some(bad),
            ..Default::default()
        };

        let report = scrub
            .run(
                &[
                    ChunkPlacement {
                        chunk_id: bad,
                        placement: vec![1, 2, 3],
                    },
                    ChunkPlacement {
                        chunk_id: good,
                        placement: vec![1, 2, 3],
                    },
                ],
                &oracle,
                &repairer,
            )
            .await;
        assert_eq!(report.repaired, 1, "good chunk repaired");
        assert_eq!(report.repair_errors, 1, "bad chunk's failure is counted");
    }
}
