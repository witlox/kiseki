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
use kiseki_log::traits::LogOps;
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
                let _applied = hydrator.poll(log.as_ref()).await;
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// Number of shards currently registered (test/observability helper).
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active.lock().len()
    }
}
