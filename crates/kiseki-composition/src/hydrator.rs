//! Composition hydrator (Phase 16e).
//!
//! Followers reconstruct their `CompositionStore` from the Raft-replicated
//! delta log. The hydrator polls the log for new `Create` deltas, decodes
//! the composition payload (`encode_composition_delta_payload` /
//! `decode_composition_delta_payload`), and calls
//! `CompositionStore::create_at` to install each one locally.
//!
//! The view stream processor in `kiseki-view` solves the same
//! consume-deltas-and-update-local-state problem for views; the hydrator
//! is the composition-side analogue. We don't reuse `kiseki_view::DeltaHandler`
//! because that trait is sync, and the gateway's `CompositionStore` lives
//! behind a `tokio::sync::Mutex` (held across awaits in the read path).
//! Calling `blocking_lock()` from the async stream-processor task would
//! panic; an async poll loop is the natural shape.
//!
//! Idempotent: applying the same delta twice is a no-op (`create_at`
//! short-circuits when the composition already exists). Crash-safe by
//! virtue of `last_applied`: a hydrator restart resumes from `seq+1`.

use std::sync::Arc;

use kiseki_common::ids::{SequenceNumber, ShardId};
use kiseki_log::delta::OperationType;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};
use tokio::sync::Mutex;

use crate::composition::{decode_composition_delta_payload, CompositionStore};

/// Polls the Raft delta log and applies composition-create records to a
/// follower's local store.
pub struct CompositionHydrator {
    compositions: Arc<Mutex<CompositionStore>>,
    last_applied: SequenceNumber,
}

impl CompositionHydrator {
    /// Create a new hydrator.
    ///
    /// The store is shared with the gateway (same `Arc`), so installations
    /// performed here are immediately visible to subsequent gateway reads.
    #[must_use]
    pub fn new(compositions: Arc<Mutex<CompositionStore>>) -> Self {
        Self {
            compositions,
            last_applied: SequenceNumber(0),
        }
    }

    /// Last applied sequence number (for telemetry / tests).
    #[must_use]
    pub fn last_applied(&self) -> SequenceNumber {
        self.last_applied
    }

    /// Poll one shard's log for new deltas and apply
    /// composition-create records. Returns the number of compositions
    /// installed in this poll. Errors are swallowed and logged: hydration
    /// is best-effort, the next poll will retry.
    pub async fn poll<L: LogOps + ?Sized>(&mut self, log: &L, shard_id: ShardId) -> u64 {
        let from = SequenceNumber(self.last_applied.0.saturating_add(1));
        // Bounded batch to keep the lock-hold time small in busy clusters.
        let to = SequenceNumber(from.0.saturating_add(999));

        let deltas = match log
            .read_deltas(ReadDeltasRequest { shard_id, from, to })
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error=%e, shard=%shard_id.0, "composition hydrator: read_deltas failed");
                return 0;
            }
        };

        if deltas.is_empty() {
            return 0;
        }

        tracing::debug!(
            count = deltas.len(),
            from = from.0,
            "composition hydrator: read deltas",
        );

        let mut applied: u64 = 0;
        let mut store = self.compositions.lock().await;
        for delta in &deltas {
            // Only Create deltas carry composition-install payloads.
            if delta.header.operation == OperationType::Create {
                if let Some((comp_id, namespace_id, size)) =
                    decode_composition_delta_payload(&delta.payload.ciphertext)
                {
                    match store.create_at(
                        comp_id,
                        namespace_id,
                        delta.header.chunk_refs.clone(),
                        size,
                    ) {
                        Ok(()) => applied += 1,
                        Err(e) => {
                            tracing::debug!(
                                error=%e, comp_id=%comp_id.0, ns=%namespace_id.0,
                                "composition hydrator: create_at failed (will retry on next poll)",
                            );
                        }
                    }
                }
            }
            // Advance regardless of operation so we don't re-scan the
            // same prefix forever. Update / Delete are out of scope for
            // Phase 16e; a future iteration adds them symmetrically.
            self.last_applied = delta.header.sequence;
        }
        if applied > 0 {
            tracing::info!(
                applied,
                last_applied = self.last_applied.0,
                "composition hydrator: installed compositions from log",
            );
        }
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition::{encode_composition_delta_payload, CompositionOps, CompositionStore};
    use crate::namespace::Namespace;
    use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, NodeId, OrgId, ShardId};
    use kiseki_log::delta::OperationType;
    use kiseki_log::shard::ShardConfig;
    use kiseki_log::traits::{AppendDeltaRequest, LogOps};
    use kiseki_log::MemShardStore;

    fn fresh_store_with_default_ns() -> Arc<Mutex<CompositionStore>> {
        let mut store = CompositionStore::new();
        let bootstrap_tenant = OrgId(uuid::Uuid::from_u128(1));
        let bootstrap_ns = NamespaceId(uuid::Uuid::from_u128(2));
        let bootstrap_shard = ShardId(uuid::Uuid::from_u128(1));
        store.add_namespace(Namespace {
            id: bootstrap_ns,
            tenant_id: bootstrap_tenant,
            shard_id: bootstrap_shard,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        Arc::new(Mutex::new(store))
    }

    fn now_timestamp() -> kiseki_common::time::DeltaTimestamp {
        kiseki_common::time::DeltaTimestamp {
            hlc: kiseki_common::time::HybridLogicalClock {
                physical_ms: 0,
                logical: 0,
                node_id: NodeId(0),
            },
            wall: kiseki_common::time::WallTime {
                millis_since_epoch: 0,
                timezone: "UTC".into(),
            },
            quality: kiseki_common::time::ClockQuality::Ntp,
        }
    }

    fn fresh_log() -> (MemShardStore, ShardId) {
        let log = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant = OrgId(uuid::Uuid::from_u128(1));
        log.create_shard(shard_id, tenant, NodeId(1), ShardConfig::default());
        (log, shard_id)
    }

    async fn append_create(
        log: &MemShardStore,
        shard_id: ShardId,
        payload: Vec<u8>,
        chunk_refs: Vec<ChunkId>,
    ) {
        log.append_delta(AppendDeltaRequest {
            shard_id,
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            operation: OperationType::Create,
            timestamp: now_timestamp(),
            hashed_key: [0u8; 32],
            chunk_refs,
            payload,
            has_inline_data: false,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn hydrator_installs_composition_from_create_delta() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();

        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let chunk_id = ChunkId([7u8; 32]);
        let payload = encode_composition_delta_payload(comp_id, ns_id, 1024);
        append_create(&log, shard_id, payload, vec![chunk_id]).await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 1);

        let s = store.lock().await;
        let got = s.get(comp_id).unwrap();
        assert_eq!(got.namespace_id, ns_id);
        assert_eq!(got.size, 1024);
        assert_eq!(got.chunks, vec![chunk_id]);
    }

    #[tokio::test]
    async fn hydrator_is_idempotent_across_repeated_polls() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let payload = encode_composition_delta_payload(comp_id, ns_id, 42);
        append_create(&log, shard_id, payload, vec![]).await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 1);
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        assert_eq!(store.lock().await.count(), 1);
    }

    #[tokio::test]
    async fn hydrator_skips_deltas_with_legacy_payload_shape() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        // Old shape: 16-byte UUID only. Hydrator should skip without
        // crashing and advance past it so the loop doesn't get stuck.
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        append_create(&log, shard_id, comp_id.0.as_bytes().to_vec(), vec![]).await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        assert_eq!(hydrator.last_applied().0, 1);
    }
}
