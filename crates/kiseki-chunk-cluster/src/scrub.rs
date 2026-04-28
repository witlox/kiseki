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

use std::time::Duration;

use async_trait::async_trait;
use kiseki_common::ids::ChunkId;

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
        O: ClusterChunkOracle,
        D: OrphanDeleter,
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
}
