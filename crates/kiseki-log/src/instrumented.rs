//! Instrumented `LogOps` wrapper — records prometheus metrics and
//! emits tracing spans around every trait method call.
//!
//! Wraps any concrete `LogOps` impl (`MemShardStore`,
//! `PersistentShardStore`, `RaftShardStore`) so metric recording
//! and span emission live in one place rather than being repeated
//! across every implementation. The runtime wraps its production
//! shard store with this before handing the `Arc<dyn LogOps>` out
//! to gRPC handlers.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use kiseki_common::ids::{ChunkId, NodeId, OrgId, SequenceNumber, ShardId};

use crate::delta::Delta;
use crate::error::LogError;
use crate::metrics::{outcome, LogMetrics};
use crate::raft::state_machine::ClusterChunkStateEntry;
use crate::shard::{ShardConfig, ShardInfo, ShardState};
use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps, ReadDeltasRequest};

/// Maps a `LogError` variant to its metric `outcome` label.
fn outcome_for(err: &LogError) -> &'static str {
    match err {
        LogError::ShardNotFound(_) => outcome::SHARD_NOT_FOUND,
        LogError::ForwardToLeader { .. } => outcome::FORWARD_TO_LEADER,
        _ => outcome::ERROR,
    }
}

/// `LogOps` wrapper that records `LogMetrics` and emits tracing
/// spans on every call.
///
/// Holds the inner store as `Arc<dyn LogOps>` so the wrapper is
/// non-generic and can be erased into the same trait object the
/// rest of the runtime threads around. Concrete-type access
/// (e.g. `RaftShardStore::initialize_shard` for split's leader-
/// side init) goes through a separate typed `Arc` the runtime
/// keeps alongside.
pub struct InstrumentedLogOps {
    inner: Arc<dyn LogOps + Send + Sync>,
    metrics: Arc<LogMetrics>,
}

impl InstrumentedLogOps {
    /// Build a new wrapper.
    pub fn new(inner: Arc<dyn LogOps + Send + Sync>, metrics: Arc<LogMetrics>) -> Self {
        Self { inner, metrics }
    }
}

#[async_trait]
impl LogOps for InstrumentedLogOps {
    // Hot-path methods (`append_delta`, `append_chunk_and_delta`,
    // `read_deltas`, `shard_health`, `set_maintenance`,
    // `truncate_log`, `register_consumer`, `advance_watermark`) use
    // `level = "debug"` so production runs at `RUST_LOG=warn` /
    // `info` short-circuit span creation entirely — the macro emits
    // a level check before evaluating the `fields(...)` arguments,
    // so neither the span nor its UUID-format strings are
    // materialized on the data path. Rare lifecycle methods
    // (`split_shard`, `merge_shards`) stay at the default INFO so
    // they show up in the production trace stream.
    #[tracing::instrument(level = "debug", skip(self, req), fields(shard_id = %req.shard_id.0, op = ?req.operation))]
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        let shard = req.shard_id.0.to_string();
        let started = Instant::now();
        let result = self.inner.append_delta(req).await;
        let label = match &result {
            Ok(_) => outcome::OK,
            Err(e) => outcome_for(e),
        };
        self.metrics.record_append(&shard, label, started.elapsed());
        result
    }

    /// ADR-044 — mirror of `append_delta` with the
    /// `ForwardToLeader` hint preserved. Same metric histogram
    /// bucket as `append_delta` (the slot tracks the openraft
    /// `client_write` latency, not the variant).
    #[tracing::instrument(level = "debug", skip(self, req), fields(shard_id = %req.shard_id.0, op = ?req.operation))]
    async fn append_delta_with_forwarding(
        &self,
        req: AppendDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let shard = req.shard_id.0.to_string();
        let started = Instant::now();
        let result = self.inner.append_delta_with_forwarding(req).await;
        let label = match &result {
            Ok(_) => outcome::OK,
            Err(e) => outcome_for(e),
        };
        self.metrics.record_append(&shard, label, started.elapsed());
        result
    }

    #[tracing::instrument(level = "debug", skip(self, req), fields(shard_id = %req.delta.shard_id.0))]
    async fn append_chunk_and_delta(
        &self,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let shard = req.delta.shard_id.0.to_string();
        let started = Instant::now();
        let result = self.inner.append_chunk_and_delta(req).await;
        let label = match &result {
            Ok(_) => outcome::OK,
            Err(e) => outcome_for(e),
        };
        // Same histogram bucket as plain append — the atomic
        // chunk+delta path is in the same SLO bucket as a regular
        // append (Phase 16b §"Performance").
        self.metrics.record_append(&shard, label, started.elapsed());
        result
    }

    async fn increment_chunk_refcount(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<(), LogError> {
        self.inner
            .increment_chunk_refcount(shard_id, tenant_id, chunk_id)
            .await
    }

    async fn decrement_chunk_refcount(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<bool, LogError> {
        self.inner
            .decrement_chunk_refcount(shard_id, tenant_id, chunk_id)
            .await
    }

    #[tracing::instrument(level = "debug", skip(self, req), fields(shard_id = %req.shard_id.0, from = req.from.0, to = req.to.0))]
    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let shard = req.shard_id.0.to_string();
        let started = Instant::now();
        let result = self.inner.read_deltas(req).await;
        let label = match &result {
            Ok(_) => outcome::OK,
            Err(e) => outcome_for(e),
        };
        self.metrics.record_read(&shard, label, started.elapsed());
        result
    }

    #[tracing::instrument(level = "debug", skip(self), fields(shard_id = %shard_id.0))]
    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        self.inner.shard_health(shard_id).await
    }

    #[tracing::instrument(level = "debug", skip(self), fields(shard_id = %shard_id.0))]
    async fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        self.inner.set_maintenance(shard_id, enabled).await
    }

    #[tracing::instrument(level = "debug", skip(self), fields(shard_id = %shard_id.0))]
    async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let result = self.inner.truncate_log(shard_id).await;
        if let Ok(seq) = &result {
            self.metrics
                .set_truncate_boundary(&shard_id.0.to_string(), seq.0);
        }
        result
    }

    #[tracing::instrument(level = "debug", skip(self), fields(shard_id = %shard_id.0))]
    async fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        let shard = shard_id.0.to_string();
        let started = Instant::now();
        let result = self.inner.compact_shard(shard_id).await;
        let (label, removed) = match &result {
            Ok(n) => (outcome::OK, *n),
            Err(e) => (outcome_for(e), 0),
        };
        self.metrics
            .record_compaction(&shard, label, started.elapsed(), removed);
        result
    }

    fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
        self.inner
            .create_shard(shard_id, tenant_id, node_id, config);
    }

    fn update_shard_range(&self, shard_id: ShardId, range_start: [u8; 32], range_end: [u8; 32]) {
        self.inner
            .update_shard_range(shard_id, range_start, range_end);
    }

    fn set_shard_state(&self, shard_id: ShardId, state: ShardState) {
        self.inner.set_shard_state(shard_id, state);
    }

    fn set_shard_config(&self, shard_id: ShardId, config: ShardConfig) {
        self.inner.set_shard_config(shard_id, config);
    }

    #[tracing::instrument(skip(self), fields(shard_id = %shard_id.0, new_shard_id = %new_shard_id.0))]
    fn split_shard(
        &self,
        shard_id: ShardId,
        new_shard_id: ShardId,
        node_id: NodeId,
    ) -> Result<ShardId, LogError> {
        self.inner.split_shard(shard_id, new_shard_id, node_id)
    }

    #[tracing::instrument(skip(self), fields(target = %target_shard_id.0, source = %source_shard_id.0))]
    fn merge_shards(
        &self,
        target_shard_id: ShardId,
        source_shard_id: ShardId,
    ) -> Result<(), LogError> {
        self.inner.merge_shards(target_shard_id, source_shard_id)
    }

    async fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        self.inner
            .register_consumer(shard_id, consumer, position)
            .await
    }

    #[tracing::instrument(level = "debug", skip(self), fields(shard_id = %shard_id.0, consumer, position = position.0))]
    async fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let result = self
            .inner
            .advance_watermark(shard_id, consumer, position)
            .await;
        if result.is_ok() {
            self.metrics
                .record_watermark_advance(&shard_id.0.to_string(), consumer);
        }
        result
    }

    async fn cluster_chunk_state_get(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<Option<ClusterChunkStateEntry>, LogError> {
        self.inner
            .cluster_chunk_state_get(shard_id, tenant_id, chunk_id)
            .await
    }

    async fn cluster_chunk_state_iter(
        &self,
        shard_id: ShardId,
    ) -> Result<Vec<(OrgId, ChunkId, ClusterChunkStateEntry)>, LogError> {
        self.inner.cluster_chunk_state_iter(shard_id).await
    }
}
