//! Composition hydrator (Phase 16f → ADR-040 rev 2).
//!
//! Followers reconstruct their `CompositionStore` from the Raft-
//! replicated delta log. The hydrator polls the log for new `Create`,
//! `Update`, and `Delete` deltas, decodes the payload, and applies
//! the resulting state changes through `CompositionStorage::apply_hydration_batch`
//! (a single redb transaction per poll — atomic under crash, I-CP1).
//!
//! Per ADR-040 §D5.1 + I-CP6, each delta has one of three outcomes:
//!
//!   - **Applied**: the data change committed (or was a no-op such as
//!     `Rename`/`SetAttribute`/`Finalize`); advance `last_applied_seq`.
//!   - **Permanent skip**: the delta is structurally un-applyable (bad
//!     payload length, decode error, unknown discriminator); advance,
//!     warn, increment `kiseki_composition_hydrator_skip_total{reason}`.
//!   - **Transient skip**: an upstream condition (namespace not yet
//!     replicated, prior Create not yet applied for an Update) is
//!     expected to clear; do **not** advance, retry on the next poll.
//!     After `KISEKI_HYDRATOR_TRANSIENT_RETRIES` consecutive transient
//!     skips on the same delta (default 100, ≈10 s at 100 ms cadence),
//!     promote to a permanent skip with `reason="exhausted_retries"`
//!     and emit `kiseki_composition_hydrator_stalled = 1`.
//!
//! The retry counter is durable — persisted alongside `last_applied_seq`
//! in the same redb transaction (I-1 / N-1 closure) — so a crash-loop
//! accumulates retries reliably and the alarm fires after the threshold
//! regardless of process restarts.
//!
//! ADR-040 §D6.3 self-defense: if the response from `read_deltas` shows
//! a sequence gap (the first delta's sequence > `last_applied + 1`, or
//! the response is empty but `shard_health.tip > last_applied`), the
//! log has been compacted past us. The hydrator enters halt mode:
//! emits one throttled `tracing::error!`, sets
//! `kiseki_composition_hydrator_stalled = 1`, stops polling. Existing
//! reads still serve from the persistent store. Recovery is operator-
//! driven (drop the metadata redb + restart).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kiseki_common::ids::{CompositionId, SequenceNumber, ShardId};
use kiseki_log::delta::OperationType;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};
use tokio::sync::Mutex;

use crate::composition::{
    decode_composition_create_payload, decode_composition_delete_payload,
    decode_composition_update_payload, Composition, CompositionStore, INLINE_DATA_THRESHOLD,
};
use crate::metrics::{skip_reason, CompositionMetrics};
use crate::persistent::HydrationBatch;

/// In-progress staging state for a single poll's batch. Lets staging
/// functions see the effects of earlier deltas in the same batch
/// (e.g. Update of a comp that was Created earlier in the same poll
/// — the Update needs to see the staged Create, not the empty redb).
#[derive(Default)]
struct Staging {
    /// `comp_id` → composition value, keyed for in-batch lookup.
    /// `puts` and `removes` are mutually exclusive: a remove
    /// supersedes any earlier put in the same batch.
    puts: HashMap<CompositionId, Composition>,
    /// Composition ids scheduled for delete in this batch.
    removes: HashSet<CompositionId>,
}

impl Staging {
    /// Current view of a composition, considering in-batch staging
    /// over the durable storage state.
    fn view(&self, store: &CompositionStore, id: CompositionId) -> Option<Composition> {
        if self.removes.contains(&id) {
            return None;
        }
        if let Some(comp) = self.puts.get(&id) {
            return Some(comp.clone());
        }
        store.storage().get(id).ok().flatten()
    }

    fn put(&mut self, comp: Composition) {
        self.removes.remove(&comp.id);
        self.puts.insert(comp.id, comp);
    }

    fn remove(&mut self, id: CompositionId) {
        self.puts.remove(&id);
        self.removes.insert(id);
    }
}

/// Per-poll outcome for a single delta. See ADR-040 §D5.1 + I-CP6.
#[derive(Debug, Clone)]
enum DeltaOutcome {
    /// The state change is staged into the batch (or is a hydrator-
    /// no-op like Rename); advance past this delta.
    Applied,
    /// The delta is structurally un-applyable; advance past it but
    /// log + count via `kiseki_composition_hydrator_skip_total{reason}`.
    PermanentSkip { reason: &'static str },
    /// An upstream condition will clear; do not advance, retry on
    /// next poll.
    TransientSkip { reason: &'static str },
}

/// Default for `KISEKI_HYDRATOR_TRANSIENT_RETRIES` per ADR-040 §D5.1.
pub const DEFAULT_TRANSIENT_RETRY_THRESHOLD: u32 = 100;

fn read_transient_retry_threshold() -> u32 {
    std::env::var("KISEKI_HYDRATOR_TRANSIENT_RETRIES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TRANSIENT_RETRY_THRESHOLD)
}

/// Polls the Raft delta log and applies composition-create / update /
/// delete records to a follower's local persistent store.
pub struct CompositionHydrator {
    compositions: Arc<Mutex<CompositionStore>>,
    /// Cache of the durable `last_applied_seq` so most polls don't
    /// pay a redb read for the meta key. Refreshed on apply.
    last_applied_cache: SequenceNumber,
    /// Cache of the durable halt flag so a halted hydrator skips the
    /// poll without acquiring the outer Mutex.
    halted_cache: bool,
    transient_retry_threshold: u32,
    /// §D10 metrics surface. Optional so unit tests get no-op behavior.
    metrics: Option<Arc<CompositionMetrics>>,
}

impl CompositionHydrator {
    /// Create a new hydrator.
    ///
    /// The store is shared with the gateway (same `Arc`), so
    /// installations performed here are immediately visible to
    /// subsequent gateway reads. Reads `last_applied_seq` and `halted`
    /// from the durable store synchronously to seed the in-memory
    /// caches; if the store can't be read at construction time
    /// (very unlikely), defaults to seq=0 / halted=false and the
    /// next poll re-checks.
    #[must_use]
    pub fn new(compositions: Arc<Mutex<CompositionStore>>) -> Self {
        let (last_applied_cache, halted_cache) = if let Ok(guard) = compositions.try_lock() {
            let last = guard
                .storage()
                .last_applied_seq()
                .unwrap_or(SequenceNumber(0));
            let halted = guard.storage().halted().unwrap_or(false);
            (last, halted)
        } else {
            (SequenceNumber(0), false)
        };
        Self {
            compositions,
            last_applied_cache,
            halted_cache,
            transient_retry_threshold: read_transient_retry_threshold(),
            metrics: None,
        }
    }

    /// Attach the §D10 metrics surface. Subsequent polls emit
    /// `apply_duration` / `last_applied_seq{shard}` /
    /// `skip_total{reason}` / `stalled`. The runtime constructs one
    /// shared `CompositionMetrics` and clones the Arc into both the
    /// hydrator and the persistent storage.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<CompositionMetrics>) -> Self {
        // If we boot already halted, surface the stalled gauge
        // immediately — the alarm should fire on a halted-at-startup
        // process before any poll runs.
        if self.halted_cache {
            metrics.hydrator_stalled.set(1);
        }
        // Same for last_applied: surface the durable seq right away
        // so dashboards don't show 0 between boot and first apply.
        // Shard label is unknown at this point (per-shard gauge is set
        // on first poll), so this is a no-op until poll runs.
        self.metrics = Some(metrics);
        self
    }

    /// Last applied sequence number (cached; durable copy is in the
    /// store's `meta.last_applied_seq`).
    #[must_use]
    pub fn last_applied(&self) -> SequenceNumber {
        self.last_applied_cache
    }

    /// Whether the hydrator is in halt mode (cached).
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted_cache
    }

    /// Poll one shard's log for new deltas and apply them. Returns
    /// the number of state changes that committed in this poll.
    /// Errors are swallowed and logged at debug — hydration is best-
    /// effort, the next poll retries.
    ///
    /// The function is one logical sequence (read deltas → gap-detect
    /// → stage each delta → apply atomic batch → refresh caches) and
    /// doesn't decompose cleanly. Splitting would obscure the data
    /// flow more than it would help.
    #[allow(clippy::too_many_lines)]
    pub async fn poll<L: LogOps + ?Sized>(&mut self, log: &L, shard_id: ShardId) -> u64 {
        // Cheap cache check; the durable flag was read into halted_cache
        // either at boot or by the prior poll's commit.
        if self.halted_cache {
            // Throttled error log every ~60 s — implementer can refine
            // with a proper rate limiter; for now we'll let runtime-
            // owned tracing handle suppression.
            tracing::error!(
                shard = %shard_id.0,
                last_applied = self.last_applied_cache.0,
                "composition hydrator: halted (compaction outran us); operator must drop metadata redb + restart",
            );
            return 0;
        }

        let from = SequenceNumber(self.last_applied_cache.0.saturating_add(1));
        // Bounded batch to keep redb txn duration reasonable.
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

        // §D6.3 gap detection — without `LogOps::earliest_visible_seq`
        // the rule is: non-empty + first.seq > last+1 means compaction
        // ate the deltas in between; or empty + tip > last means same.
        if let Some(first) = deltas.first() {
            if first.header.sequence.0 > from.0 {
                return self
                    .enter_halt_mode(shard_id, from.0, first.header.sequence.0)
                    .await;
            }
        } else {
            // Empty: check shard tip via shard_health.
            if let Ok(info) = log.shard_health(shard_id).await {
                if info.tip.0 > self.last_applied_cache.0 {
                    return self.enter_halt_mode(shard_id, from.0, info.tip.0 + 1).await;
                }
            }
            // Transient `shard_health` failure or genuine no-new-deltas:
            // both are fine. Sleep until the next poll.
            return 0;
        }

        tracing::debug!(
            count = deltas.len(),
            from = from.0,
            "composition hydrator: read deltas",
        );

        let mut store = self.compositions.lock().await;

        let prior_stuck_state = store.storage().stuck_state().ok().flatten();

        let mut staging = Staging::default();
        let mut last_applied_in_batch = self.last_applied_cache;
        let mut applied_count: u64 = 0;
        let mut new_stuck_state: Option<(SequenceNumber, u32)> = None;
        let mut stop_at_first_transient = false;

        for delta in &deltas {
            if stop_at_first_transient {
                break;
            }
            let outcome = match delta.header.operation {
                OperationType::Create => stage_create(&store, &mut staging, delta),
                OperationType::Update => stage_update(&store, &mut staging, delta),
                OperationType::Delete => stage_delete(&mut staging, delta),
                // Rename, SetAttribute, Finalize aren't installed by
                // the hydrator. Treat as Applied so the seq advances
                // and we don't infinite-loop.
                _ => DeltaOutcome::Applied,
            };
            match outcome {
                DeltaOutcome::Applied => {
                    last_applied_in_batch = delta.header.sequence;
                    applied_count += 1;
                }
                DeltaOutcome::PermanentSkip { reason } => {
                    tracing::warn!(
                        reason,
                        seq = delta.header.sequence.0,
                        "composition hydrator: permanent skip",
                    );
                    if let Some(ref m) = self.metrics {
                        m.hydrator_skip_total.with_label_values(&[reason]).inc();
                    }
                    last_applied_in_batch = delta.header.sequence;
                }
                DeltaOutcome::TransientSkip { reason } => {
                    let (current_at, current_retries) = match prior_stuck_state {
                        Some((s, r)) if s == delta.header.sequence => (s, r),
                        _ => (delta.header.sequence, 0),
                    };
                    let new_retries = current_retries.saturating_add(1);
                    if new_retries >= self.transient_retry_threshold {
                        tracing::error!(
                            reason,
                            seq = current_at.0,
                            retries = new_retries,
                            "composition hydrator: exhausted retries — promoting to permanent skip",
                        );
                        // Permanent skip path: advance past and clear
                        // stuck state. Stalled flag stays — operator
                        // intervention required to fully clear.
                        if let Some(ref m) = self.metrics {
                            m.hydrator_skip_total
                                .with_label_values(&[skip_reason::EXHAUSTED_RETRIES])
                                .inc();
                        }
                        last_applied_in_batch = delta.header.sequence;
                        new_stuck_state = None;
                    } else {
                        tracing::debug!(
                            reason,
                            seq = current_at.0,
                            retries = new_retries,
                            "composition hydrator: transient skip — will retry",
                        );
                        new_stuck_state = Some((current_at, new_retries));
                        stop_at_first_transient = true;
                    }
                }
            }
        }

        // Build the batch. Stuck state semantics:
        //   - Some(Some(_)): we just stuck → record it.
        //   - Some(None): no stuck state → clear (we made forward progress).
        let stuck_state_update = if stop_at_first_transient {
            Some(new_stuck_state)
        } else {
            // No transient skip blocked us → clear any prior stuck
            // state. (If there was none, this is a no-op.)
            Some(None)
        };

        let batch = HydrationBatch {
            puts: staging.puts.into_values().collect(),
            removes: staging.removes.into_iter().collect(),
            new_last_applied_seq: last_applied_in_batch,
            stuck_state: stuck_state_update,
            halted: None,
        };

        // §D10: time the atomic redb commit, labeled by shard. The
        // PersistentRedbStorage layer separately tracks commit errors
        // (redb_commit_errors_total) so we don't need to here.
        let timer = self.metrics.as_ref().map(|m| {
            m.hydrator_apply_duration
                .with_label_values(&[&shard_id.0.to_string()])
                .start_timer()
        });
        let apply_result = store.storage_mut().apply_hydration_batch(batch);
        drop(timer); // Stop the histogram timer before logging.
        if let Err(e) = apply_result {
            // Commit failed (disk full, redb commit error, etc.). Don't
            // advance the cache; next poll retries. The redb commit
            // error counter was already incremented by the storage
            // layer's record_commit_error helper.
            tracing::warn!(error=%e, "composition hydrator: apply batch failed");
            return 0;
        }

        // Refresh in-memory caches from the durable state we just
        // committed. Keeps the next poll's gap-detection rule honest.
        self.last_applied_cache = last_applied_in_batch;
        if let Some(ref m) = self.metrics {
            m.hydrator_last_applied_seq
                .with_label_values(&[&shard_id.0.to_string()])
                .set(i64::try_from(last_applied_in_batch.0).unwrap_or(i64::MAX));
        }

        if applied_count > 0 {
            tracing::info!(
                applied = applied_count,
                last_applied = self.last_applied_cache.0,
                "composition hydrator: installed compositions from log",
            );
        }
        applied_count
    }

    async fn enter_halt_mode(
        &mut self,
        shard_id: ShardId,
        expected_seq: u64,
        first_visible_seq: u64,
    ) -> u64 {
        tracing::error!(
            shard = %shard_id.0,
            last_applied = self.last_applied_cache.0,
            expected_next = expected_seq,
            first_visible = first_visible_seq,
            "composition hydrator: gap detected — log compaction outran us; entering halt mode",
        );
        // Persist halt flag so subsequent restarts also short-circuit.
        let mut store = self.compositions.lock().await;
        let batch = HydrationBatch {
            puts: Vec::new(),
            removes: Vec::new(),
            new_last_applied_seq: self.last_applied_cache,
            stuck_state: None,
            halted: Some(true),
        };
        let _ = store.storage_mut().apply_hydration_batch(batch);
        self.halted_cache = true;
        if let Some(ref m) = self.metrics {
            m.hydrator_stalled.set(1);
        }
        0
    }
}

// ---------------------------------------------------------------------------
// Per-op staging functions: decode the delta payload, push the result into
// the appropriate batch vec, and return the outcome.
// ---------------------------------------------------------------------------

fn stage_create(
    store: &CompositionStore,
    staging: &mut Staging,
    delta: &kiseki_log::delta::Delta,
) -> DeltaOutcome {
    let Some((comp_id, namespace_id, size)) =
        decode_composition_create_payload(&delta.payload.ciphertext)
    else {
        return DeltaOutcome::PermanentSkip {
            reason: "create_payload_decode",
        };
    };
    // Idempotent: if the comp is already visible (durable or in-batch
    // from a previous create in the same poll), nothing to do.
    if staging.view(store, comp_id).is_some() {
        return DeltaOutcome::Applied;
    }
    // Look up the namespace in-memory; if missing, transient
    // (Phase 18 will replicate tenant-created namespaces).
    let Some(ns) = store.namespace(namespace_id) else {
        return DeltaOutcome::TransientSkip {
            reason: "namespace_not_registered",
        };
    };
    let chunks = delta.header.chunk_refs.clone();
    let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
    staging.put(Composition {
        id: comp_id,
        tenant_id: ns.tenant_id,
        namespace_id,
        shard_id: ns.shard_id,
        chunks,
        version: 1,
        size,
        has_inline_data,
        content_type: None,
    });
    DeltaOutcome::Applied
}

fn stage_update(
    store: &CompositionStore,
    staging: &mut Staging,
    delta: &kiseki_log::delta::Delta,
) -> DeltaOutcome {
    let Some((comp_id, size)) = decode_composition_update_payload(&delta.payload.ciphertext) else {
        return DeltaOutcome::PermanentSkip {
            reason: "update_payload_decode",
        };
    };
    let chunks = delta.header.chunk_refs.clone();
    let Some(mut comp) = staging.view(store, comp_id) else {
        // Either no prior Create on this node, or a prior Delete in
        // the same batch killed it. Transient: a future poll with the
        // Create's delta replicated will let this Update go through.
        return DeltaOutcome::TransientSkip {
            reason: "update_before_create",
        };
    };
    // Idempotent: state already matches → no-op (don't double-bump
    // version on re-applied deltas).
    if comp.chunks == chunks && comp.size == size {
        return DeltaOutcome::Applied;
    }
    comp.chunks = chunks;
    comp.size = size;
    comp.version += 1;
    comp.has_inline_data =
        comp.chunks.is_empty() && comp.size > 0 && comp.size <= INLINE_DATA_THRESHOLD;
    staging.put(comp);
    DeltaOutcome::Applied
}

fn stage_delete(staging: &mut Staging, delta: &kiseki_log::delta::Delta) -> DeltaOutcome {
    let Some(comp_id) = decode_composition_delete_payload(&delta.payload.ciphertext) else {
        return DeltaOutcome::PermanentSkip {
            reason: "delete_payload_decode",
        };
    };
    staging.remove(comp_id);
    DeltaOutcome::Applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition::{
        encode_composition_create_payload, encode_composition_delete_payload,
        encode_composition_update_payload, CompositionOps, CompositionStore,
    };
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

    async fn append_delta_op(
        log: &MemShardStore,
        shard_id: ShardId,
        op: OperationType,
        payload: Vec<u8>,
        chunk_refs: Vec<ChunkId>,
    ) {
        log.append_delta(AppendDeltaRequest {
            shard_id,
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            operation: op,
            timestamp: now_timestamp(),
            hashed_key: [0u8; 32],
            chunk_refs,
            payload,
            has_inline_data: false,
        })
        .await
        .unwrap();
    }

    async fn append_create(
        log: &MemShardStore,
        shard_id: ShardId,
        payload: Vec<u8>,
        chunk_refs: Vec<ChunkId>,
    ) {
        append_delta_op(log, shard_id, OperationType::Create, payload, chunk_refs).await;
    }

    #[tokio::test]
    async fn hydrator_installs_composition_from_create_delta() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();

        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let chunk_id = ChunkId([7u8; 32]);
        let payload = encode_composition_create_payload(comp_id, ns_id, 1024);
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
        let payload = encode_composition_create_payload(comp_id, ns_id, 42);
        append_create(&log, shard_id, payload, vec![]).await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 1);
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        assert_eq!(store.lock().await.count().unwrap(), 1);
    }

    #[tokio::test]
    async fn hydrator_skips_deltas_with_legacy_payload_shape() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        // Wrong-length payload for a Create op. Hydrator should skip
        // without crashing and advance past it so the loop doesn't get
        // stuck. The exact length is unimportant — anything other than
        // COMPOSITION_CREATE_PAYLOAD_LEN (40) makes the decoder return
        // None.
        append_create(&log, shard_id, vec![0u8; 5], vec![]).await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        assert_eq!(hydrator.last_applied().0, 1);
    }

    #[tokio::test]
    async fn hydrator_applies_update_delta_replaces_chunks_and_size() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));

        // Seed: Create with 1 chunk, size 100.
        let chunk_a = ChunkId([1u8; 32]);
        append_create(
            &log,
            shard_id,
            encode_composition_create_payload(comp_id, ns_id, 100),
            vec![chunk_a],
        )
        .await;

        // Update: 2 chunks, size 250.
        let chunk_b = ChunkId([2u8; 32]);
        let chunk_c = ChunkId([3u8; 32]);
        append_delta_op(
            &log,
            shard_id,
            OperationType::Update,
            encode_composition_update_payload(comp_id, 250),
            vec![chunk_b, chunk_c],
        )
        .await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 2);

        let s = store.lock().await;
        let got = s.get(comp_id).unwrap();
        assert_eq!(got.chunks, vec![chunk_b, chunk_c]);
        assert_eq!(got.size, 250);
        assert_eq!(got.version, 2, "Update should bump version once");
    }

    #[tokio::test]
    async fn hydrator_applies_delete_delta_removes_composition() {
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));

        append_create(
            &log,
            shard_id,
            encode_composition_create_payload(comp_id, ns_id, 64),
            vec![],
        )
        .await;
        append_delta_op(
            &log,
            shard_id,
            OperationType::Delete,
            encode_composition_delete_payload(comp_id),
            vec![],
        )
        .await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(hydrator.poll(&log, shard_id).await, 2);

        let s = store.lock().await;
        assert!(s.get(comp_id).is_err(), "Delete should remove composition");

        // Phase 17 N-1 closure: a fresh hydrator reads `last_applied_seq`
        // from the durable store, so a restart past this seq doesn't
        // replay previously-applied deltas. The store's
        // `last_applied_seq` is now 2; h2 polls from 3 and finds nothing.
        drop(s);
        let mut h2 = CompositionHydrator::new(Arc::clone(&store));
        assert_eq!(h2.poll(&log, shard_id).await, 0);
    }

    #[tokio::test]
    async fn hydrator_transient_skip_does_not_advance_until_threshold() {
        // I-CP6 / N-1: a Create whose namespace isn't registered is
        // a TransientSkip. The hydrator does NOT advance past it, and
        // the per-delta retry counter accumulates across polls in the
        // durable stuck_state. After the threshold is exceeded, the
        // skip is promoted to permanent and the hydrator advances.
        std::env::set_var("KISEKI_HYDRATOR_TRANSIENT_RETRIES", "3");

        // Fresh store with NO namespace registered.
        let store = Arc::new(Mutex::new(CompositionStore::new()));
        let (log, shard_id) = fresh_log();

        // Create against an unregistered namespace.
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let unknown_ns = NamespaceId(uuid::Uuid::from_u128(999));
        append_create(
            &log,
            shard_id,
            encode_composition_create_payload(comp_id, unknown_ns, 100),
            vec![],
        )
        .await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        // First two polls: transient — last_applied stays at 0, retry
        // counter accumulates.
        for expected in 1..=2 {
            assert_eq!(hydrator.poll(&log, shard_id).await, 0);
            let s = store.lock().await;
            assert_eq!(s.storage().last_applied_seq().unwrap().0, 0);
            assert_eq!(
                s.storage().stuck_state().unwrap(),
                Some((SequenceNumber(1), expected))
            );
        }
        // Third poll: hits threshold (3) → promote to permanent skip,
        // advance past, clear stuck.
        assert_eq!(hydrator.poll(&log, shard_id).await, 0);
        let s = store.lock().await;
        assert_eq!(s.storage().last_applied_seq().unwrap().0, 1);
        assert_eq!(s.storage().stuck_state().unwrap(), None);

        // Cleanup the env var so other tests aren't affected.
        std::env::remove_var("KISEKI_HYDRATOR_TRANSIENT_RETRIES");
    }

    /// Stub `LogOps` that returns a configurable list of deltas + a
    /// configurable `tip`. Lets the hydrator-gap-detection test
    /// trigger §D6.3's halt-mode path without needing a log backend
    /// that supports compaction (the in-process `MemShardStore`
    /// doesn't model log truncation).
    ///
    /// Closes auditor finding A3.
    struct GapInjectingLog {
        deltas: std::sync::Mutex<Vec<Delta>>,
        tip: kiseki_common::ids::SequenceNumber,
        shard_id: ShardId,
        tenant_id: OrgId,
    }

    use kiseki_log::delta::Delta;
    use kiseki_log::shard::{ShardConfig as LogShardConfig, ShardInfo, ShardState};

    #[async_trait::async_trait]
    impl LogOps for GapInjectingLog {
        async fn append_delta(
            &self,
            _req: AppendDeltaRequest,
        ) -> Result<kiseki_common::ids::SequenceNumber, kiseki_log::error::LogError> {
            unimplemented!("test stub: hydrator never appends")
        }
        async fn read_deltas(
            &self,
            req: ReadDeltasRequest,
        ) -> Result<Vec<Delta>, kiseki_log::error::LogError> {
            let d = self.deltas.lock().unwrap();
            Ok(d.iter()
                .filter(|x| x.header.sequence >= req.from && x.header.sequence <= req.to)
                .cloned()
                .collect())
        }
        async fn shard_health(
            &self,
            _shard_id: ShardId,
        ) -> Result<ShardInfo, kiseki_log::error::LogError> {
            Ok(ShardInfo {
                shard_id: self.shard_id,
                tenant_id: self.tenant_id,
                raft_members: vec![NodeId(1)],
                leader: Some(NodeId(1)),
                tip: self.tip,
                delta_count: self.deltas.lock().unwrap().len() as u64,
                byte_size: 0,
                state: ShardState::Healthy,
                config: LogShardConfig::default(),
                range_start: [0u8; 32],
                range_end: [0xFFu8; 32],
            })
        }
        async fn set_maintenance(
            &self,
            _shard_id: ShardId,
            _enabled: bool,
        ) -> Result<(), kiseki_log::error::LogError> {
            unimplemented!()
        }
        async fn truncate_log(
            &self,
            _shard_id: ShardId,
        ) -> Result<kiseki_common::ids::SequenceNumber, kiseki_log::error::LogError> {
            unimplemented!()
        }
        async fn compact_shard(
            &self,
            _shard_id: ShardId,
        ) -> Result<u64, kiseki_log::error::LogError> {
            unimplemented!()
        }
        fn create_shard(
            &self,
            _shard_id: ShardId,
            _tenant_id: OrgId,
            _node_id: NodeId,
            _config: LogShardConfig,
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
        fn set_shard_config(&self, _shard_id: ShardId, _config: LogShardConfig) {
            unimplemented!()
        }
        async fn register_consumer(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: kiseki_common::ids::SequenceNumber,
        ) -> Result<(), kiseki_log::error::LogError> {
            unimplemented!()
        }
        async fn advance_watermark(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: kiseki_common::ids::SequenceNumber,
        ) -> Result<(), kiseki_log::error::LogError> {
            unimplemented!()
        }
    }

    fn build_delta_at_seq(seq: u64, payload: Vec<u8>) -> Delta {
        Delta {
            header: kiseki_log::delta::DeltaHeader {
                sequence: kiseki_common::ids::SequenceNumber(seq),
                shard_id: ShardId(uuid::Uuid::from_u128(1)),
                tenant_id: OrgId(uuid::Uuid::from_u128(1)),
                operation: OperationType::Create,
                timestamp: now_timestamp(),
                hashed_key: [0u8; 32],
                tombstone: false,
                chunk_refs: Vec::new(),
                payload_size: payload.len() as u32,
                has_inline_data: false,
            },
            payload: kiseki_log::delta::DeltaPayload {
                ciphertext: payload,
                auth_tag: Vec::new(),
                nonce: Vec::new(),
                system_epoch: None,
                tenant_epoch: None,
                tenant_wrapped_material: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn hydrator_halts_when_first_delta_seq_skips_past_expected() {
        // §D6.3 + I-CP5 (A3 closure): after read_deltas, if the first
        // delta's sequence > last_applied + 1, the log compacted past
        // us. Hydrator must enter halt mode.
        let store = fresh_store_with_default_ns();

        // Stub log: the only "visible" delta is at seq=10. The
        // hydrator's last_applied=0, so it polls from seq=1. With no
        // deltas in [1, 9], the first visible delta has seq=10 — gap.
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let log = GapInjectingLog {
            deltas: std::sync::Mutex::new(vec![build_delta_at_seq(
                10,
                encode_composition_create_payload(comp_id, ns_id, 1024),
            )]),
            tip: kiseki_common::ids::SequenceNumber(10),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        assert!(!hydrator.halted(), "fresh hydrator must not be halted");

        let applied = hydrator.poll(&log, ShardId(uuid::Uuid::from_u128(1))).await;
        assert_eq!(applied, 0, "halt mode must not apply anything");
        assert!(hydrator.halted(), "hydrator must enter halt mode");

        // Halt is durable — re-reading the storage's flag confirms
        // it persisted (I-CP5).
        let s = store.lock().await;
        assert!(s.storage().halted().unwrap());
    }

    #[tokio::test]
    async fn hydrator_halts_when_empty_response_but_tip_advanced() {
        // §D6.3 second case: read_deltas returns empty AND
        // shard_health.tip > last_applied → compaction has eaten
        // everything in [last_applied+1, tip]. Same halt path.
        let store = fresh_store_with_default_ns();

        let log = GapInjectingLog {
            deltas: std::sync::Mutex::new(Vec::new()), // nothing visible
            tip: kiseki_common::ids::SequenceNumber(50), // but tip says 50
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        let applied = hydrator.poll(&log, ShardId(uuid::Uuid::from_u128(1))).await;
        assert_eq!(applied, 0);
        assert!(hydrator.halted(), "empty + advanced tip must halt");
    }

    #[tokio::test]
    async fn hydrator_does_not_halt_when_caught_up_at_tip() {
        // Counter-case: empty response AND tip == last_applied →
        // genuine no-new-deltas. Must NOT halt.
        let store = fresh_store_with_default_ns();
        // Move last_applied to 5 first.
        {
            let mut s = store.lock().await;
            s.storage_mut()
                .apply_hydration_batch(HydrationBatch {
                    puts: Vec::new(),
                    removes: Vec::new(),
                    new_last_applied_seq: kiseki_common::ids::SequenceNumber(5),
                    stuck_state: Some(None),
                    halted: None,
                })
                .unwrap();
        }
        let log = GapInjectingLog {
            deltas: std::sync::Mutex::new(Vec::new()),
            tip: kiseki_common::ids::SequenceNumber(5), // we're at tip already
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };
        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        let applied = hydrator.poll(&log, ShardId(uuid::Uuid::from_u128(1))).await;
        assert_eq!(applied, 0);
        assert!(!hydrator.halted(), "caught-up steady state must not halt");
    }

    #[tokio::test]
    async fn hydrator_update_idempotent_when_state_already_matches() {
        // A redundant Update (same chunks + size as the live record)
        // is a no-op — the staging path doesn't bump version when the
        // state already matches. Mirrors `update_at`'s idempotency
        // contract from the in-memory CompositionStore impl.
        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        let comp_id = CompositionId(uuid::Uuid::new_v4());
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let chunk = ChunkId([9u8; 32]);

        // Create (size=50, no chunks).
        append_create(
            &log,
            shard_id,
            encode_composition_create_payload(comp_id, ns_id, 50),
            vec![],
        )
        .await;
        // Update to (size=50, [chunk]) — first update, bumps version to 2.
        append_delta_op(
            &log,
            shard_id,
            OperationType::Update,
            encode_composition_update_payload(comp_id, 50),
            vec![chunk],
        )
        .await;
        // Redundant Update (same chunks, same size) — should no-op.
        append_delta_op(
            &log,
            shard_id,
            OperationType::Update,
            encode_composition_update_payload(comp_id, 50),
            vec![chunk],
        )
        .await;

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store));
        hydrator.poll(&log, shard_id).await;
        let v = store.lock().await.get(comp_id).unwrap().version;
        assert_eq!(v, 2, "version should bump exactly once for two ops");
    }
}
