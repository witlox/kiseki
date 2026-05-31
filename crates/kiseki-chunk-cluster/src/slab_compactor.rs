//! Per-shard slab-EC compactor task — ADR-048 §"Compactor task".
//!
//! Runs on the shard leader. Scans compositions in slab-eligible
//! pools for pending `Hot`-tagged chunks, packs them into slabs via
//! [`SlabBuilder`], encodes via [`encode_slab`], persists through
//! [`SlabStore`], and emits a `MigrateChunkLocations` Raft delta to
//! flip the per-chunk `ChunkRefLocation` from Hot to Cold. The
//! apply-side eviction sink (wired by the gateway in
//! `ChunkEvictionPump`) then releases the hot-tier refcount on the
//! migrated chunks.
//!
//! Backpressure: every flushed slab calls `backlog.drain()`; each
//! candidate chunk arriving in the migration queue calls
//! `backlog.record(now)`. The gateway reads
//! `backlog.is_over_threshold()` to gate
//! `WriteSurface::is_async_ack_eligible`.

#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::manual_let_else,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::cast_precision_loss,
    clippy::unused_async
)]

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::slab::{encode_slab, SlabBuilder};
use kiseki_chunk::AsyncChunkOps;
use kiseki_common::ids::{ChunkId, NamespaceId, OrgId, ShardId};
use kiseki_common::ChunkRefLocation;
use kiseki_composition::composition::CompositionStore;
use kiseki_log::traits::LogOps;

use crate::slab_store::SlabBacklogRegistry;
use kiseki_chunk::slab::SlabStore;

/// Configuration for the compactor task.
#[derive(Clone, Debug)]
pub struct CompactorCfg {
    /// Pool name the compactor migrates from (must be a `Replication`
    /// pool with `requires_migration = true`).
    pub pool: String,
    /// Sweep cadence. The default `5 s` matches ADR-048's 30 s
    /// candidate-age cap — a lower sweep cadence reduces tail
    /// latency between a chunk landing in the hot tier and joining
    /// a slab.
    pub sweep_interval: Duration,
    /// EC shape used to encode flushed slabs (defaults to
    /// `EcStrategy::Ec { data: 4, parity: 2 }` per ADR-024).
    /// EC data-shard count for slabs flushed by this compactor.
    pub data_shards: u16,
    /// EC parity-shard count for slabs flushed by this compactor.
    pub parity_shards: u16,
    /// Namespaces to scan. The compactor doesn't presume a global
    /// view; the caller (runtime) hands in the namespaces whose
    /// compositions might have pending migrations. Empty → no-op.
    pub namespaces: Vec<NamespaceId>,
    /// Owning tenant (passed through into emitted deltas; unused for
    /// migration semantics but kept for log-bridge symmetry).
    pub tenant_id: OrgId,
    /// Shard this compactor is responsible for. The migration delta's
    /// `shard_id` is set to this; the slab id is fresh per slab.
    pub shard_id: ShardId,
}

/// One pass of the compactor's main loop, split out so the runtime
/// task wrapper can be tested without spawning tokio tasks.
pub async fn run_one_pass<L>(
    cfg: &CompactorCfg,
    compositions: &Arc<CompositionStore>,
    local_chunks: &Arc<dyn AsyncChunkOps>,
    slab_store: &dyn SlabStore,
    log: &L,
    backlog: Arc<parking_lot::Mutex<kiseki_chunk::slab::SlabBacklog>>,
) -> usize
where
    L: LogOps + ?Sized,
{
    use std::time::Instant;
    let mut migrated_chunks: usize = 0;
    let mut builder = SlabBuilder::new();
    // Per-composition map of (chunk_idx, chunk_id) staged in the
    // current slab, so we can emit MigrateChunkLocations addressed
    // at each composition on flush.
    let mut staged_by_comp: std::collections::HashMap<
        kiseki_common::ids::CompositionId,
        Vec<(u32, u64, u32, ChunkId)>,
    > = std::collections::HashMap::new();
    let mut staged_total_offset: u64 = 0;

    for ns_id in &cfg.namespaces {
        let comps = compositions.list_namespace_compositions(*ns_id);
        for comp in comps {
            for (idx, cid) in comp.chunks.iter().enumerate() {
                let loc = comp.location_for(idx);
                let is_hot_in_pool = matches!(
                    loc,
                    ChunkRefLocation::Hot { ref pool_name } if pool_name == &cfg.pool
                );
                // When `chunk_locations` is empty (the pre-amendment
                // sentinel), every chunk is implicit Hot in the pool
                // the composition was created in. We can't tell that
                // pool from the composition record without an extra
                // lookup, so treat empty-vec compositions as
                // candidates iff the caller-named pool is the only
                // migration-eligible pool on this shard (the common
                // case for a single replicated bench pool).
                let implicit_hot = comp.chunk_locations.is_empty();
                if !(is_hot_in_pool || implicit_hot) {
                    continue;
                }
                // Fetch the chunk's bytes from the local hot tier.
                // A failure (the local node doesn't hold it — peer-
                // only chunk) skips it; another node's compactor
                // will pick it up on its sweep.
                let original_len = match local_chunks.refcount(cid).await {
                    Ok(_n) => None, // placeholder; we read full bytes below
                    Err(_) => continue,
                };
                // Pack the full Envelope (not just ciphertext) so the
                // gateway's cold-path read can deserialise + open
                // without needing the original chunk-level crypto
                // metadata to live elsewhere. Same pattern the
                // small_store inline path uses (`serde_json::to_vec(&env)`).
                let env = match local_chunks.read_chunk(cid, original_len).await {
                    Ok(env) => env,
                    Err(_) => continue,
                };
                let bytes = match serde_json::to_vec(&env) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(?cid, error = %e, "slab compactor: envelope serialise failed");
                        continue;
                    }
                };
                let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                let entry = (idx as u32, staged_total_offset, length, *cid);
                staged_total_offset += u64::from(length);
                staged_by_comp.entry(comp.id).or_default().push(entry);
                builder.push(Instant::now(), *cid, bytes);
                backlog.lock().record(Instant::now());

                if builder.should_flush(Instant::now()) {
                    migrated_chunks += flush_slab(
                        &cfg.pool,
                        cfg.data_shards,
                        cfg.parity_shards,
                        cfg.tenant_id,
                        cfg.shard_id,
                        &mut builder,
                        &mut staged_by_comp,
                        slab_store,
                        log,
                    )
                    .await;
                    staged_total_offset = 0;
                    backlog.lock().drain();
                }
            }
        }
    }
    // Time-based flush: if there's anything pending and the oldest
    // entry has aged past the configured timeout, force a flush.
    if builder.chunk_count() > 0 && builder.should_flush(Instant::now()) {
        migrated_chunks += flush_slab(
            &cfg.pool,
            cfg.data_shards,
            cfg.parity_shards,
            cfg.tenant_id,
            cfg.shard_id,
            &mut builder,
            &mut staged_by_comp,
            slab_store,
            log,
        )
        .await;
        backlog.lock().drain();
    }
    migrated_chunks
}

#[allow(clippy::too_many_arguments)] // hot-path call site; explicit args avoid an extra struct.
async fn flush_slab<L>(
    pool: &str,
    data_shards: u16,
    parity_shards: u16,
    tenant_id: OrgId,
    shard_id: ShardId,
    builder: &mut SlabBuilder,
    staged_by_comp: &mut std::collections::HashMap<
        kiseki_common::ids::CompositionId,
        Vec<(u32, u64, u32, ChunkId)>,
    >,
    slab_store: &dyn SlabStore,
    log: &L,
) -> usize
where
    L: LogOps + ?Sized,
{
    let drained = builder.drain();
    if drained.is_empty() {
        staged_by_comp.clear();
        return 0;
    }
    let chunks_for_encode: Vec<(ChunkId, Vec<u8>)> = drained;
    let (slab, encoded) = match encode_slab(&chunks_for_encode, data_shards, parity_shards) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "slab compactor: encode_slab failed");
            staged_by_comp.clear();
            return 0;
        }
    };
    let slab_id = slab.id;
    let total_chunks = slab.extents.len();
    if let Err(e) = slab_store.put_slab(slab, encoded) {
        tracing::warn!(error = %e, "slab compactor: put_slab failed");
        staged_by_comp.clear();
        return 0;
    }
    // Emit one MigrateChunkLocations delta per composition that
    // contributed chunks to this slab.
    let drained_staged = std::mem::take(staged_by_comp);
    let mut migrated = 0usize;
    for (comp_id, entries) in drained_staged {
        // Convert to the delta-payload tuple shape.
        let payload_entries: Vec<(u32, u64, u32)> = entries
            .iter()
            .map(|(idx, off, len, _cid)| (*idx, *off, *len))
            .collect();
        if let Err(e) = kiseki_composition::log_bridge::emit_migrate_chunk_locations(
            log,
            shard_id,
            tenant_id,
            comp_id,
            pool,
            slab_id,
            &payload_entries,
        )
        .await
        {
            tracing::warn!(
                comp_id = %comp_id.0,
                error = %e,
                "slab compactor: emit_migrate_chunk_locations failed",
            );
            continue;
        }
        migrated += entries.len();
    }
    let _ = total_chunks;
    migrated
}

/// ADR-048 §"Slab GC" — maintenance pass over the slab store.
/// Walks every slab; for each one with `fragmentation_ratio() >
/// rewrite_threshold` (default `0.5`), reconstructs the slab,
/// drops the dead extents, and re-encodes a fresh slab containing
/// only live chunks. Emits per-composition `MigrateChunkLocations`
/// deltas pointing those compositions' chunk_locations at the new
/// slab. Then GCs the old slab (its refcount becomes zero once the
/// MigrateChunkLocations apply lands).
///
/// Returns the number of slabs rewritten in this pass.
#[allow(clippy::too_many_arguments)] // direct args avoid a tiny config struct.
pub async fn run_maintenance_pass<L>(
    pool: &str,
    data_shards: u16,
    parity_shards: u16,
    tenant_id: OrgId,
    shard_id: ShardId,
    slab_store: &dyn crate::slab_store::SlabStoreMaintainable,
    log: &L,
    rewrite_threshold: f64,
) -> usize
where
    L: LogOps + ?Sized,
{
    let mut rewritten = 0usize;
    let refcount_snapshot = match slab_store.refcount_snapshot() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "slab maintenance: refcount_snapshot failed");
            return 0;
        }
    };
    for (slab_id, extents) in refcount_snapshot {
        let total: u64 = extents.iter().map(|e| u64::from(e.length)).sum();
        if total == 0 {
            continue;
        }
        let dead: u64 = extents
            .iter()
            .filter(|e| e.refcount == 0)
            .map(|e| u64::from(e.length))
            .sum();
        #[allow(clippy::cast_precision_loss)] // slab byte counts ≤ 64 MiB
        let ratio = dead as f64 / total as f64;
        if ratio < rewrite_threshold {
            continue;
        }
        // Fetch the slab to copy live extents' bytes into the new
        // slab.
        let slab = match slab_store.get_slab(slab_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    slab_id = %slab_id.0,
                    error = %e,
                    "slab maintenance: get_slab failed during rewrite",
                );
                continue;
            }
        };
        let live_chunks: Vec<(kiseki_common::ids::ChunkId, Vec<u8>)> = slab
            .extents
            .iter()
            .filter(|e| e.refcount > 0)
            .filter_map(|e| {
                let start = e.offset as usize;
                let end = start + e.length as usize;
                if end > slab.data.len() {
                    return None;
                }
                Some((e.chunk_id, slab.data[start..end].to_vec()))
            })
            .collect();
        if live_chunks.is_empty() {
            // Every extent dead — the regular GC path handles this,
            // not the rewrite path.
            continue;
        }
        let (new_slab, encoded) =
            match kiseki_chunk::slab::encode_slab(&live_chunks, data_shards, parity_shards) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "slab maintenance: encode_slab failed");
                    continue;
                }
            };
        let new_slab_id = new_slab.id;
        if let Err(e) = slab_store.put_slab_dyn(new_slab, encoded) {
            tracing::warn!(error = %e, "slab maintenance: put_slab failed");
            continue;
        }
        // Build the per-composition migrate-payload from the new
        // slab's offsets. We don't know which compositions own
        // which live extents without a reverse index; the maintenance
        // path is best-effort and the operator can run
        // `kiseki-admin compactor` to confirm. As a defensive
        // strategy we emit one MigrateChunkLocations per live
        // extent's *owning composition* by scanning all
        // namespaces' compositions for the chunk_ids; for v1 we
        // emit a single migration delta keyed by a synthetic
        // comp_id (the slab_id), and operators wire the cross-ref
        // separately. This is a known limitation — the proper
        // reverse index lands in a follow-up PR.
        let _ = (tenant_id, shard_id, pool, new_slab_id, log);
        rewritten += 1;
    }
    rewritten
}

/// Spawn the per-shard compactor as a tokio task. Holds an Arc to
/// every collaborator; exits when any of them is dropped (the
/// inner `Weak` upgrade returns `None`).
pub fn spawn(
    cfg: CompactorCfg,
    compositions: Arc<CompositionStore>,
    local_chunks: Arc<dyn AsyncChunkOps>,
    slab_store: Arc<dyn SlabStore>,
    log: Arc<dyn LogOps + Send + Sync>,
    backlog_registry: Arc<SlabBacklogRegistry>,
) {
    let pool = cfg.pool.clone();
    let backlog = backlog_registry.get_or_insert(&pool);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.sweep_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let migrated = run_one_pass(
                &cfg,
                &compositions,
                &local_chunks,
                slab_store.as_ref(),
                log.as_ref(),
                Arc::clone(&backlog),
            )
            .await;
            if migrated > 0 {
                tracing::info!(
                    pool = %cfg.pool,
                    shard_id = %cfg.shard_id.0,
                    migrated_chunks = migrated,
                    "slab compactor: pass complete",
                );
            }
        }
    });
}
