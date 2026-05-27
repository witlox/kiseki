//! Composition hydrator (Phase 16f → ADR-040 rev 2).
//!
//! Followers reconstruct their `CompositionStore` from the Raft-
//! replicated delta log. The hydrator polls the log for new `Create`,
//! `Update`, and `Delete` deltas, decodes the payload, and applies
//! the resulting state changes through `CompositionStorage::apply_hydration_batch`
//! (a single atomic backend batch per poll — atomic under crash, I-CP1).
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
//! in the same backend batch (I-1 / N-1 closure) — so a crash-loop
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
//! driven (wipe the metadata directory + restart).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kiseki_common::ids::{CompositionId, SequenceNumber, ShardId};
use kiseki_log::delta::OperationType;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};

use crate::composition::{
    decode_composition_create_payload_named, decode_composition_delete_payload,
    decode_composition_update_payload, Composition, CompositionStore, INLINE_DATA_THRESHOLD,
};
use crate::metrics::{skip_reason, CompositionMetrics};
use crate::persistent::HydrationBatch;
use kiseki_common::ids::NamespaceId;

/// In-progress staging state for a single poll's batch. Lets staging
/// functions see the effects of earlier deltas in the same batch
/// (e.g. Update of a comp that was Created earlier in the same poll
/// — the Update needs to see the staged Create, not the empty store).
#[derive(Default)]
struct Staging {
    /// `comp_id` → composition value, keyed for in-batch lookup.
    /// `puts` and `removes` are mutually exclusive: a remove
    /// supersedes any earlier put in the same batch.
    puts: HashMap<CompositionId, Composition>,
    /// Composition ids scheduled for delete in this batch.
    removes: HashSet<CompositionId>,
    /// Name bindings to insert on commit. Populated from Create
    /// deltas that carry a v2 name field.
    name_inserts: Vec<(NamespaceId, String, CompositionId)>,
    /// Name bindings to remove on commit. Populated from Delete
    /// deltas — looked up via reverse index since the Delete payload
    /// itself carries only the `composition_id`.
    name_removes: Vec<(NamespaceId, String)>,
    /// ADR-040 Phase 18 — namespaces to register in the in-memory
    /// `CompositionStore` wrapper before applying any Create deltas
    /// that reference them. Populated from
    /// `OperationType::NamespaceCreate` deltas. Applied via
    /// `CompositionStore::add_namespace` directly (not via the
    /// storage trait) since the namespace map lives outside the
    /// per-composition durable layer.
    namespace_inserts: Vec<crate::namespace::Namespace>,
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
        store.with_storage_locked(|s| s.get(id).ok().flatten())
    }

    fn put(&mut self, comp: Composition) {
        self.removes.remove(&comp.id);
        self.puts.insert(comp.id, comp);
    }

    fn remove(&mut self, id: CompositionId) {
        self.puts.remove(&id);
        self.removes.insert(id);
    }

    fn bind_name(&mut self, ns: NamespaceId, name: String, id: CompositionId) {
        self.name_inserts.push((ns, name, id));
    }

    fn unbind_name(&mut self, ns: NamespaceId, name: String) {
        self.name_removes.push((ns, name));
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
    compositions: Arc<CompositionStore>,
    /// Shard this hydrator is bound to. One hydrator instance polls
    /// exactly one shard; the registry spawns N instances for N shards.
    shard_id: ShardId,
    /// Cache of the durable `last_applied_seq` for `shard_id` so most
    /// polls don't pay a backend read for the meta key. Refreshed on
    /// apply.
    last_applied_cache: SequenceNumber,
    /// Cache of the durable per-shard halt flag (I-CP5b) for
    /// `self.shard_id`, so a halted hydrator skips the poll without
    /// acquiring the storage lock. Was node-global pre-PR-2 of #87.
    halted_cache: bool,
    transient_retry_threshold: u32,
    /// §D10 metrics surface. Optional so unit tests get no-op behavior.
    metrics: Option<Arc<CompositionMetrics>>,
}

impl CompositionHydrator {
    /// Create a new hydrator bound to one shard.
    ///
    /// The store is shared with the gateway (same `Arc`), so
    /// installations performed here are immediately visible to
    /// subsequent gateway reads. Reads `last_applied_seq(shard_id)`
    /// and `halted` from the durable store synchronously to seed the
    /// in-memory caches.
    #[must_use]
    pub fn new(compositions: Arc<CompositionStore>, shard_id: ShardId) -> Self {
        let (last_applied_cache, halted_cache) = compositions.with_storage_locked(|s| {
            (
                s.last_applied_seq(shard_id).unwrap_or(SequenceNumber(0)),
                // Per-shard halt read (I-CP5b, issue #87 PR-2). Was a
                // node-global flag before the amendment.
                s.halted(shard_id).unwrap_or(false),
            )
        });
        Self {
            compositions,
            shard_id,
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

    /// Poll this hydrator's bound shard for new deltas and apply
    /// them. Returns the number of state changes that committed in
    /// this poll. Errors are swallowed and logged at debug —
    /// hydration is best-effort, the next poll retries.
    ///
    /// The function is one logical sequence (read deltas → gap-detect
    /// → stage each delta → apply atomic batch → refresh caches) and
    /// doesn't decompose cleanly. Splitting would obscure the data
    /// flow more than it would help.
    #[allow(clippy::too_many_lines)]
    pub async fn poll<L: LogOps + ?Sized>(&mut self, log: &L) -> u64 {
        let shard_id = self.shard_id;
        // Cheap cache check; the durable flag was read into halted_cache
        // either at boot or by the prior poll's commit.
        if self.halted_cache {
            // Throttled error log every ~60 s — implementer can refine
            // with a proper rate limiter; for now we'll let runtime-
            // owned tracing handle suppression.
            tracing::error!(
                shard = %shard_id.0,
                last_applied = self.last_applied_cache.0,
                "composition hydrator: halted (compaction outran us); operator must wipe metadata directory + restart",
            );
            return 0;
        }

        let from = SequenceNumber(self.last_applied_cache.0.saturating_add(1));
        // Bounded batch to keep backend commit duration reasonable.
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

        // §D6.3 gap detection (amended for issue #87, 2026-05-19).
        // Halt only on *positive* evidence of compaction:
        //   - non-empty response whose first delta's seq > expected
        //     (we see a delta at S without S-1; S-1 was GC'd), or
        //   - empty response AND `earliest_visible_seq > from`
        //     (the log itself reports its lowest visible seq is past
        //     our expected next).
        // The pre-amendment rule "empty + tip > last_applied" fired
        // on benign states (snapshot install, fresh shards on busy
        // nodes, append-vs-read races) and turned a per-shard
        // transient into a cluster-wide read 503 via the node-global
        // halt flag.
        if let Some(first) = deltas.first() {
            if first.header.sequence.0 > from.0 {
                return self.enter_halt_mode(shard_id, from.0, first.header.sequence.0);
            }
        } else {
            // Empty response: only halt if the log actively confirms
            // a compaction-gap via earliest_visible_seq. `tip` alone
            // is not sufficient — see test
            // `hydrator_does_not_halt_on_advanced_tip_without_compaction_evidence`.
            if let Ok(earliest) = log.earliest_visible_seq(shard_id).await {
                if earliest.0 > from.0 {
                    return self.enter_halt_mode(shard_id, from.0, earliest.0);
                }
            }
            // Either `earliest_visible_seq` failed (transient log
            // error) or the log says nothing has been GC'd past us.
            // Both are non-halt states; retry on the next poll.
            return 0;
        }

        tracing::debug!(
            count = deltas.len(),
            from = from.0,
            "composition hydrator: read deltas",
        );

        let store: &CompositionStore = self.compositions.as_ref();

        let prior_stuck_state =
            store.with_storage_locked(|s| s.stuck_state(shard_id).ok().flatten());

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
                OperationType::Create => stage_create(store, &mut staging, delta),
                OperationType::Update => stage_update(store, &mut staging, delta),
                OperationType::Delete => stage_delete(store, &mut staging, delta),
                OperationType::NamespaceCreate => stage_namespace_create(&mut staging, delta),
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

        // ADR-040 Phase 18 — apply staged namespaces to the in-memory
        // CompositionStore wrapper BEFORE the storage batch commits.
        // The namespace map lives on the wrapper (not the per-
        // composition durable layer), so subsequent Create deltas in
        // the same poll see the registered namespace via
        // `store.namespace(...)`. `add_namespace` is idempotent —
        // re-applying the same NamespaceCreate after a hydrator
        // restart is safe (it overwrites with identical metadata).
        let namespace_inserts = std::mem::take(&mut staging.namespace_inserts);
        for ns in namespace_inserts {
            store.add_namespace(ns);
        }

        let puts: Vec<Composition> = staging.puts.into_values().collect();
        let removes: Vec<CompositionId> = staging.removes.into_iter().collect();
        // Collect the ids the LRU read-cache must drop before we move
        // `puts` / `removes` into the batch. Followers serve reads
        // through the same `CompositionStore::get` path the leader
        // does, so a stale post-Update cache entry would let a
        // follower hand back the pre-Update version forever.
        let touched_ids: Vec<CompositionId> = puts
            .iter()
            .map(|c| c.id)
            .chain(removes.iter().copied())
            .collect();
        let batch = HydrationBatch {
            shard_id,
            puts,
            removes,
            name_inserts: staging.name_inserts,
            name_removes: staging.name_removes,
            new_last_applied_seq: last_applied_in_batch,
            stuck_state: stuck_state_update,
            halted: None,
        };

        // §D10: time the atomic backend commit, labeled by shard. The
        // storage backend separately tracks commit errors
        // (`store_commit_errors_total`) so we don't need to here.
        let timer = self.metrics.as_ref().map(|m| {
            m.hydrator_apply_duration
                .with_label_values(&[&shard_id.0.to_string()])
                .start_timer()
        });
        // Apply the batch AND invalidate the LRU read-cache for every
        // touched composition id while the storage `Mutex` is still
        // held. Doing the invalidation inside the closure (rather than
        // after `with_storage_locked` returns) closes the brief
        // read-staleness window where the storage backend already
        // holds the post-batch values but the LRU still has a
        // pre-batch entry. Same lock-ordering invariant the
        // leader-side mutators (`update`, `delete`, `rename`) follow.
        let apply_result = store.with_storage_locked(|s| {
            let r = s.apply_hydration_batch(batch);
            if r.is_ok() {
                for id in &touched_ids {
                    store.invalidate_cache(*id);
                }
            }
            r
        });
        drop(timer); // Stop the histogram timer before logging.
        if let Err(e) = apply_result {
            // Commit failed (disk full, backend I/O error, etc.). Don't
            // advance the cache; next poll retries. The store commit
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

    fn enter_halt_mode(
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
        let batch = HydrationBatch {
            shard_id,
            puts: Vec::new(),
            removes: Vec::new(),
            name_inserts: Vec::new(),
            name_removes: Vec::new(),
            new_last_applied_seq: self.last_applied_cache,
            stuck_state: None,
            halted: Some(true),
        };
        let _ = self
            .compositions
            .with_storage_locked(|s| s.apply_hydration_batch(batch));
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
    let Some((comp_id, namespace_id, size, name, chunk_plaintext_lens)) =
        decode_composition_create_payload_named(&delta.payload.ciphertext)
    else {
        return DeltaOutcome::PermanentSkip {
            reason: "create_payload_decode",
        };
    };
    // Idempotent: if the comp is already visible (durable or in-batch
    // from a previous create in the same poll), nothing to do — but
    // still re-bind the name so a follower that re-applies a Create
    // delta after a name index has been wiped converges.
    if staging.view(store, comp_id).is_some() {
        if let Some(name) = name {
            staging.bind_name(namespace_id, name, comp_id);
        }
        return DeltaOutcome::Applied;
    }
    // Look up the namespace — first in this batch's staged
    // namespace_inserts (ADR-040 Phase 18: a NamespaceCreate delta
    // may immediately precede the Create delta in the same poll),
    // then in the durable in-memory store. If neither has it, fall
    // back to the transient skip — the producer side either hasn't
    // emitted the NamespaceCreate yet (rolling upgrade), or there's
    // a delta-ordering bug. Either way the next poll will retry and
    // the exhausted-retries promotion turns into a permanent skip
    // if it never resolves.
    let (tenant_id_for_comp, shard_id_for_comp) = if let Some(ns) = staging
        .namespace_inserts
        .iter()
        .find(|n| n.id == namespace_id)
    {
        (ns.tenant_id, ns.shard_id)
    } else if let Some(ns) = store.namespace(namespace_id) {
        (ns.tenant_id, ns.shard_id)
    } else {
        return DeltaOutcome::TransientSkip {
            reason: "namespace_not_registered",
        };
    };
    let chunks = delta.header.chunk_refs.clone();
    let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
    staging.put(Composition {
        id: comp_id,
        tenant_id: tenant_id_for_comp,
        namespace_id,
        shard_id: shard_id_for_comp,
        chunks,
        version: 1,
        size,
        has_inline_data,
        content_type: None,
        // For follower-hydrated compositions, the per-chunk lens
        // ride along in the v3 create-delta payload (decoded above
        // into `chunk_plaintext_lens` when present). Regular PUTs
        // emit a v2-or-earlier payload and the read path falls back
        // to MAX_PLAINTEXT_PER_CHUNK index math.
        chunk_plaintext_lens: chunk_plaintext_lens.unwrap_or_default(),
    });
    if let Some(name) = name {
        staging.bind_name(namespace_id, name, comp_id);
    }
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

fn stage_delete(
    store: &CompositionStore,
    staging: &mut Staging,
    delta: &kiseki_log::delta::Delta,
) -> DeltaOutcome {
    let Some(comp_id) = decode_composition_delete_payload(&delta.payload.ciphertext) else {
        return DeltaOutcome::PermanentSkip {
            reason: "delete_payload_decode",
        };
    };
    // Resolve the name binding (if any) so the follower's name index
    // unbinds atomically with the composition row. Without this, a
    // GET-by-key after the delete would still resolve to a vanished
    // composition_id until the next compaction.
    if let Ok(Some((ns, name))) = store.with_storage_locked(|s| s.name_for(comp_id)) {
        staging.unbind_name(ns, name);
    }
    staging.remove(comp_id);
    DeltaOutcome::Applied
}

/// ADR-040 Phase 18 — register a namespace on this follower.
fn stage_namespace_create(staging: &mut Staging, delta: &kiseki_log::delta::Delta) -> DeltaOutcome {
    let Some(ns) = crate::composition::decode_namespace_create_payload(&delta.payload.ciphertext)
    else {
        return DeltaOutcome::PermanentSkip {
            reason: "namespace_create_payload_decode",
        };
    };
    staging.namespace_inserts.push(ns);
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

    fn fresh_store_with_default_ns() -> Arc<CompositionStore> {
        let store = CompositionStore::new();
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
            tier_policy: Vec::new(),
        });
        Arc::new(store)
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

    /// F-1 RED pin (2026-05-15 GCP perf-run observation): the hydrator
    /// catches up at ~50 deltas/sec when fed a burst, observed as a
    /// 100 k-delta backlog accumulating in the seconds after a fio
    /// write phase. Symptom downstream: every subsequent
    /// protocol-agnostic write blocks waiting on Raft commit ack
    /// because the leader's mutex stack is saturated.
    ///
    /// Today the hydrator reads up to 1 000 deltas per poll
    /// (`from..from+999`) and sleeps 100 ms between polls — a
    /// theoretical ceiling of ~10 000 ops/sec. The observed 50 ops/sec
    /// puts the actual catch-up rate at 200x below the theoretical
    /// ceiling, which is more than a slow fsync can explain on its
    /// own.
    ///
    /// This test pins a bounded catch-up budget: 5 000 deltas in a
    /// burst must drain in < 5 seconds of wall clock on the same
    /// (in-memory) backend. Today the in-memory backend has no fsync
    /// cost so this test bounds the **algorithmic** ceiling — if it
    /// fails RED, the hydrator's per-delta cost is the bottleneck
    /// (not fjall fsync). If it passes locally, the GCP backlog is
    /// fjall/disk-bound and the bottleneck is in storage.
    #[tokio::test]
    async fn hydrator_drains_5k_delta_burst_within_5s() {
        const N: u64 = 5_000;

        let store = fresh_store_with_default_ns();
        let (log, shard_id) = fresh_log();
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));

        for i in 0..N {
            let comp_id = CompositionId(uuid::Uuid::from_u128(u128::from(i) + 1));
            let payload = encode_composition_create_payload(comp_id, ns_id, 64);
            append_create(&log, shard_id, payload, vec![ChunkId([0u8; 32])]).await;
        }

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        let started = std::time::Instant::now();
        let mut applied_total: u64 = 0;
        let deadline = started + std::time::Duration::from_secs(5);
        while applied_total < N && std::time::Instant::now() < deadline {
            applied_total += hydrator.poll(&log).await;
        }
        let elapsed = started.elapsed();
        assert!(
            applied_total >= N,
            "F-1: hydrator only applied {applied_total}/{N} deltas in {:.2}s — \
             the per-delta apply path is slower than the algorithmic ceiling. \
             At 5 000 deltas the in-memory backend (zero fsync, zero disk I/O) \
             should drain in well under a second. If this test stays red, \
             investigate (a) hot loops in `stage_create`, (b) lock-contention on \
             `CompositionStore::storage`, (c) per-delta tracing overhead, before \
             attributing the GCP-observed 50 ops/sec to fjall fsync.",
            elapsed.as_secs_f64(),
        );
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        assert_eq!(hydrator.poll(&log).await, 1);

        let s = store.as_ref();
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        assert_eq!(hydrator.poll(&log).await, 1);
        assert_eq!(hydrator.poll(&log).await, 0);
        assert_eq!(hydrator.poll(&log).await, 0);
        assert_eq!(store.count().unwrap(), 1);
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        assert_eq!(hydrator.poll(&log).await, 0);
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        assert_eq!(hydrator.poll(&log).await, 2);

        let s = store.as_ref();
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        assert_eq!(hydrator.poll(&log).await, 2);

        let s = store.as_ref();
        assert!(s.get(comp_id).is_err(), "Delete should remove composition");

        // Phase 17 N-1 closure: a fresh hydrator reads `last_applied_seq`
        // from the durable store, so a restart past this seq doesn't
        // replay previously-applied deltas. The store's
        // `last_applied_seq` is now 2; h2 polls from 3 and finds nothing.
        // (`s` was a shared borrow of the store; the prior `drop(&s)`
        // was a no-op caught by `clippy::dropping_references`.)
        let mut h2 = CompositionHydrator::new(Arc::clone(&store), shard_id);
        assert_eq!(h2.poll(&log).await, 0);
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
        let store = Arc::new(CompositionStore::new());
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        // First two polls: transient — last_applied stays at 0, retry
        // counter accumulates.
        for expected in 1..=2 {
            assert_eq!(hydrator.poll(&log).await, 0);
            store.with_storage_locked(|s| {
                assert_eq!(s.last_applied_seq(shard_id).unwrap().0, 0);
                assert_eq!(
                    s.stuck_state(shard_id).unwrap(),
                    Some((SequenceNumber(1), expected))
                );
            });
        }
        // Third poll: hits threshold (3) → promote to permanent skip,
        // advance past, clear stuck.
        assert_eq!(hydrator.poll(&log).await, 0);
        store.with_storage_locked(|s| {
            assert_eq!(s.last_applied_seq(shard_id).unwrap().0, 1);
            assert_eq!(s.stuck_state(shard_id).unwrap(), None);
        });

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
        /// Lowest sequence still in the log's view, after any GC.
        /// `SequenceNumber(0)` means "no compaction has occurred" —
        /// fresh shard or all-deltas-still-present states. Issue #87
        /// rule: halt only fires when this is > `last_applied + 1`.
        earliest_visible: kiseki_common::ids::SequenceNumber,
        shard_id: ShardId,
        tenant_id: OrgId,
    }

    use kiseki_log::delta::Delta;
    use kiseki_log::shard::{ShardConfig as LogShardConfig, ShardInfo, ShardState};

    // Test-stub for `LogOps`: only the two methods the hydrator
    // actually calls (`read_deltas`, `shard_health`) are real. The
    // rest are `unimplemented!()` because hitting them would mean the
    // hydrator under test took a code path the gap-detection scenarios
    // don't exercise. The `restriction` lint forbids `unimplemented!`
    // in production, but stubbing the trait this way is the cleanest
    // expression of the intent in test code.
    #[allow(clippy::unimplemented)]
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
        async fn earliest_visible_seq(
            &self,
            _shard_id: ShardId,
        ) -> Result<kiseki_common::ids::SequenceNumber, kiseki_log::error::LogError> {
            Ok(self.earliest_visible)
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
                payload_size: u32::try_from(payload.len()).unwrap_or(u32::MAX),
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
            // The visible delta starts at seq=10, so the log has GC'd
            // 1..9. earliest_visible matches what an honest log
            // backend would return — used here just for symmetry; the
            // halt fires off the non-empty-with-skip branch in this
            // scenario, not the empty-with-earliest-visible branch.
            earliest_visible: kiseki_common::ids::SequenceNumber(10),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };

        let mut hydrator =
            CompositionHydrator::new(Arc::clone(&store), ShardId(uuid::Uuid::from_u128(1)));
        assert!(!hydrator.halted(), "fresh hydrator must not be halted");

        let applied = hydrator.poll(&log).await;
        assert_eq!(applied, 0, "halt mode must not apply anything");
        assert!(hydrator.halted(), "hydrator must enter halt mode");

        // Halt is durable — re-reading the storage's per-shard flag
        // confirms it persisted (I-CP5 / I-CP5b).
        assert!(store.with_storage_locked(|s| s.halted(ShardId(uuid::Uuid::from_u128(1))).unwrap()));
    }

    /// Issue #87 (2026-05-19): the pre-amendment §D6.3 said "empty +
    /// `tip > last_applied` is a gap → halt." That predicate fires
    /// on benign states (snapshot install replaces deltas Vec while
    /// keeping tip, fresh-shard provisioning where sibling shards
    /// have appended deltas, race between `append_delta_inner`'s
    /// `tip++` and `deltas.push`). The corrected rule needs
    /// *positive* evidence that earlier sequences were GC'd —
    /// `earliest_visible_seq > last_applied + 1`. When the log
    /// reports `earliest_visible_seq == 0` (no GC, fresh shard),
    /// the hydrator MUST stay healthy regardless of how high `tip`
    /// has climbed.
    #[tokio::test]
    async fn hydrator_does_not_halt_on_advanced_tip_without_compaction_evidence() {
        let store = fresh_store_with_default_ns();

        let log = GapInjectingLog {
            deltas: std::sync::Mutex::new(Vec::new()), // hydrator's view sees no deltas
            tip: kiseki_common::ids::SequenceNumber(50), // tip says 50
            // Critical: log reports no GC has happened. This is the
            // snapshot-install / fresh-shard / append-race shape that
            // tripped the pre-amendment predicate in production.
            earliest_visible: kiseki_common::ids::SequenceNumber(0),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };

        let mut hydrator =
            CompositionHydrator::new(Arc::clone(&store), ShardId(uuid::Uuid::from_u128(1)));
        let applied = hydrator.poll(&log).await;
        assert_eq!(applied, 0);
        assert!(
            !hydrator.halted(),
            "empty deltas + advanced tip + no compaction evidence MUST NOT halt (issue #87)",
        );
        assert!(
            !store.with_storage_locked(|s| s.halted(ShardId(uuid::Uuid::from_u128(1))).unwrap()),
            "halt flag MUST NOT have been persisted (issue #87)",
        );
    }

    #[tokio::test]
    async fn hydrator_halts_when_empty_response_and_earliest_visible_past_us() {
        // Genuine compaction-gap signal: read_deltas returns empty
        // AND the log's earliest_visible_seq is already past our
        // expected next — i.e., the log itself reports that the
        // entries we wanted have been GC'd. This is the corrected
        // empty-branch halt condition (ADR-040 §D6.3 amended).
        let store = fresh_store_with_default_ns();

        let log = GapInjectingLog {
            deltas: std::sync::Mutex::new(Vec::new()),
            tip: kiseki_common::ids::SequenceNumber(50),
            // Log says: lowest seq still visible is 30. Hydrator
            // wanted to read from last_applied+1=1. 30 > 1 → gap.
            earliest_visible: kiseki_common::ids::SequenceNumber(30),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };

        let mut hydrator =
            CompositionHydrator::new(Arc::clone(&store), ShardId(uuid::Uuid::from_u128(1)));
        let applied = hydrator.poll(&log).await;
        assert_eq!(applied, 0);
        assert!(
            hydrator.halted(),
            "empty + earliest_visible past us is genuine compaction-gap, must halt",
        );
    }

    /// Issue #87 PR-2 (I-CP5b): per-shard halt scope. One shard's
    /// hydrator tripping its compaction-gap MUST NOT propagate the
    /// halt state to other shards sharing the same `CompositionStore`
    /// on the same node. Pre-PR-2 the `halted` field was a single
    /// node-global bool; a single per-shard trip 503'd every read on
    /// the node.
    #[tokio::test]
    async fn shard_halt_does_not_propagate_to_other_shards_on_same_store() {
        let store = fresh_store_with_default_ns();
        let shard_a = ShardId(uuid::Uuid::from_u128(1));
        let shard_b = ShardId(uuid::Uuid::from_u128(2));

        // Trip the halt on shard_a via a genuine compaction-gap.
        let log_a = GapInjectingLog {
            deltas: std::sync::Mutex::new(Vec::new()),
            tip: kiseki_common::ids::SequenceNumber(50),
            earliest_visible: kiseki_common::ids::SequenceNumber(30),
            shard_id: shard_a,
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };
        let mut hydrator_a = CompositionHydrator::new(Arc::clone(&store), shard_a);
        hydrator_a.poll(&log_a).await;
        assert!(hydrator_a.halted(), "precondition: shard_a halted");
        assert!(
            store.with_storage_locked(|s| s.halted(shard_a).unwrap()),
            "precondition: shard_a halt persisted",
        );

        // The blast radius assertion: shard_b is unrelated to
        // shard_a's failure and MUST NOT inherit the halt flag.
        assert!(
            !store.with_storage_locked(|s| s.halted(shard_b).unwrap()),
            "halt on shard_a MUST NOT propagate to shard_b (I-CP5b, issue #87 PR-2)",
        );

        // And a fresh hydrator for shard_b must observe halted=false
        // at boot.
        let hydrator_b = CompositionHydrator::new(Arc::clone(&store), shard_b);
        assert!(
            !hydrator_b.halted(),
            "fresh hydrator on unrelated shard_b MUST boot un-halted (I-CP5b)",
        );
    }

    #[tokio::test]
    async fn hydrator_does_not_halt_when_caught_up_at_tip() {
        // Counter-case: empty response AND tip == last_applied →
        // genuine no-new-deltas. Must NOT halt.
        let store = fresh_store_with_default_ns();
        // Move last_applied to 5 first.
        {
            store
                .with_storage_locked(|s| {
                    s.apply_hydration_batch(HydrationBatch {
                        shard_id: ShardId(uuid::Uuid::from_u128(1)),
                        puts: Vec::new(),
                        removes: Vec::new(),
                        name_inserts: Vec::new(),
                        name_removes: Vec::new(),
                        new_last_applied_seq: kiseki_common::ids::SequenceNumber(5),
                        stuck_state: Some(None),
                        halted: None,
                    })
                })
                .unwrap();
        }
        let log = GapInjectingLog {
            deltas: std::sync::Mutex::new(Vec::new()),
            tip: kiseki_common::ids::SequenceNumber(5), // we're at tip already
            earliest_visible: kiseki_common::ids::SequenceNumber(0),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        };
        let mut hydrator =
            CompositionHydrator::new(Arc::clone(&store), ShardId(uuid::Uuid::from_u128(1)));
        let applied = hydrator.poll(&log).await;
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

        let mut hydrator = CompositionHydrator::new(Arc::clone(&store), shard_id);
        hydrator.poll(&log).await;
        let v = store.get(comp_id).unwrap().version;
        assert_eq!(v, 2, "version should bump exactly once for two ops");
    }
}
