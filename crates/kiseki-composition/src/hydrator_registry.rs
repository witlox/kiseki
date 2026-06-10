//! Per-shard composition-hydrator registry.
//!
//! Each Raft shard has its own delta log with its own sequence space.
//! The Phase 16f composition hydrator was originally wired against a
//! single bootstrap shard at startup — fine when every namespace lived
//! on that one shard, but the ADR-033 §4 / §1 multi-shard topology
//! puts each tenant namespace on its own per-namespace shard (created
//! at first-touch by `ControlPlaneProvisioner`). Without a hydrator
//! polling those shards, followers would never install the
//! composition rows for objects PUT through their per-shard Raft log,
//! so cross-node S3 GET in fresh tenant buckets returned 404.
//!
//! This registry spawns one hydrator task per known shard. The
//! cluster-control apply hook calls [`HydratorRegistry::register`]
//! whenever a `CreateNamespace` / `RecordSplit` commits, so every
//! node hosting that shard immediately starts hydrating it. Idempotent
//! — re-registering an already-active shard is a no-op.
//!
//! The registry is in-memory only: on restart the runtime re-registers
//! the bootstrap shard and the apply hook re-fires for every shard the
//! control-plane state machine knows about (apply hooks run on replay
//! through snapshot install + tail catch-up, so the same shard set
//! gets re-registered). Per-shard `last_applied_seq` in
//! `CompositionStorage` survives restart, so re-registration does not
//! cause a full log replay.

use std::collections::HashSet;
use std::sync::Arc;

use kiseki_common::ids::ShardId;
use kiseki_log::traits::{LogOps, HYDRATOR_CONSUMER};
use parking_lot::Mutex;

use crate::composition::CompositionStore;
use crate::hydrator::CompositionHydrator;
use crate::metrics::CompositionMetrics;

/// Tracks one hydrator task per registered shard.
pub struct HydratorRegistry {
    compositions: Arc<CompositionStore>,
    log: Arc<dyn LogOps + Send + Sync>,
    metrics: Option<Arc<CompositionMetrics>>,
    /// In-memory set of shards already being polled. Guards against
    /// double-spawn under concurrent calls from the apply hook.
    active: Mutex<HashSet<ShardId>>,
    poll_interval: std::time::Duration,
}

impl HydratorRegistry {
    /// Build a registry. No hydrator tasks are spawned until
    /// [`HydratorRegistry::register`] is called.
    #[must_use]
    pub fn new(
        compositions: Arc<CompositionStore>,
        log: Arc<dyn LogOps + Send + Sync>,
        metrics: Option<Arc<CompositionMetrics>>,
    ) -> Self {
        // Poll cadence is tunable via KISEKI_HYDRATOR_POLL_MS (default
        // 100 ms). Set it very high to effectively pause hydration — used
        // by the #126/#133 perf bisection to isolate the hydrator's
        // runtime impact on the write path.
        let poll_ms = std::env::var("KISEKI_HYDRATOR_POLL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(100);
        Self {
            compositions,
            log,
            metrics,
            active: Mutex::new(HashSet::new()),
            poll_interval: std::time::Duration::from_millis(poll_ms),
        }
    }

    /// Register a shard for hydration. Spawns a new tokio task that
    /// polls this shard's delta log every 100 ms. Idempotent —
    /// re-registering an active shard returns immediately.
    ///
    /// MUST be called from inside a tokio runtime (uses
    /// `tokio::spawn`).
    pub fn register(&self, shard_id: ShardId) {
        {
            let mut active = self.active.lock();
            if !active.insert(shard_id) {
                tracing::debug!(
                    shard_id = %shard_id.0,
                    "hydrator registry: shard already registered — skipping",
                );
                return;
            }
        }
        let compositions = Arc::clone(&self.compositions);
        let log = Arc::clone(&self.log);
        let metrics = self.metrics.clone();
        let interval = self.poll_interval;
        tokio::spawn(async move {
            let mut hydrator = CompositionHydrator::new(compositions, shard_id);
            if let Some(m) = metrics {
                hydrator = hydrator.with_metrics(m);
            }
            tracing::info!(
                shard_id = %shard_id.0,
                "composition hydrator: per-shard poll loop started",
            );
            loop {
                let applied = hydrator.poll(log.as_ref()).await;
                // P3 / I-L4: report this NODE's consumption position —
                // synchronous, infallible, node-local (NEVER a Raft
                // write: register/advance forward to the leader and
                // fail forever on followers). The shard leader's
                // supervisor gathers every voter's reported position
                // and proposes `min` as the replicated `hydrator`
                // watermark, so delta pruning can never outrun this
                // node. Reported after EVERY poll (not just applying
                // ones) so a restarted node's durable last_applied is
                // visible to the gather even on an idle shard — at one
                // map insert per poll the throttle isn't worth having.
                log.report_consumer_position(shard_id, HYDRATOR_CONSUMER, hydrator.last_applied());
                // #212 / #133 residue: drain while busy. Sleeping
                // unconditionally capped sustained hydration at
                // ~window/interval deltas/s and — once the write rate
                // outran that — let the backlog cross the compaction
                // horizon into permanent halt. Re-poll immediately
                // whenever the last poll applied anything; only sleep
                // once caught up (applied == 0).
                if applied == 0 {
                    tokio::time::sleep(interval).await;
                }
            }
        });
    }

    /// Number of shards currently registered (test/observability helper).
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId, SequenceNumber};
    use kiseki_log::delta::{Delta, OperationType};
    use kiseki_log::error::LogError;
    use kiseki_log::shard::{ShardConfig, ShardInfo, ShardState};
    use kiseki_log::traits::{AppendDeltaRequest, ReadDeltasRequest};
    use kiseki_log::MemShardStore;

    use crate::composition::encode_composition_create_payload;
    use crate::namespace::Namespace;

    const TEST_NS: u128 = 2;

    fn fresh_store_with_default_ns() -> Arc<CompositionStore> {
        let store = CompositionStore::new();
        store.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(TEST_NS)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
            size_band_pools: crate::namespace::NamespaceSizeBandPools::default(),
        });
        Arc::new(store)
    }

    fn fresh_log() -> (Arc<MemShardStore>, ShardId) {
        let log = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        log.create_shard(
            shard_id,
            OrgId(uuid::Uuid::from_u128(1)),
            kiseki_common::ids::NodeId(1),
            ShardConfig::default(),
        );
        (Arc::new(log), shard_id)
    }

    async fn append_creates(log: &MemShardStore, shard_id: ShardId, n: u64, id_offset: u128) {
        let ns_id = NamespaceId(uuid::Uuid::from_u128(TEST_NS));
        for i in 0..n {
            let comp_id = CompositionId(uuid::Uuid::from_u128(u128::from(i) + id_offset + 1));
            let payload = encode_composition_create_payload(comp_id, ns_id, 64, None, &[], None);
            log.append_delta(AppendDeltaRequest {
                shard_id,
                tenant_id: OrgId(uuid::Uuid::from_u128(1)),
                operation: OperationType::Create,
                timestamp: kiseki_common::time::DeltaTimestamp {
                    hlc: kiseki_common::time::HybridLogicalClock {
                        physical_ms: 0,
                        logical: 0,
                        node_id: kiseki_common::ids::NodeId(0),
                    },
                    wall: kiseki_common::time::WallTime {
                        millis_since_epoch: 0,
                        timezone: "UTC".into(),
                    },
                    quality: kiseki_common::time::ClockQuality::Ntp,
                },
                hashed_key: [0u8; 32],
                chunk_refs: vec![ChunkId([0u8; 32])],
                payload,
                has_inline_data: false,
            })
            .await
            .unwrap();
        }
    }

    /// Observable traffic from the registry's poll loop.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum LogEvent {
        Report { consumer: String, position: u64 },
        ReadDeltas,
    }

    /// `LogOps` wrapper around `MemShardStore` that records the calls
    /// the registry makes. Only the methods the hydrator/registry
    /// exercise are real; the rest are `unimplemented!()` (same
    /// precedent as `GapInjectingLog` in `hydrator.rs`).
    struct RecordingLog {
        inner: Arc<MemShardStore>,
        events: std::sync::Mutex<Vec<LogEvent>>,
    }

    impl RecordingLog {
        fn new(inner: Arc<MemShardStore>) -> Self {
            Self {
                inner,
                events: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<LogEvent> {
            self.events.lock().unwrap().clone()
        }

        fn record(&self, ev: LogEvent) {
            self.events.lock().unwrap().push(ev);
        }
    }

    #[allow(clippy::unimplemented)]
    #[async_trait::async_trait]
    impl LogOps for RecordingLog {
        async fn put_intent_and_fan(
            &self,
            _shard_id: ShardId,
            _intent: kiseki_log::intent::WriteIntent,
        ) -> Result<(), LogError> {
            unimplemented!("test stub: hydrator never produces intents")
        }
        async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
            self.inner.append_delta(req).await
        }
        async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
            self.record(LogEvent::ReadDeltas);
            self.inner.read_deltas(req).await
        }
        async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
            self.inner.shard_health(shard_id).await
        }
        async fn earliest_visible_seq(
            &self,
            shard_id: ShardId,
        ) -> Result<SequenceNumber, LogError> {
            self.inner.earliest_visible_seq(shard_id).await
        }
        async fn set_maintenance(
            &self,
            _shard_id: ShardId,
            _enabled: bool,
        ) -> Result<(), LogError> {
            unimplemented!()
        }
        async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
            self.inner.truncate_log(shard_id).await
        }
        async fn compact_shard(&self, _shard_id: ShardId) -> Result<u64, LogError> {
            unimplemented!()
        }
        fn create_shard(
            &self,
            _shard_id: ShardId,
            _tenant_id: OrgId,
            _node_id: kiseki_common::ids::NodeId,
            _config: ShardConfig,
        ) {
            unimplemented!()
        }
        fn update_shard_range(
            &self,
            _shard_id: ShardId,
            _range_start: [u8; 32],
            _range_end: [u8; 32],
        ) {
            unimplemented!()
        }
        fn set_shard_state(&self, _shard_id: ShardId, _state: ShardState) {
            unimplemented!()
        }
        fn set_shard_config(&self, _shard_id: ShardId, _config: ShardConfig) {
            unimplemented!()
        }
        async fn register_consumer(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: SequenceNumber,
        ) -> Result<(), LogError> {
            unimplemented!(
                "P3: the hydrator must NEVER register via Raft — see report_consumer_position"
            )
        }
        async fn advance_watermark(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: SequenceNumber,
        ) -> Result<(), LogError> {
            unimplemented!(
                "P3: the hydrator must NEVER advance via Raft — see report_consumer_position"
            )
        }
        fn report_consumer_position(
            &self,
            shard_id: ShardId,
            consumer: &str,
            position: SequenceNumber,
        ) {
            self.record(LogEvent::Report {
                consumer: consumer.to_owned(),
                position: position.0,
            });
            self.inner
                .report_consumer_position(shard_id, consumer, position);
        }
    }

    async fn wait_until(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cond()
    }

    fn last_applied(store: &CompositionStore, shard_id: ShardId) -> u64 {
        store
            .with_storage_locked(|s| s.last_applied_seq(shard_id))
            .map_or(0, |s| s.0)
    }

    /// P3 / I-L4 — the poll loop reports the hydrator's `last_applied`
    /// via the LOCAL `report_consumer_position` seam (never the Raft
    /// register/advance path, which fails forever on followers — the
    /// `unimplemented!` stubs above are the proof: hitting either
    /// panics the task and the drain assert below fails). After
    /// draining the backlog the final reported position equals the
    /// backlog tip, every reported position carries `last_applied`
    /// (monotonic, ≤ tip), and the report reaches the underlying
    /// store's watermark machinery.
    #[tokio::test]
    async fn poll_loop_reports_last_applied_through_local_seam() {
        let store = fresh_store_with_default_ns();
        let (mem, shard_id) = fresh_log();
        append_creates(&mem, shard_id, 1_500, 0).await;
        let rec = Arc::new(RecordingLog::new(Arc::clone(&mem)));

        let registry = HydratorRegistry::new(
            Arc::clone(&store),
            Arc::clone(&rec) as Arc<dyn LogOps + Send + Sync>,
            None,
        );
        registry.register(shard_id);

        assert!(
            wait_until(5_000, || last_applied(&store, shard_id) >= 1_500).await,
            "hydrator did not drain the backlog",
        );
        assert!(
            wait_until(2_000, || rec.events().iter().any(|e| matches!(
                e,
                LogEvent::Report { consumer, position }
                    if consumer == HYDRATOR_CONSUMER && *position == 1_500
            )))
            .await,
            "no report at the drained tip observed; events: {:?}",
            rec.events(),
        );
        // Reported positions are monotonic and never past the tip.
        let mut prev = 0u64;
        for e in rec.events() {
            if let LogEvent::Report { consumer, position } = e {
                assert_eq!(consumer, HYDRATOR_CONSUMER);
                assert!(position >= prev, "reports must be monotonic");
                assert!(position <= 1_500, "reports must never pass last_applied");
                prev = position;
            }
        }
        // The report reached the underlying store's watermark
        // machinery: with the hydrator as sole consumer, truncate_log
        // prunes up to exactly the reported position (I-L4).
        let boundary = mem.truncate_log(shard_id).await.unwrap();
        assert_eq!(
            boundary,
            SequenceNumber(1_500),
            "gc boundary should follow the reported position",
        );
    }

    /// A fresh shard with nothing applied reports position 0 — which
    /// can never advance a watermark (`min > current` is false) but
    /// keeps the node visible to the leader's gather.
    #[tokio::test]
    async fn idle_shard_reports_zero_position() {
        let store = fresh_store_with_default_ns();
        let (mem, shard_id) = fresh_log();
        let rec = Arc::new(RecordingLog::new(Arc::clone(&mem)));

        let registry = HydratorRegistry::new(
            Arc::clone(&store),
            Arc::clone(&rec) as Arc<dyn LogOps + Send + Sync>,
            None,
        );
        registry.register(shard_id);

        assert!(
            wait_until(2_000, || rec
                .events()
                .iter()
                .any(|e| matches!(e, LogEvent::Report { position: 0, .. })))
            .await,
            "idle hydrator must still report (position 0); events: {:?}",
            rec.events(),
        );
        // And the poll loop keeps polling after reporting.
        assert!(
            rec.events().contains(&LogEvent::ReadDeltas),
            "poll loop must keep polling; events: {:?}",
            rec.events(),
        );
    }
}
