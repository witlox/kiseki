//! Scrub scheduler (Phase 16c step 5).
//!
//! Glues steps 3+4 to the 16b scrub primitives: walks the local
//! store with [`AsyncChunkOps::list_chunk_ids`], queries the
//! Raft state machine via [`LogOps::cluster_chunk_state_get`] /
//! `_iter`, and drives the orphan + under-replication scrub
//! orchestrators against the result.
//!
//! Layered for testability:
//!
//! - [`ScrubScheduler::run_once`] executes a single pass — easy to
//!   drive from a unit test with mocks for every dependency.
//! - [`ScrubScheduler::start_periodic`] spawns a tokio task that
//!   calls `run_once` on a fixed interval; ergonomic for the
//!   server runtime.
//!
//! Spec: closes Finding 1 from
//! `specs/findings/phase-16b-adversary-audit.md`.

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::AsyncChunkOps;
use kiseki_common::ids::{OrgId, ShardId};
use kiseki_log::traits::LogOps;

use crate::scrub::{
    ChunkPlacement, ChunkPlacementWithLen, FragmentAvailabilityOracle, LogChunkOracle,
    OrphanDeleter, OrphanScrub, OrphanScrubPolicy, OrphanScrubReport, Repairer,
    UnderReplicationPolicy, UnderReplicationReport, UnderReplicationScrub,
};

/// Result of one scrub pass — orphan + under-replication combined.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrubReport {
    /// Orphan-fragment scrub outcomes.
    pub orphan: OrphanScrubReport,
    /// Under-replication scrub outcomes.
    pub under_replication: UnderReplicationReport,
}

/// Scheduler holding every dependency needed for a scrub pass.
/// Build once at runtime startup; call [`run_once`] from a
/// `tokio::time::interval` loop or directly from tests.
pub struct ScrubScheduler {
    log: Arc<dyn LogOps>,
    local: Arc<dyn AsyncChunkOps>,
    peer_oracle: Arc<dyn FragmentAvailabilityOracle>,
    deleter: Arc<dyn OrphanDeleter>,
    repairer: Arc<dyn Repairer>,
    orphan_policy: OrphanScrubPolicy,
    under_replication_policy: UnderReplicationPolicy,
    /// Phase 16e step 3: when set, the under-replication scrub
    /// uses [`UnderReplicationScrub::run_ec`] + the repairer's
    /// `repair_ec` method (decode + re-encode). When `None` the
    /// legacy Replication-N path runs.
    strategy: Option<crate::ec::EcStrategy>,
    shard_id: ShardId,
    tenant_id: OrgId,
}

impl ScrubScheduler {
    /// Build a scheduler with explicit policies.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        log: Arc<dyn LogOps>,
        local: Arc<dyn AsyncChunkOps>,
        peer_oracle: Arc<dyn FragmentAvailabilityOracle>,
        deleter: Arc<dyn OrphanDeleter>,
        repairer: Arc<dyn Repairer>,
        shard_id: ShardId,
        tenant_id: OrgId,
        orphan_policy: OrphanScrubPolicy,
        under_replication_policy: UnderReplicationPolicy,
    ) -> Self {
        Self {
            log,
            local,
            peer_oracle,
            deleter,
            repairer,
            orphan_policy,
            under_replication_policy,
            strategy: None,
            shard_id,
            tenant_id,
        }
    }

    /// Phase 16e step 3: configure the EC strategy. When set,
    /// `run_once` builds candidate placements with `original_len`
    /// from `cluster_chunk_state` and dispatches via
    /// `UnderReplicationScrub::run_ec`.
    #[must_use]
    pub fn with_strategy(mut self, strategy: crate::ec::EcStrategy) -> Self {
        self.strategy = Some(strategy);
        self
    }

    /// Execute one scrub pass. Reads the local chunk set + the
    /// `cluster_chunk_state` table, runs both scrub orchestrators,
    /// and returns the combined report.
    ///
    /// # Errors
    /// Returns [`kiseki_log::error::LogError`] only on the
    /// `cluster_chunk_state_iter` call — every other dep absorbs
    /// errors into the per-scrub report.
    pub async fn run_once(&self) -> Result<ScrubReport, kiseki_log::error::LogError> {
        // Orphan scrub: walk the local store, ask the LogChunkOracle
        // about each id.
        let local_ids = self.local.list_chunk_ids().await;
        let oracle = LogChunkOracle::new(
            Arc::clone(&self.log),
            self.shard_id,
            self.tenant_id,
        );
        let orphan = OrphanScrub::new(self.orphan_policy)
            .run(&local_ids, &oracle, self.deleter.as_ref())
            .await;

        // Under-replication scrub: walk cluster_chunk_state, ask the
        // peer oracle about each row's placement. Phase 16e step 3:
        // dispatch on configured strategy — EC mode uses run_ec
        // with original_len from each row, Replication-N uses run.
        let rows = self.log.cluster_chunk_state_iter(self.shard_id).await?;
        let under_replication = if let Some(strategy) = self.strategy {
            let candidates: Vec<ChunkPlacementWithLen> = rows
                .into_iter()
                .filter(|(_, _, e)| !e.tombstoned && !e.placement.is_empty())
                .map(|(_, chunk_id, e)| ChunkPlacementWithLen {
                    chunk_id,
                    placement: e.placement,
                    original_len: usize::try_from(e.original_len).unwrap_or(0),
                })
                .collect();
            UnderReplicationScrub::new(self.under_replication_policy)
                .with_strategy(strategy)
                .run_ec(
                    &candidates,
                    self.peer_oracle.as_ref(),
                    self.repairer.as_ref(),
                )
                .await
        } else {
            let candidates: Vec<ChunkPlacement> = rows
                .into_iter()
                .filter(|(_, _, e)| !e.tombstoned && !e.placement.is_empty())
                .map(|(_, chunk_id, e)| ChunkPlacement {
                    chunk_id,
                    placement: e.placement,
                })
                .collect();
            UnderReplicationScrub::new(self.under_replication_policy)
                .run(
                    &candidates,
                    self.peer_oracle.as_ref(),
                    self.repairer.as_ref(),
                )
                .await
        };

        Ok(ScrubReport {
            orphan,
            under_replication,
        })
    }

    /// Spawn a tokio task that calls [`run_once`] every `interval`.
    /// Errors from `run_once` are logged and the loop continues —
    /// a one-shard hiccup must not stall the rest. Phase 16e step 4
    /// adds graceful shutdown: the loop exits cleanly when
    /// `shutdown.changed()` fires (caller sends `true`), so the
    /// returned `JoinHandle` joins normally rather than via abort.
    #[must_use]
    pub fn start_periodic(
        self: Arc<Self>,
        interval: Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate fire so the runtime has time to settle.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately; absorb it
            loop {
                tokio::select! {
                    biased; // check shutdown first so a pending tick + shutdown
                            // exits cleanly instead of running one more pass
                    res = shutdown.changed() => {
                        if res.is_err() || *shutdown.borrow() {
                            tracing::info!("scrub scheduler: shutdown received, draining");
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        match self.run_once().await {
                            Ok(report) => {
                                if report.orphan.deleted > 0
                                    || report.under_replication.repaired > 0
                                {
                                    tracing::info!(
                                        orphan_deleted = report.orphan.deleted,
                                        under_repl_repaired = report.under_replication.repaired,
                                        under_repl_critical = report.under_replication.critical,
                                        under_repl_lost = report.under_replication.lost,
                                        "scrub pass made changes",
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error=%e, "scrub pass failed");
                            }
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use kiseki_chunk::ChunkError;
    use kiseki_common::ids::{ChunkId, NodeId, SequenceNumber};
    use kiseki_crypto::envelope::Envelope;
    use kiseki_log::raft::state_machine::ClusterChunkStateEntry;

    use super::*;

    fn cid(b: u8) -> ChunkId {
        ChunkId([b; 32])
    }

    fn shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }

    fn tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(2))
    }

    /// `AsyncChunkOps` mock returning a fixed `list_chunk_ids`.
    /// Other methods are unreachable for the scheduler tests.
    struct FakeLocalChunks {
        chunk_ids: Vec<ChunkId>,
    }

    #[async_trait]
    impl AsyncChunkOps for FakeLocalChunks {
        async fn write_chunk(
            &self,
            _env: Envelope,
            _pool: &str,
        ) -> Result<bool, ChunkError> {
            unreachable!("scheduler test does not write")
        }
        async fn read_chunk(&self, _id: &ChunkId) -> Result<Envelope, ChunkError> {
            unreachable!("scheduler test does not read")
        }
        async fn increment_refcount(&self, _id: &ChunkId) -> Result<u64, ChunkError> {
            unreachable!()
        }
        async fn decrement_refcount(&self, _id: &ChunkId) -> Result<u64, ChunkError> {
            unreachable!()
        }
        async fn set_retention_hold(
            &self,
            _id: &ChunkId,
            _hold: &str,
        ) -> Result<(), ChunkError> {
            Ok(())
        }
        async fn release_retention_hold(
            &self,
            _id: &ChunkId,
            _hold: &str,
        ) -> Result<(), ChunkError> {
            Ok(())
        }
        async fn gc(&self) -> u64 {
            0
        }
        async fn refcount(&self, _id: &ChunkId) -> Result<u64, ChunkError> {
            Ok(0)
        }
        async fn list_chunk_ids(&self) -> Vec<ChunkId> {
            self.chunk_ids.clone()
        }
    }

    /// `LogOps` mock answering only the `cluster_chunk_state` methods.
    struct FakeLog {
        single: HashMap<ChunkId, ClusterChunkStateEntry>,
        iter: Vec<(OrgId, ChunkId, ClusterChunkStateEntry)>,
    }

    #[async_trait]
    impl LogOps for FakeLog {
        async fn append_delta(
            &self,
            _req: kiseki_log::traits::AppendDeltaRequest,
        ) -> Result<SequenceNumber, kiseki_log::error::LogError> {
            unreachable!()
        }
        async fn read_deltas(
            &self,
            _req: kiseki_log::traits::ReadDeltasRequest,
        ) -> Result<Vec<kiseki_log::delta::Delta>, kiseki_log::error::LogError> {
            unreachable!()
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
        ) -> Result<SequenceNumber, kiseki_log::error::LogError> {
            Ok(SequenceNumber(0))
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
            _node_id: NodeId,
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
            _position: SequenceNumber,
        ) -> Result<(), kiseki_log::error::LogError> {
            Ok(())
        }
        async fn advance_watermark(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: SequenceNumber,
        ) -> Result<(), kiseki_log::error::LogError> {
            Ok(())
        }

        async fn cluster_chunk_state_get(
            &self,
            _shard_id: ShardId,
            _tenant_id: OrgId,
            chunk_id: ChunkId,
        ) -> Result<Option<ClusterChunkStateEntry>, kiseki_log::error::LogError> {
            Ok(self.single.get(&chunk_id).cloned())
        }
        async fn cluster_chunk_state_iter(
            &self,
            _shard_id: ShardId,
        ) -> Result<Vec<(OrgId, ChunkId, ClusterChunkStateEntry)>, kiseki_log::error::LogError>
        {
            Ok(self.iter.clone())
        }
    }

    struct FakePeerOracle;

    #[async_trait]
    impl FragmentAvailabilityOracle for FakePeerOracle {
        async fn check(&self, _chunk_id: ChunkId, peer_ids: &[u64]) -> Vec<bool> {
            // Pretend every peer reports `true` (healthy) — the
            // under-replication scrub should observe Healthy.
            vec![true; peer_ids.len()]
        }
    }

    #[derive(Default)]
    struct FakeDeleter {
        deleted: Mutex<Vec<ChunkId>>,
    }

    #[async_trait]
    impl OrphanDeleter for FakeDeleter {
        async fn delete(&self, chunk_id: ChunkId) -> Result<bool, String> {
            self.deleted.lock().unwrap().push(chunk_id);
            Ok(true)
        }
    }

    #[derive(Default)]
    struct FakeRepairer {
        repairs: Mutex<u32>,
    }

    #[async_trait]
    impl Repairer for FakeRepairer {
        async fn repair(
            &self,
            _chunk_id: ChunkId,
            _from: u64,
            _to: u64,
        ) -> Result<(), String> {
            *self.repairs.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// Phase 16c step 5: a fully-healthy 3-node cluster scrub pass
    /// scans every local chunk, finds metadata for each, and reports
    /// nothing to do.
    #[tokio::test]
    async fn run_once_healthy_cluster_does_nothing() {
        let chunk_ids = vec![cid(0xA1), cid(0xA2)];
        let entry = ClusterChunkStateEntry {
            refcount: 1,
            placement: vec![1, 2, 3],
            tombstoned: false,
            created_ms: 0,
            original_len: 0,
        };
        let single: HashMap<_, _> = chunk_ids
            .iter()
            .map(|id| (*id, entry.clone()))
            .collect();
        let iter = chunk_ids
            .iter()
            .map(|id| (tenant(), *id, entry.clone()))
            .collect();

        let log: Arc<dyn LogOps> = Arc::new(FakeLog { single, iter });
        let local: Arc<dyn AsyncChunkOps> = Arc::new(FakeLocalChunks { chunk_ids });
        let peer_oracle: Arc<dyn FragmentAvailabilityOracle> = Arc::new(FakePeerOracle);
        let deleter: Arc<dyn OrphanDeleter> = Arc::new(FakeDeleter::default());
        let repairer: Arc<dyn Repairer> = Arc::new(FakeRepairer::default());

        let scheduler = ScrubScheduler::new(
            log,
            local,
            peer_oracle,
            Arc::clone(&deleter),
            Arc::clone(&repairer),
            shard(),
            tenant(),
            OrphanScrubPolicy::default(),
            UnderReplicationPolicy {
                target_copies: 3,
                min_acks: 2,
            },
        );

        let report = scheduler.run_once().await.expect("ok");
        assert_eq!(report.orphan.scanned, 2, "every local chunk scanned");
        assert_eq!(
            report.orphan.deleted, 0,
            "healthy cluster: nothing deleted"
        );
        assert_eq!(report.under_replication.scanned, 2);
        assert_eq!(report.under_replication.healthy, 2);
        assert_eq!(report.under_replication.repaired, 0);
    }

    /// Phase 16c step 5: when a local chunk has no
    /// `cluster_chunk_state` row AND age (process uptime) ≥ TTL, the
    /// orphan scrub deletes it. We force this with a TTL of zero.
    #[tokio::test]
    async fn run_once_reclaims_truly_orphaned_chunks() {
        let orphan_chunk = cid(0xBE);
        let local: Arc<dyn AsyncChunkOps> = Arc::new(FakeLocalChunks {
            chunk_ids: vec![orphan_chunk],
        });
        let log: Arc<dyn LogOps> = Arc::new(FakeLog {
            single: HashMap::new(), // no metadata for the orphan
            iter: Vec::new(),
        });
        let peer_oracle: Arc<dyn FragmentAvailabilityOracle> = Arc::new(FakePeerOracle);
        // Hold the concrete deleter so the test can read its log
        // without erasing through the trait object.
        let deleter_inner = Arc::new(FakeDeleter::default());
        let deleter: Arc<dyn OrphanDeleter> = Arc::clone(&deleter_inner) as _;
        let repairer: Arc<dyn Repairer> = Arc::new(FakeRepairer::default());

        let scheduler = ScrubScheduler::new(
            log,
            local,
            peer_oracle,
            deleter,
            repairer,
            shard(),
            tenant(),
            OrphanScrubPolicy {
                ttl: Duration::ZERO, // force the Delete branch
            },
            UnderReplicationPolicy {
                target_copies: 3,
                min_acks: 2,
            },
        );

        let report = scheduler.run_once().await.expect("ok");
        assert_eq!(report.orphan.scanned, 1);
        assert_eq!(report.orphan.deleted, 1);
        let log_entries = deleter_inner.deleted.lock().unwrap();
        assert!(log_entries.contains(&orphan_chunk));
    }

    /// Phase 16e step 4: the scrub task exits cleanly when the
    /// shutdown signal fires. The `JoinHandle` joins normally (not
    /// via abort) within a small bound so production runtimes can
    /// drain in-flight work before exiting.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_periodic_drains_on_shutdown_signal() {
        let chunk_ids = vec![cid(0xA1)];
        let log: Arc<dyn LogOps> = Arc::new(FakeLog {
            single: HashMap::new(),
            iter: Vec::new(),
        });
        let local: Arc<dyn AsyncChunkOps> = Arc::new(FakeLocalChunks { chunk_ids });
        let peer_oracle: Arc<dyn FragmentAvailabilityOracle> = Arc::new(FakePeerOracle);
        let deleter: Arc<dyn OrphanDeleter> = Arc::new(FakeDeleter::default());
        let repairer: Arc<dyn Repairer> = Arc::new(FakeRepairer::default());

        let scheduler = Arc::new(ScrubScheduler::new(
            log,
            local,
            peer_oracle,
            deleter,
            repairer,
            shard(),
            tenant(),
            OrphanScrubPolicy::default(),
            UnderReplicationPolicy {
                target_copies: 3,
                min_acks: 2,
            },
        ));

        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = scheduler.start_periodic(Duration::from_millis(20), rx);

        // Let one tick happen, then signal shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).expect("send shutdown");

        // The handle must complete cleanly (no abort) within a
        // small bound — the loop hits the `shutdown.changed()`
        // arm on the next select iteration.
        let join_result = tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("scrub task did not exit within 500ms of shutdown signal");
        join_result.expect("scrub task panicked instead of clean join");
    }
}
