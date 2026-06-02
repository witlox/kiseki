//! openraft state machine for Log shards.

#![allow(clippy::doc_markdown)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::OnceLock;

use futures::TryStreamExt;
use kiseki_common::ids::{ChunkId, OrgId, SequenceNumber, ShardId};
use kiseki_common::time::HybridLogicalClock;
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine, Snapshot};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder, StoredMembership};
use prometheus::{register_int_counter_vec, IntCounterVec, Opts};
use serde::{Deserialize, Serialize};

use super::types::{LogResponse, LogTypeConfig};
use crate::delta::{Delta, DeltaHeader, DeltaPayload, OperationType};
use crate::raft_store::{IncorporateItem, LogCommand, NewChunkMeta};
use crate::watermark::ConsumerWatermarks;

/// Default cap on the recent-incorporated-seqs window (entries).
/// PART 8 hybrid bound — whichever fires first.
const DEFAULT_DEDUP_WINDOW_ENTRIES: usize = 100_000;

/// Default cap on the recent-incorporated-seqs window (milliseconds since the
/// front entry's `apply_ms`).
const DEFAULT_DEDUP_WINDOW_MS: u64 = 60_000;

/// Prometheus counter for ancient-cutoff refusals (PART 8, Finding AA).
/// `OnceLock` so multi-shard processes only register the metric once;
/// Prometheus disallows duplicate registration.
fn dedup_ancient_refused_counter() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            Opts::new(
                "kiseki_log_dedup_ancient_refused_total",
                "Per-shard count of IncorporateIntent entries refused-with-alarm \
                 because their log_index fell below the SM's ancient cutoff \
                 (PART 8 Finding AA: long-partition recovery dropping ancient \
                 intents). Non-zero indicates a recovery path is delivering \
                 intents whose log-index window has rolled off — investigate."
            ),
            &["shard"],
        )
        .expect("kiseki-log: failed to register dedup_ancient_refused counter")
    })
}

/// Read the entry-cap from `KISEKI_DEDUP_WINDOW_ENTRIES`, defaulting to
/// [`DEFAULT_DEDUP_WINDOW_ENTRIES`]. Used by `ShardSmInner::new` so every fresh
/// shard picks up the operator override; tests can flip the env var.
fn dedup_window_entries() -> usize {
    std::env::var("KISEKI_DEDUP_WINDOW_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(DEFAULT_DEDUP_WINDOW_ENTRIES)
}

/// Read the time-cap from `KISEKI_DEDUP_WINDOW_MS`, defaulting to
/// [`DEFAULT_DEDUP_WINDOW_MS`].
fn dedup_window_ms() -> u64 {
    std::env::var("KISEKI_DEDUP_WINDOW_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(DEFAULT_DEDUP_WINDOW_MS)
}

/// `cluster_chunk_state` row — Raft-replicated chunk metadata
/// (Phase 16a, D-4). Keyed by `(tenant_id, chunk_id)` so cross-
/// tenant dedup doesn't leak refcounts (I-T1; round-2 fix).
///
/// Distinct from the local `chunk_meta` redb table in ADR-022:
/// that one maps `chunk_id → (device_id, offset, size, fragment_idx)`
/// for the on-disk layout. This one is cluster-wide replication
/// metadata.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClusterChunkStateEntry {
    /// Number of compositions referencing this chunk in this tenant.
    pub refcount: u64,
    /// Node IDs holding fragments for this chunk. Replication-N has
    /// N entries; EC X+Y has X+Y entries.
    pub placement: Vec<u64>,
    /// True when refcount reached 0 — the entry is held until the
    /// next compaction prunes it (preserves audit trail across
    /// concurrent reads in flight when the decrement applied).
    pub tombstoned: bool,
    /// Apply-time millisecond stamp from the Raft log index. Used
    /// by the orphan-fragment scrub to compute the 24h TTL
    /// (Risk #5 in the implementation plan).
    pub created_ms: u64,
    /// Phase 16d step 3: pre-encode ciphertext length. Lets the
    /// read path size the decoded output exactly under EC mode
    /// instead of relying on a trim-trailing-zeros heuristic.
    /// Defaults to 0 for entries created before 16d (round-trip
    /// safe via serde's default).
    #[serde(default)]
    pub original_len: u64,
}

type C = LogTypeConfig;

/// Serializable snapshot of a shard's delta state.
#[derive(Clone, Default, Serialize, Deserialize)]
struct ShardSnapshot {
    /// Number of deltas committed.
    delta_count: u64,
    /// Current tip sequence number.
    tip: u64,
    /// Whether in maintenance mode.
    maintenance: bool,
    /// Serialized deltas.
    deltas: Vec<SerializableDelta>,
    /// Serialized consumer watermarks.
    watermarks: Vec<(String, u64)>,
    /// Shard ID bytes (if set).
    shard_id: Option<[u8; 16]>,
    /// Tenant ID bytes (if set).
    tenant_id: Option<[u8; 16]>,
    /// ADR-047 PART 8 (Finding AA): log-index ancient-cutoff watermark. Any
    /// `IncorporateIntent` whose Raft log-index falls *below* this is
    /// refused-with-alarm (the SM increments
    /// `kiseki_log_dedup_ancient_refused_total` and logs `tracing::error!`).
    /// Persisted in snapshots so a freshly-installed follower honors the same
    /// cutoff.
    ancient_cutoff_log_index: u64,
    /// ADR-047 PART 8 — the bounded recent-incorporated-seqs window. One
    /// `RecentIncorporatedEntry` per applied `IncorporateIntent` *strictly above
    /// the ancient cutoff*, oldest at front. Pruned on push when the entry-cap
    /// or time-cap fires; eviction advances `ancient_cutoff_log_index`. The
    /// authoritative gate against duplicate apply for the recent window
    /// (multi-writer late-arrival / re-fan / replay).
    recent_incorporated: Vec<RecentIncorporatedEntry>,
}

/// One entry in the recent-incorporated window. Serializable so it survives
/// `ShardSnapshot` build/install. Triple `(log_index, perspective_seq,
/// apply_ms)` per the PART 8 spec — `log_index` for eviction-by-age coupling to
/// Raft progress, `perspective_seq` for O(log N) membership lookups in the
/// in-memory `HashSet` mirror, `apply_ms` for time-bound eviction.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct RecentIncorporatedEntry {
    /// The Raft log index of the `IncorporateIntent` apply that recorded this.
    log_index: u64,
    /// The committer-assigned perspective-seq the apply gate keys on.
    perspective_seq: HybridLogicalClock,
    /// Apply-time wall-clock (ms since Unix epoch) — drives the time-bound
    /// window eviction. Sourced from the SM apply-loop's clock read, not from
    /// the delta's HLC (the delta HLC is the log index, not a wall clock).
    apply_ms: u64,
}

/// Serializable form of a Delta for snapshots.
#[derive(Clone, Serialize, Deserialize)]
struct SerializableDelta {
    sequence: u64,
    shard_id: [u8; 16],
    tenant_id: [u8; 16],
    operation: u8,
    hashed_key: [u8; 32],
    tombstone: bool,
    chunk_refs: Vec<[u8; 32]>,
    payload_size: u32,
    has_inline_data: bool,
    ciphertext: Vec<u8>,
}

impl SerializableDelta {
    fn from_delta(d: &Delta) -> Self {
        Self {
            sequence: d.header.sequence.0,
            shard_id: *d.header.shard_id.0.as_bytes(),
            tenant_id: *d.header.tenant_id.0.as_bytes(),
            operation: op_to_u8(d.header.operation),
            hashed_key: d.header.hashed_key,
            tombstone: d.header.tombstone,
            chunk_refs: d.header.chunk_refs.iter().map(|c| c.0).collect(),
            payload_size: d.header.payload_size,
            has_inline_data: d.header.has_inline_data,
            ciphertext: d.payload.ciphertext.clone(),
        }
    }

    fn to_delta(&self) -> Delta {
        Delta {
            header: DeltaHeader {
                sequence: SequenceNumber(self.sequence),
                shard_id: ShardId(uuid::Uuid::from_bytes(self.shard_id)),
                tenant_id: OrgId(uuid::Uuid::from_bytes(self.tenant_id)),
                operation: u8_to_op(self.operation),
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
                hashed_key: self.hashed_key,
                tombstone: self.tombstone,
                chunk_refs: self
                    .chunk_refs
                    .iter()
                    .map(|b| kiseki_common::ids::ChunkId(*b))
                    .collect(),
                payload_size: self.payload_size,
                has_inline_data: self.has_inline_data,
            },
            payload: DeltaPayload {
                ciphertext: self.ciphertext.clone(),
                auth_tag: Vec::new(),
                nonce: Vec::new(),
                system_epoch: None,
                tenant_epoch: None,
                tenant_wrapped_material: Vec::new(),
            },
        }
    }
}

/// Apply-time wall-clock in ms since the Unix epoch. Used for the dedup window
/// time-bound. Read on every IncorporateIntent / IncorporateIntents apply so a
/// follower's eviction tracks its own apply clock (replication lag does NOT
/// shrink the window from the leader's perspective). Returns 0 if the system
/// clock pre-dates the Unix epoch (impossible in practice; defensive).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn op_to_u8(op: OperationType) -> u8 {
    match op {
        OperationType::Create => 0,
        OperationType::Update => 1,
        OperationType::Delete => 2,
        OperationType::Rename => 3,
        OperationType::SetAttribute => 4,
        OperationType::Finalize => 5,
        OperationType::NamespaceCreate => 6,
        OperationType::MigrateChunkLocations => 7,
    }
}

fn u8_to_op(v: u8) -> OperationType {
    match v {
        0 => OperationType::Create,
        1 => OperationType::Update,
        2 => OperationType::Delete,
        3 => OperationType::Rename,
        4 => OperationType::SetAttribute,
        5 => OperationType::Finalize,
        6 => OperationType::NamespaceCreate,
        _ => OperationType::MigrateChunkLocations,
    }
}

/// Inner state for the shard state machine.
pub struct ShardSmInner {
    pub(crate) delta_count: u64,
    pub(crate) tip: u64,
    pub(crate) maintenance: bool,
    pub(crate) deltas: Vec<Delta>,
    pub(crate) watermarks: ConsumerWatermarks,
    pub(crate) shard_id: ShardId,
    pub(crate) tenant_id: OrgId,
    last_applied_log: Option<LogIdOf<C>>,
    last_membership: StoredMembershipOf<C>,
    /// Inline content store for small files (ADR-030, I-SF5).
    /// When set, inline payloads are offloaded to this store on apply
    /// and cleared from in-memory deltas.
    pub(crate) inline_store: Option<Arc<dyn kiseki_common::inline_store::InlineStore>>,
    /// `cluster_chunk_state` table (Phase 16a, D-4).
    /// Raft-replicated chunk metadata keyed by `(tenant, chunk_id)`.
    /// See `ClusterChunkStateEntry` doc for the contract.
    pub cluster_chunk_state: HashMap<(OrgId, ChunkId), ClusterChunkStateEntry>,
    /// Inclusive lower bound of the shard's hashed-key range
    /// (ADR-033 §4). Mutated through `LogCommand::UpdateShardRange`
    /// so every replica converges on the same range.
    pub(crate) range_start: [u8; 32],
    /// Exclusive upper bound of the shard's hashed-key range.
    pub(crate) range_end: [u8; 32],
    /// Shard lifecycle state (ADR-033 §3 / ADR-034). Mutated through
    /// `LogCommand::SetShardState`. `maintenance` (above) is the
    /// distinct W4 maintenance-mode flag — both can be set
    /// independently; the merged report in `shard_health` prefers
    /// `Maintenance` over the lifecycle state when the W4 flag is on.
    pub(crate) state: crate::shard::ShardState,
    /// Shard `ShardConfig` (delta + byte ceilings, inline
    /// thresholds). Mutated through `LogCommand::SetShardConfig`.
    /// The Raft-replicated copy makes auto-split triggers consistent
    /// across replicas (otherwise nodes disagree on when a shard
    /// crosses the I-L6 ceiling).
    pub(crate) config: crate::shard::ShardConfig,
    /// ADR-047 PART 8 (Finding AA): log-index ancient-cutoff watermark. An
    /// `IncorporateIntent` whose Raft log_index is *strictly below* this
    /// value is refused-with-alarm — never silently dropped. Advanced when
    /// the recent-incorporated window evicts its oldest entry.
    pub(crate) ancient_cutoff_log_index: u64,
    /// ADR-047 PART 8 — the recent-incorporated window itself, oldest at the
    /// front. One entry per applied `IncorporateIntent` strictly above the
    /// ancient cutoff. Bounded by `dedup_window_entries` and `dedup_window_ms`
    /// (whichever fires first); on eviction the front entry's log_index + 1
    /// becomes the new ancient cutoff.
    pub(crate) recent_incorporated: VecDeque<RecentIncorporatedEntry>,
    /// Mirror of the perspective-seq set in `recent_incorporated`, for O(1)
    /// membership checks in the apply gate. Kept strictly in sync with the
    /// deque: every push_back inserts here, every pop_front removes here.
    /// Rebuilt from the deque in `install_snapshot`.
    pub(crate) recent_incorporated_seqs: HashSet<HybridLogicalClock>,
    /// Per-shard cap on `recent_incorporated.len()`. Snapshot at SM
    /// construction so a deployment-time env-var change picks up on restart;
    /// not replicated (the cap is a local-policy knob, not consensus state).
    pub(crate) dedup_window_entries: usize,
    /// Per-shard cap on `apply_ms` age (ms) before the front entry evicts.
    pub(crate) dedup_window_ms: u64,
}

impl ShardSmInner {
    pub(crate) fn new(shard_id: ShardId, tenant_id: OrgId) -> Self {
        Self {
            delta_count: 0,
            tip: 0,
            maintenance: false,
            deltas: Vec::new(),
            watermarks: ConsumerWatermarks::new(),
            shard_id,
            tenant_id,
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
            inline_store: None,
            cluster_chunk_state: HashMap::new(),
            // Default range covers the full 256-bit key space — a
            // single-shard cluster owns everything until split.
            range_start: [0u8; 32],
            range_end: [0xFFu8; 32],
            state: crate::shard::ShardState::Healthy,
            config: crate::shard::ShardConfig::default(),
            ancient_cutoff_log_index: 0,
            recent_incorporated: VecDeque::new(),
            recent_incorporated_seqs: HashSet::new(),
            dedup_window_entries: dedup_window_entries(),
            dedup_window_ms: dedup_window_ms(),
        }
    }

    /// Construct with explicit dedup-window bounds (test-only constructor).
    /// Production picks bounds from env vars in [`Self::new`].
    #[cfg(test)]
    pub(crate) fn new_with_bounds(
        shard_id: ShardId,
        tenant_id: OrgId,
        window_entries: usize,
        window_ms: u64,
    ) -> Self {
        let mut s = Self::new(shard_id, tenant_id);
        s.dedup_window_entries = window_entries.max(1);
        s.dedup_window_ms = window_ms.max(1);
        s
    }

    /// O(1) membership check used by the SM apply gate.
    pub(crate) fn dedup_contains(&self, seq: &HybridLogicalClock) -> bool {
        self.recent_incorporated_seqs.contains(seq)
    }

    /// Snapshot the perspective-seqs currently in the recent-incorporated set
    /// — read by the supervisor's per-intent self-prune so it removes only the
    /// intents that are *replicated and applied on this node* (PART 8 §T).
    pub(crate) fn recent_incorporated_snapshot(&self) -> HashSet<HybridLogicalClock> {
        self.recent_incorporated_seqs.clone()
    }

    /// PART 8 §1 — push a newly-incorporated entry into the recent window and
    /// evict the front under the hybrid bound (entries OR time, whichever
    /// fires). Advances `ancient_cutoff_log_index` to `evicted.log_index + 1`
    /// so the next ancient check correctly sees `log_idx < cutoff` for the
    /// just-evicted region.
    ///
    /// `now_ms` is the apply-time wall-clock (ms since Unix epoch); pulled
    /// from `SystemTime::now()` at the apply site for production, supplied by
    /// the test driver in unit tests.
    fn push_recent_and_evict(
        &mut self,
        log_index: u64,
        perspective_seq: HybridLogicalClock,
        now_ms: u64,
    ) {
        self.recent_incorporated.push_back(RecentIncorporatedEntry {
            log_index,
            perspective_seq,
            apply_ms: now_ms,
        });
        self.recent_incorporated_seqs.insert(perspective_seq);

        // Evict from the front while EITHER cap is exceeded. Each eviction
        // pushes the ancient cutoff forward to the evicted entry's
        // log_index + 1, so an ancient intent at that index reads as
        // strictly-below-cutoff on the next gate check.
        loop {
            let evict = if self.recent_incorporated.len() > self.dedup_window_entries {
                true
            } else if let Some(front) = self.recent_incorporated.front() {
                now_ms.saturating_sub(front.apply_ms) > self.dedup_window_ms
            } else {
                false
            };
            if !evict {
                break;
            }
            // Both length and front existence are checked above so the pop
            // can never return None — but use `if let` for clippy hygiene.
            if let Some(front) = self.recent_incorporated.pop_front() {
                self.recent_incorporated_seqs.remove(&front.perspective_seq);
                // Cutoff is exclusive — anything at-or-below the evicted index
                // is now ancient.
                self.ancient_cutoff_log_index = self
                    .ancient_cutoff_log_index
                    .max(front.log_index.saturating_add(1));
            } else {
                break;
            }
        }
    }

    /// PART 8 §1 — apply one `IncorporateItem` (single or batch element).
    /// Returns the assigned tip (the SM's monotonic `tip` after a successful
    /// append, or the unchanged tip on duplicate/refuse — the SOLE truthful
    /// `Appended` shape, matching the prior `IncorporateIntent` contract).
    ///
    /// The apply order is (atomically in a single SM-lock holding):
    ///   1. duplicate check via recent_incorporated_seqs (skip if hit),
    ///   2. ancient cutoff check (refuse-with-alarm if log_idx < cutoff),
    ///   3. atomic chunk-meta + delta append (same body as ChunkAndDelta),
    ///   4. recent_incorporated push + evict-and-advance-cutoff.
    fn apply_one_incorporate(
        &mut self,
        item: &IncorporateItem,
        log_index: u64,
        now_ms: u64,
    ) -> u64 {
        // (1) Duplicate gate — seq already in the recent window. No-op,
        // return the unchanged tip (matches the pre-PART-8 `Appended(tip)`
        // contract for replay/duplicate).
        // ADR-047 hot-path timer (sm.recent_dedup) — HashSet::contains
        // on the seq set, sub-µs but fires on every apply.
        let is_dup = kiseki_tracing::hot_span!("sm.recent_dedup", {
            self.dedup_contains(&item.perspective_seq)
        });
        if is_dup {
            return self.tip;
        }
        // (2) Ancient gate — log_index below the cutoff is suspicious; record
        // the alarm + log and refuse. This catches a long-partition recovery
        // delivering a re-gathered intent whose log-index window has rolled
        // off, per Finding AA.
        // ADR-047 hot-path timer (sm.ancient_check) — single u64
        // compare; histogram exists so a future refuse-with-alarm
        // path that fans expensive logging is observable.
        let is_ancient = kiseki_tracing::hot_span!("sm.ancient_check", {
            log_index < self.ancient_cutoff_log_index
        });
        if is_ancient {
            dedup_ancient_refused_counter()
                .with_label_values(&[&self.shard_id.0.to_string()])
                .inc();
            tracing::error!(
                shard = %self.shard_id.0,
                seq = ?item.perspective_seq,
                log_index,
                cutoff = self.ancient_cutoff_log_index,
                "intent below ancient cutoff — refused (PART 8 Finding AA)",
            );
            return self.tip;
        }
        // (3) Real apply — chunk_meta + delta, atomic in this SM lock.
        // ADR-047 hot-path timer (sm.apply_new_chunks) — chunk_meta
        // table updates (one row per new chunk).
        kiseki_tracing::hot_span!("sm.apply_new_chunks", {
            self.apply_new_chunks(&item.tenant_id_bytes, &item.new_chunks, log_index);
        });
        // #129: inline small-file payloads write to the local
        // InlineStore keyed by chunk_id (ADR-030 §2 canonical key).
        // Same as the ChunkAndDelta apply branch above — every
        // replica that applies this intent gets the bytes locally
        // so the gateway read path on ANY node finds them via
        // small_store.get(&chunk_id.0).
        if !item.inline_payloads.is_empty() {
            if let Some(ref store) = self.inline_store {
                for (chunk_id, bytes) in &item.inline_payloads {
                    if let Err(e) = store.put(chunk_id, bytes) {
                        tracing::warn!(
                            chunk_id = ?chunk_id,
                            error = %e,
                            "sm.apply_one_incorporate: inline_store.put failed (#129); fallback via chunk tier",
                        );
                    }
                }
            }
        }
        // ADR-047 hot-path timer (sm.append_delta_inner) — THE
        // per-apply cost: tip bump + delta append + (optional)
        // inline offload. Per the escalation this is one of the
        // candidates for the unattributed budget tail.
        let tip = kiseki_tracing::hot_span!("sm.append_delta_inner", {
            self.append_delta_inner(
                &item.tenant_id_bytes,
                item.operation,
                &item.hashed_key,
                &item.chunk_refs,
                &item.payload,
                item.has_inline_data,
                log_index,
            )
        });
        // (4) Record + evict — same apply block, so a follower applying this
        // same entry independently arrives at the same set state.
        // ADR-047 hot-path timer (sm.push_recent) — recent set push
        // + cutoff advance + evict. Dedup-window-size dependent.
        kiseki_tracing::hot_span!("sm.push_recent", {
            self.push_recent_and_evict(log_index, item.perspective_seq, now_ms);
        });
        tip
    }

    /// Set the inline store for small-file content offload.
    #[allow(dead_code)]
    pub(crate) fn with_inline_store(
        mut self,
        store: Arc<dyn kiseki_common::inline_store::InlineStore>,
    ) -> Self {
        self.inline_store = Some(store);
        self
    }

    #[allow(clippy::too_many_arguments)] // mirrors delta-args structure on the wire
    fn append_delta_inner(
        &mut self,
        tenant_id_bytes: &[u8; 16],
        operation: u8,
        hashed_key: &[u8; 32],
        chunk_refs: &[[u8; 32]],
        payload: &[u8],
        has_inline_data: bool,
        log_index: u64,
    ) -> u64 {
        self.tip += 1;
        self.delta_count += 1;
        let next_seq = SequenceNumber(self.tip);

        #[allow(clippy::cast_possible_truncation)]
        let payload_size = payload.len() as u32;

        let op = u8_to_op(operation);

        let timestamp = kiseki_common::time::DeltaTimestamp {
            hlc: kiseki_common::time::HybridLogicalClock {
                physical_ms: log_index,
                logical: 0,
                node_id: kiseki_common::ids::NodeId(0),
            },
            wall: kiseki_common::time::WallTime {
                millis_since_epoch: log_index,
                timezone: "UTC".into(),
            },
            quality: kiseki_common::time::ClockQuality::Ntp,
        };

        // Offload inline content to the store if available (I-SF5).
        // Key = hashed_key XOR sequence (last 8 bytes), so two deltas
        // with the same hashed_key but different sequences produce
        // different inline keys.
        let inline_key = {
            let mut k = *hashed_key;
            let seq_bytes = self.tip.to_le_bytes();
            for (i, &b) in seq_bytes.iter().enumerate() {
                k[24 + i] ^= b;
            }
            k
        };
        let ciphertext = if has_inline_data {
            if let Some(ref store) = self.inline_store {
                let _ = store.put(&inline_key, payload);
                Vec::new()
            } else {
                payload.to_vec()
            }
        } else {
            payload.to_vec()
        };

        let delta = Delta {
            header: DeltaHeader {
                sequence: next_seq,
                shard_id: self.shard_id,
                tenant_id: OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes)),
                operation: op,
                timestamp,
                hashed_key: *hashed_key,
                tombstone: operation == 2,
                chunk_refs: chunk_refs.iter().map(|b| ChunkId(*b)).collect(),
                payload_size,
                has_inline_data,
            },
            payload: DeltaPayload {
                ciphertext,
                auth_tag: Vec::new(),
                nonce: Vec::new(),
                system_epoch: None,
                tenant_epoch: None,
                tenant_wrapped_material: Vec::new(),
            },
        };

        self.deltas.push(delta);
        self.tip
    }

    /// Apply Phase 16a `cluster_chunk_state` mutations: create new
    /// entries for each `NewChunkMeta`. Idempotent on re-apply
    /// (existing key keeps its current refcount + placement).
    fn apply_new_chunks(
        &mut self,
        tenant_id_bytes: &[u8; 16],
        new_chunks: &[NewChunkMeta],
        log_index: u64,
    ) {
        let tenant = OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes));
        for nc in new_chunks {
            let key = (tenant, ChunkId(nc.chunk_id));
            self.cluster_chunk_state
                .entry(key)
                .or_insert_with(|| ClusterChunkStateEntry {
                    refcount: 1,
                    placement: nc.placement.clone(),
                    tombstoned: false,
                    created_ms: log_index,
                    original_len: nc.original_len,
                });
        }
    }

    #[allow(clippy::too_many_lines)] // Big match per LogCommand variant
    pub(crate) fn apply_command(&mut self, cmd: &LogCommand, log_index: u64) -> LogResponse {
        self.apply_command_at(cmd, log_index, now_ms())
    }

    /// Same as [`Self::apply_command`] but with an explicit `now_ms` (the
    /// apply-time wall-clock for the dedup time-bound). Production calls
    /// [`Self::apply_command`] (reads `SystemTime::now`); tests call this
    /// directly so the time-bound eviction is deterministic.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn apply_command_at(
        &mut self,
        cmd: &LogCommand,
        log_index: u64,
        now_ms: u64,
    ) -> LogResponse {
        match cmd {
            LogCommand::AppendDelta {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
            } => {
                let tip = self.append_delta_inner(
                    tenant_id_bytes,
                    *operation,
                    hashed_key,
                    chunk_refs,
                    payload,
                    *has_inline_data,
                    log_index,
                );
                LogResponse::Appended(tip)
            }
            LogCommand::ChunkAndDelta {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
                new_chunks,
                inline_payloads,
            } => {
                // Atomic per D-4 round 2: chunk_meta entries are
                // created BEFORE the delta is appended so a reader
                // observing the delta after this apply step always
                // finds the corresponding cluster_chunk_state.
                self.apply_new_chunks(tenant_id_bytes, new_chunks, log_index);
                // #129: write inline small-file payloads to the
                // local inline store keyed by chunk_id (ADR-030
                // §2 canonical key). Every replica that applies
                // this entry executes this branch, so the gateway
                // read path on ANY node finds the bytes locally
                // via `small_store.get(&chunk_id.0)`.
                if !inline_payloads.is_empty() {
                    if let Some(ref store) = self.inline_store {
                        for (chunk_id, bytes) in inline_payloads {
                            if let Err(e) = store.put(chunk_id, bytes) {
                                tracing::warn!(
                                    chunk_id = ?chunk_id,
                                    error = %e,
                                    "sm.apply: inline_store.put failed (#129); write replicated via chunk tier fallback",
                                );
                            }
                        }
                    }
                }
                let tip = self.append_delta_inner(
                    tenant_id_bytes,
                    *operation,
                    hashed_key,
                    chunk_refs,
                    payload,
                    *has_inline_data,
                    log_index,
                );
                LogResponse::Appended(tip)
            }
            // ADR-047 PART 8 — an async-committed intent. Two complementary
            // gates run inside the same `apply` lock:
            //   (1) **Recent set:** if `perspective_seq` is already in the
            //       bounded recent-incorporated set, SKIP (idempotent on
            //       re-fan / replay / recovery re-gather).
            //   (2) **Ancient cutoff:** if this entry's log_index is *strictly
            //       below* `ancient_cutoff_log_index`, REFUSE-with-alarm
            //       (`kiseki_log_dedup_ancient_refused_total` + tracing::error!)
            //       — catches a long-partition recovery delivering an intent
            //       whose dedup window has rolled off (Finding AA).
            // Otherwise: atomic chunk_meta + delta append, then push the seq
            // into the recent window and evict-and-advance-cutoff under the
            // hybrid bound (Finding Q). Atomicity (PART 8 §P) is preserved
            // because both the delta append and the set update happen in this
            // single apply-lock-held block.
            LogCommand::IncorporateIntent {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
                new_chunks,
                perspective_seq,
                inline_payloads,
            } => {
                let item = IncorporateItem {
                    tenant_id_bytes: *tenant_id_bytes,
                    operation: *operation,
                    hashed_key: *hashed_key,
                    chunk_refs: chunk_refs.clone(),
                    payload: payload.clone(),
                    has_inline_data: *has_inline_data,
                    new_chunks: new_chunks.clone(),
                    perspective_seq: *perspective_seq,
                    inline_payloads: inline_payloads.clone(),
                };
                let tip = self.apply_one_incorporate(&item, log_index, now_ms);
                LogResponse::Appended(tip)
            }
            // PART 8 §U — the batched variant. Each item runs through the
            // same per-item gate as the single variant (`apply_one_incorporate`);
            // the supervisor caps each drain pass at 1 000 items so a single
            // Raft round absorbs at most 1 000 intents. Returns the final tip
            // (post-batch), matching the single-variant shape.
            LogCommand::IncorporateIntents { items } => {
                let mut last_tip = self.tip;
                for item in items {
                    last_tip = self.apply_one_incorporate(item, log_index, now_ms);
                }
                LogResponse::Appended(last_tip)
            }
            LogCommand::IncrementChunkRefcount {
                tenant_id_bytes,
                chunk_id,
            } => {
                let tenant = OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes));
                let key = (tenant, ChunkId(*chunk_id));
                if let Some(entry) = self.cluster_chunk_state.get_mut(&key) {
                    entry.refcount = entry.refcount.saturating_add(1);
                    // A new reference revives a tombstoned entry —
                    // unusual (would mean concurrent decrement +
                    // re-create) but defensible.
                    entry.tombstoned = false;
                }
                LogResponse::Ok
            }
            LogCommand::DecrementChunkRefcount {
                tenant_id_bytes,
                chunk_id,
            } => {
                let tenant = OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes));
                let key = (tenant, ChunkId(*chunk_id));
                let mut tombstoned_now = false;
                if let Some(entry) = self.cluster_chunk_state.get_mut(&key) {
                    let was_tombstoned = entry.tombstoned;
                    entry.refcount = entry.refcount.saturating_sub(1);
                    if entry.refcount == 0 && !was_tombstoned {
                        entry.tombstoned = true;
                        tombstoned_now = true;
                    }
                }
                LogResponse::DecrementOutcome(tombstoned_now)
            }
            LogCommand::SetMaintenance { enabled } => {
                self.maintenance = *enabled;
                LogResponse::Ok
            }
            LogCommand::AdvanceWatermark { consumer, position } => {
                self.watermarks.advance(consumer, SequenceNumber(*position));
                LogResponse::Ok
            }
            LogCommand::SetShardState { state } => {
                if let Some(s) = crate::shard::ShardState::from_u8(*state) {
                    self.state = s;
                } else {
                    tracing::warn!(
                        byte = state,
                        "SetShardState: unknown ShardState byte; \
                         leaving state unchanged",
                    );
                }
                LogResponse::Ok
            }
            LogCommand::UpdateShardRange {
                range_start,
                range_end,
            } => {
                self.range_start = *range_start;
                self.range_end = *range_end;
                LogResponse::Ok
            }
            LogCommand::SetShardConfig {
                max_delta_count,
                max_byte_size,
                inline_threshold_bytes,
                inline_floor_bytes,
                inline_ceiling_bytes,
            } => {
                self.config = crate::shard::ShardConfig {
                    max_delta_count: *max_delta_count,
                    max_byte_size: *max_byte_size,
                    inline_threshold_bytes: *inline_threshold_bytes,
                    inline_floor_bytes: *inline_floor_bytes,
                    inline_ceiling_bytes: *inline_ceiling_bytes,
                };
                LogResponse::Ok
            }
        }
    }
}

/// openraft state machine for a Log shard.
#[derive(Clone)]
pub struct ShardStateMachine {
    inner: Arc<futures::lock::Mutex<ShardSmInner>>,
}

impl ShardStateMachine {
    pub(crate) fn new(inner: Arc<futures::lock::Mutex<ShardSmInner>>) -> Self {
        Self { inner }
    }
}

impl RaftSnapshotBuilder<C> for ShardStateMachine {
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C>, io::Error> {
        let inner = self.inner.lock().await;
        // Build serializable deltas, reading inline content from store
        // for deltas whose ciphertext was offloaded (I-SF5).
        let deltas: Vec<SerializableDelta> = inner
            .deltas
            .iter()
            .map(|d| {
                let mut sd = SerializableDelta::from_delta(d);
                if d.header.has_inline_data && sd.ciphertext.is_empty() {
                    if let Some(ref store) = inner.inline_store {
                        let mut inline_key = d.header.hashed_key;
                        let seq_bytes = d.header.sequence.0.to_le_bytes();
                        for (i, &b) in seq_bytes.iter().enumerate() {
                            inline_key[24 + i] ^= b;
                        }
                        if let Ok(Some(data)) = store.get(&inline_key) {
                            sd.ciphertext = data;
                        }
                    }
                }
                sd
            })
            .collect();
        // PART 8 — defensive sanity bound: the recent window must never grow
        // past 2× its configured entry-cap. Catches a runaway eviction bug
        // before it floods the snapshot.
        debug_assert!(
            inner.recent_incorporated.len() <= inner.dedup_window_entries.saturating_mul(2),
            "recent_incorporated runaway: len={} > 2x cap={}",
            inner.recent_incorporated.len(),
            inner.dedup_window_entries
        );
        let snap = ShardSnapshot {
            delta_count: inner.delta_count,
            tip: inner.tip,
            maintenance: inner.maintenance,
            deltas,
            watermarks: inner.watermarks.as_vec(),
            shard_id: Some(*inner.shard_id.0.as_bytes()),
            tenant_id: Some(*inner.tenant_id.0.as_bytes()),
            ancient_cutoff_log_index: inner.ancient_cutoff_log_index,
            recent_incorporated: inner.recent_incorporated.iter().copied().collect(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let snapshot_id = format!(
            "snap-{}",
            inner
                .last_applied_log
                .as_ref()
                .map_or(0, openraft::LogId::index)
        );
        let meta = SnapshotMetaOf::<C> {
            last_log_id: inner.last_applied_log,
            last_membership: inner.last_membership.clone(),
            snapshot_id,
        };
        Ok(Snapshot {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl RaftStateMachine<C> for ShardStateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf<C>>, StoredMembershipOf<C>), io::Error> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied_log, inner.last_membership.clone()))
    }

    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: futures::Stream<Item = Result<EntryResponder<C>, io::Error>> + Unpin + OptionalSend,
    {
        let mut inner = self.inner.lock().await;
        while let Some((entry, responder)) = entries.try_next().await? {
            let log_index = entry.log_id.index();
            inner.last_applied_log = Some(entry.log_id);
            let response = match &entry.payload {
                EntryPayload::Blank => LogResponse::Ok,
                EntryPayload::Normal(cmd) => inner.apply_command(cmd, log_index),
                EntryPayload::Membership(mem) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), mem.clone());
                    LogResponse::Ok
                }
            };
            if let Some(responder) = responder {
                responder.send(response);
            }
        }
        Ok(())
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<<C as openraft::RaftTypeConfig>::SnapshotData, io::Error> {
        Ok(Cursor::new(Vec::new()))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<C>,
        snapshot: <C as openraft::RaftTypeConfig>::SnapshotData,
    ) -> Result<(), io::Error> {
        let data = snapshot.into_inner();
        let snap: ShardSnapshot = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut inner = self.inner.lock().await;
        inner.delta_count = snap.delta_count;
        inner.tip = snap.tip;
        inner.maintenance = snap.maintenance;
        // PART 8 — restore the ancient cutoff and rebuild the recent-set from
        // the VecDeque contents. The HashSet mirror is NOT persisted (it is a
        // derived index); rebuilding it here guarantees the two are in sync at
        // the snapshot-install boundary.
        inner.ancient_cutoff_log_index = snap.ancient_cutoff_log_index;
        inner.recent_incorporated = snap.recent_incorporated.iter().copied().collect();
        inner.recent_incorporated_seqs = inner
            .recent_incorporated
            .iter()
            .map(|e| e.perspective_seq)
            .collect();
        // The dedup-window caps are local-policy knobs not in consensus state
        // — re-read from env so a deploy-time change takes effect on restart.
        inner.dedup_window_entries = dedup_window_entries();
        inner.dedup_window_ms = dedup_window_ms();
        // Restore deltas, offloading inline content to the store if available.
        inner.deltas = snap
            .deltas
            .iter()
            .map(|sd| {
                let delta = sd.to_delta();
                if delta.header.has_inline_data && !sd.ciphertext.is_empty() {
                    if let Some(ref store) = inner.inline_store {
                        let _ = store.put(&delta.header.hashed_key, &sd.ciphertext);
                    }
                }
                // Clear ciphertext from in-memory delta if store is available.
                if delta.header.has_inline_data && inner.inline_store.is_some() {
                    let mut d = delta;
                    d.payload.ciphertext = Vec::new();
                    d
                } else {
                    delta
                }
            })
            .collect();
        let mut wm = ConsumerWatermarks::new();
        for (consumer, pos) in &snap.watermarks {
            wm.advance(consumer, SequenceNumber(*pos));
        }
        inner.watermarks = wm;
        if let Some(sid) = snap.shard_id {
            inner.shard_id = ShardId(uuid::Uuid::from_bytes(sid));
        }
        if let Some(tid) = snap.tenant_id {
            inner.tenant_id = OrgId(uuid::Uuid::from_bytes(tid));
        }
        inner.last_applied_log = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        let Some(ref last) = inner.last_applied_log else {
            return Ok(None);
        };
        let snap = ShardSnapshot {
            delta_count: inner.delta_count,
            tip: inner.tip,
            maintenance: inner.maintenance,
            deltas: inner
                .deltas
                .iter()
                .map(SerializableDelta::from_delta)
                .collect(),
            watermarks: inner.watermarks.as_vec(),
            shard_id: Some(*inner.shard_id.0.as_bytes()),
            tenant_id: Some(*inner.tenant_id.0.as_bytes()),
            ancient_cutoff_log_index: inner.ancient_cutoff_log_index,
            recent_incorporated: inner.recent_incorporated.iter().copied().collect(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let meta = SnapshotMetaOf::<C> {
            last_log_id: Some(*last),
            last_membership: inner.last_membership.clone(),
            snapshot_id: format!("snap-{}", last.index()),
        };
        Ok(Some(Snapshot {
            meta,
            snapshot: Cursor::new(data),
        }))
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    /// Inline store key derivation: XOR sequence into last 8 bytes of `hashed_key`.
    /// Two deltas with the same `hashed_key` but different sequences must produce
    /// different inline keys (I-SF5 uniqueness invariant).
    fn compute_inline_key(hashed_key: &[u8; 32], sequence: u64) -> [u8; 32] {
        let mut k = *hashed_key;
        let seq_bytes = sequence.to_le_bytes();
        for (i, &b) in seq_bytes.iter().enumerate() {
            k[24 + i] ^= b;
        }
        k
    }

    #[test]
    fn inline_key_differs_for_different_sequences() {
        let hashed_key = [0xAB_u8; 32];
        let key_seq1 = compute_inline_key(&hashed_key, 1);
        let key_seq2 = compute_inline_key(&hashed_key, 2);
        assert_ne!(
            key_seq1, key_seq2,
            "inline keys for same hashed_key with different sequences must differ"
        );
    }

    #[test]
    fn inline_key_same_for_same_sequence() {
        let hashed_key = [0xCD_u8; 32];
        let key_a = compute_inline_key(&hashed_key, 42);
        let key_b = compute_inline_key(&hashed_key, 42);
        assert_eq!(
            key_a, key_b,
            "inline keys for same hashed_key and same sequence must be identical"
        );
    }

    #[test]
    fn inline_key_xor_only_affects_last_8_bytes() {
        let hashed_key = [0xFF_u8; 32];
        let key = compute_inline_key(&hashed_key, 1);
        // First 24 bytes should be unchanged.
        assert_eq!(&key[..24], &[0xFF_u8; 24]);
        // Last 8 bytes should differ from original (XOR with non-zero sequence).
        assert_ne!(&key[24..], &[0xFF_u8; 8]);
    }

    // -----------------------------------------------------------
    // Phase 16a — cluster_chunk_state Raft state machine.
    //
    // Tests the atomic CombinedProposal path that bundles a
    // composition delta with chunk metadata creation per D-4 + D-10
    // of specs/implementation/phase-16-cross-node-chunks.md.
    // -----------------------------------------------------------

    use super::*;
    use crate::raft_store::{LogCommand, NewChunkMeta};
    use kiseki_common::ids::{ChunkId, OrgId, ShardId};

    fn fresh_inner() -> ShardSmInner {
        ShardSmInner::new(
            ShardId(uuid::Uuid::from_u128(0xabc)),
            OrgId(uuid::Uuid::from_u128(0xdef)),
        )
    }

    fn org(b: u8) -> [u8; 16] {
        [b; 16]
    }

    fn chunk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    /// #129 — applying `LogCommand::ChunkAndDelta` with
    /// `inline_payloads` writes each (chunk_id, bytes) pair into the
    /// SM's local `InlineStore` (keyed by chunk_id, ADR-030 §2). This
    /// is the multi-node correctness contract for the inline write
    /// path: every replica's SM apply lands the bytes on its local
    /// SmallObjectStore so a cross-node GET resolves locally.
    #[test]
    fn chunk_and_delta_with_inline_payloads_writes_to_local_inline_store() {
        use kiseki_common::inline_store::InlineStore;
        use std::sync::Mutex;

        #[derive(Default)]
        struct CapturingInlineStore {
            puts: Mutex<Vec<([u8; 32], Vec<u8>)>>,
        }
        impl InlineStore for CapturingInlineStore {
            fn put(&self, key: &[u8; 32], data: &[u8]) -> std::io::Result<bool> {
                self.puts.lock().unwrap().push((*key, data.to_vec()));
                Ok(true)
            }
            fn get(&self, _: &[u8; 32]) -> std::io::Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn delete(&self, _: &[u8; 32]) -> std::io::Result<bool> {
                Ok(false)
            }
        }

        let store = std::sync::Arc::new(CapturingInlineStore::default());
        let mut inner = fresh_inner();
        inner.inline_store = Some(std::sync::Arc::clone(&store) as std::sync::Arc<dyn InlineStore>);

        let tenant = org(7);
        let chunk_a = chunk(0xA1);
        let chunk_b = chunk(0xB2);
        let cmd = LogCommand::ChunkAndDelta {
            tenant_id_bytes: tenant,
            operation: 0,
            hashed_key: [0x11; 32],
            chunk_refs: vec![chunk_a, chunk_b],
            payload: vec![],
            // false so the legacy `derive_inline_key`-keyed offload
            // (pre-#129, hashed_key XOR seq) does NOT fire — this
            // test asserts only the #129 chunk_id-keyed path.
            has_inline_data: false,
            new_chunks: vec![],
            inline_payloads: vec![
                (chunk_a, vec![0xDE, 0xAD, 0xBE, 0xEF]),
                (chunk_b, vec![0xCA, 0xFE]),
            ],
        };
        let _ = inner.apply_command(&cmd, 1);

        let puts = store.puts.lock().unwrap();
        assert_eq!(puts.len(), 2, "both inline payloads must reach store");
        assert_eq!(
            puts[0].0, chunk_a,
            "first put keyed by chunk_id (ADR-030 §2)"
        );
        assert_eq!(puts[0].1, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(puts[1].0, chunk_b);
        assert_eq!(puts[1].1, vec![0xCA, 0xFE]);
    }

    /// Combined proposal: delta append + chunk meta create.
    /// Applying must produce BOTH a new delta AND a new
    /// `cluster_chunk_state` entry — atomically, in a single apply
    /// step. This is the I-L2 / I-L5 atomicity contract from D-4.
    #[test]
    fn combined_proposal_atomically_appends_delta_and_creates_chunk_meta() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let chunk_id = chunk(2);
        let cmd = LogCommand::ChunkAndDelta {
            tenant_id_bytes: tenant,
            operation: 0, // Create
            hashed_key: [0x55; 32],
            chunk_refs: vec![chunk_id],
            payload: vec![0xAA; 16],
            has_inline_data: false,
            new_chunks: vec![NewChunkMeta {
                chunk_id,
                placement: vec![1, 2, 3],
                original_len: 1024,
            }],
            inline_payloads: vec![],
        };

        let _ = inner.apply_command(&cmd, 1);

        // Delta side observable.
        assert_eq!(inner.deltas.len(), 1, "delta must be appended");
        assert_eq!(inner.tip, 1);
        // Chunk meta side observable in the same apply step.
        let key = (OrgId(uuid::Uuid::from_bytes(tenant)), ChunkId(chunk_id));
        let entry = inner
            .cluster_chunk_state
            .get(&key)
            .expect("chunk_meta entry must exist after CombinedProposal apply");
        assert_eq!(entry.refcount, 1);
        assert_eq!(entry.placement, vec![1, 2, 3]);
        assert!(!entry.tombstoned);
        // Phase 16d step 3: original_len round-trips into the
        // cluster_chunk_state row so read_chunk_ec can decode
        // without the trim-trailing-zeros heuristic.
        assert_eq!(
            entry.original_len, 1024,
            "original_len must round-trip into cluster_chunk_state"
        );
    }

    /// Separate tenants writing the same `chunk_id` end up with
    /// independent `cluster_chunk_state` entries. Q1.C round-2 fix:
    /// `(tenant_id, chunk_id)` keying prevents the cross-tenant
    /// refcount inference leak under cross-tenant dedup.
    #[test]
    fn chunk_meta_keyed_by_tenant_isolates_refcount_across_tenants() {
        let mut inner = fresh_inner();
        let chunk_id = chunk(7);
        let tenant_a = org(1);
        let tenant_b = org(2);

        for tenant in [tenant_a, tenant_b] {
            let cmd = LogCommand::ChunkAndDelta {
                tenant_id_bytes: tenant,
                operation: 0,
                hashed_key: [0x33; 32],
                chunk_refs: vec![chunk_id],
                payload: vec![],
                has_inline_data: false,
                new_chunks: vec![NewChunkMeta {
                    chunk_id,
                    placement: vec![1, 2, 3],
                    original_len: 0,
                }],
                inline_payloads: vec![],
            };
            let _ = inner.apply_command(&cmd, 1);
        }

        let key_a = (OrgId(uuid::Uuid::from_bytes(tenant_a)), ChunkId(chunk_id));
        let key_b = (OrgId(uuid::Uuid::from_bytes(tenant_b)), ChunkId(chunk_id));
        assert_eq!(
            inner.cluster_chunk_state.get(&key_a).map(|e| e.refcount),
            Some(1),
            "tenant A has its own refcount=1"
        );
        assert_eq!(
            inner.cluster_chunk_state.get(&key_b).map(|e| e.refcount),
            Some(1),
            "tenant B has its own refcount=1, independent of tenant A"
        );
    }

    /// `IncrementChunkRefcount` on an existing entry bumps refcount
    /// (used when a second composition references an already-stored
    /// chunk via dedup).
    #[test]
    fn increment_chunk_refcount_bumps_existing_entry() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let chunk_id = chunk(3);

        // Seed via combined proposal.
        let _ = inner.apply_command(
            &LogCommand::ChunkAndDelta {
                tenant_id_bytes: tenant,
                operation: 0,
                hashed_key: [0; 32],
                chunk_refs: vec![chunk_id],
                payload: vec![],
                has_inline_data: false,
                new_chunks: vec![NewChunkMeta {
                    chunk_id,
                    placement: vec![1, 2, 3],
                    original_len: 0,
                }],
                inline_payloads: vec![],
            },
            1,
        );

        // Bump.
        let _ = inner.apply_command(
            &LogCommand::IncrementChunkRefcount {
                tenant_id_bytes: tenant,
                chunk_id,
            },
            2,
        );

        let key = (OrgId(uuid::Uuid::from_bytes(tenant)), ChunkId(chunk_id));
        assert_eq!(inner.cluster_chunk_state[&key].refcount, 2);
    }

    /// Decrement to refcount=0 tombstones the entry (does not
    /// remove it from the map immediately — compaction prunes
    /// tombstones later, per D-4 round 2).
    #[test]
    fn decrement_to_zero_tombstones_entry_for_compaction_prune() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let chunk_id = chunk(4);

        let _ = inner.apply_command(
            &LogCommand::ChunkAndDelta {
                tenant_id_bytes: tenant,
                operation: 0,
                hashed_key: [0; 32],
                chunk_refs: vec![chunk_id],
                payload: vec![],
                has_inline_data: false,
                new_chunks: vec![NewChunkMeta {
                    chunk_id,
                    placement: vec![1, 2, 3],
                    original_len: 0,
                }],
                inline_payloads: vec![],
            },
            1,
        );
        let _ = inner.apply_command(
            &LogCommand::DecrementChunkRefcount {
                tenant_id_bytes: tenant,
                chunk_id,
            },
            2,
        );

        let key = (OrgId(uuid::Uuid::from_bytes(tenant)), ChunkId(chunk_id));
        let entry = inner
            .cluster_chunk_state
            .get(&key)
            .expect("entry still present (tombstoned, not removed)");
        assert_eq!(entry.refcount, 0);
        assert!(entry.tombstoned, "refcount=0 must mark entry tombstoned");
    }

    // -----------------------------------------------------------
    // ADR-047 PART 8 — ancient cutoff + bounded recent-incorporated set.
    //
    // Replaces the broken global `max_incorporated_seq` floor (PART 6).
    // -----------------------------------------------------------

    fn hlc(physical_ms: u64, logical: u32, node: u64) -> kiseki_common::time::HybridLogicalClock {
        kiseki_common::time::HybridLogicalClock {
            physical_ms,
            logical,
            node_id: kiseki_common::ids::NodeId(node),
        }
    }

    fn mk_incorporate(
        tenant: [u8; 16],
        seq: kiseki_common::time::HybridLogicalClock,
    ) -> LogCommand {
        LogCommand::IncorporateIntent {
            tenant_id_bytes: tenant,
            operation: 0,
            hashed_key: [0; 32],
            chunk_refs: vec![],
            payload: vec![],
            has_inline_data: false,
            new_chunks: vec![],
            perspective_seq: seq,
            inline_payloads: vec![],
        }
    }

    /// PART 8 / PART 6 §"The bug" — the multi-writer late-arrival case the
    /// broken global floor silently dropped. Two distinct writes (different
    /// keys) on different nodes: the newer-seq one arrives first, then the
    /// older-seq one lands a tick later. Under the broken floor design the
    /// older write was filtered out / pruned. Under PART 8, BOTH must be
    /// incorporated (different keys, neither in the recent set, neither below
    /// the cutoff). This is the test that would have caught the original bug.
    #[test]
    fn multi_writer_late_arrival_is_incorporated() {
        let mut inner = fresh_inner();
        let tenant = org(1);

        // Newer seq arrives first (a write to K2 on node-B with a later HLC).
        let mut cmd_b = mk_incorporate(tenant, hlc(10, 0, 2));
        if let LogCommand::IncorporateIntent {
            ref mut hashed_key, ..
        } = cmd_b
        {
            *hashed_key = [0xBB; 32];
        }
        let _ = inner.apply_command_at(&cmd_b, 1, 1_000);
        assert_eq!(inner.deltas.len(), 1);

        // Older seq arrives later (a write to K1 on node-A with an earlier
        // HLC). The fix: this MUST be incorporated; the broken global floor
        // would have skipped it.
        let mut cmd_a = mk_incorporate(tenant, hlc(5, 0, 1));
        if let LogCommand::IncorporateIntent {
            ref mut hashed_key, ..
        } = cmd_a
        {
            *hashed_key = [0xAA; 32];
        }
        let _ = inner.apply_command_at(&cmd_a, 2, 1_010);

        assert_eq!(
            inner.deltas.len(),
            2,
            "both writes must be incorporated; PART 6 bug regressed"
        );
        // Both seqs are in the recent set so a future replay is correctly
        // deduped.
        assert!(inner.recent_incorporated_seqs.contains(&hlc(10, 0, 2)));
        assert!(inner.recent_incorporated_seqs.contains(&hlc(5, 0, 1)));
    }

    /// A replay of the same seq is a no-op: the recent set dedups it. Exactly
    /// one delta, set carries the seq exactly once.
    #[test]
    fn recent_incorporated_dedups_replay() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let seq = hlc(7, 0, 1);

        let _ = inner.apply_command_at(&mk_incorporate(tenant, seq), 1, 1_000);
        assert_eq!(inner.deltas.len(), 1);
        assert!(inner.recent_incorporated_seqs.contains(&seq));

        // Replay: same seq, different log index. Still a no-op.
        let _ = inner.apply_command_at(&mk_incorporate(tenant, seq), 2, 1_010);
        assert_eq!(inner.deltas.len(), 1, "replay must not double the delta");
        assert_eq!(inner.recent_incorporated_seqs.len(), 1);
        // The seq appears exactly once in the deque.
        let count = inner
            .recent_incorporated
            .iter()
            .filter(|e| e.perspective_seq == seq)
            .count();
        assert_eq!(count, 1, "seq must appear exactly once in the deque");
    }

    /// PART 8 Finding AA — an intent below the ancient cutoff is REFUSED with
    /// an alarm (counter increment), NOT silently dropped. With a tiny
    /// window-entries cap of 3, four ascending seqs evict the oldest; a fifth
    /// intent issued at the just-evicted log-index is refused.
    #[test]
    fn ancient_cutoff_refuses() {
        // Unique shard id so the per-shard Prometheus counter doesn't
        // alias another test that also exercises the ancient path.
        let mut inner = ShardSmInner::new_with_bounds(
            ShardId(uuid::Uuid::from_u128(0xABC_AA1)),
            OrgId(uuid::Uuid::from_u128(0xdef)),
            3,
            60_000,
        );
        let tenant = org(1);
        let label = inner.shard_id.0.to_string();
        let before = dedup_ancient_refused_counter()
            .with_label_values(&[&label])
            .get();

        // Drive four ascending seqs at four ascending log indices. The 4th
        // push evicts the 1st (entry-cap = 3), advancing the cutoff to 1 + 1
        // = 2 (the evicted entry's log index was 1, so cutoff becomes 2).
        for i in 1..=4u64 {
            let mut cmd = mk_incorporate(tenant, hlc(i, 0, 1));
            if let LogCommand::IncorporateIntent {
                ref mut hashed_key, ..
            } = cmd
            {
                hashed_key[0] = u8::try_from(i).unwrap_or(0xff);
            }
            let _ = inner.apply_command_at(&cmd, i, 1_000 + i);
        }
        assert!(
            inner.ancient_cutoff_log_index >= 2,
            "cutoff must advance past evicted entry; got {}",
            inner.ancient_cutoff_log_index
        );
        let cutoff = inner.ancient_cutoff_log_index;

        // An ancient intent at log_index strictly below the cutoff is REFUSED
        // — alarm fires, no delta added.
        let ancient_log_index = cutoff.saturating_sub(1);
        let pre_delta_count = inner.deltas.len();
        let mut ancient = mk_incorporate(tenant, hlc(999, 0, 7));
        if let LogCommand::IncorporateIntent {
            ref mut hashed_key, ..
        } = ancient
        {
            *hashed_key = [0xDE; 32];
        }
        let _ = inner.apply_command_at(&ancient, ancient_log_index, 2_000);

        assert_eq!(
            inner.deltas.len(),
            pre_delta_count,
            "ancient intent must NOT append a delta"
        );
        let after = dedup_ancient_refused_counter()
            .with_label_values(&[&label])
            .get();
        assert_eq!(after, before + 1, "ancient counter must increment");
    }

    /// With window_entries=3 the 4th apply evicts the 1st; cutoff jumps to
    /// (evicted.log_index + 1). Strict log-index coupling so a stalled shard's
    /// window does not silently shrink (Finding Q).
    #[test]
    fn window_evicts_oldest_advances_cutoff() {
        let mut inner = ShardSmInner::new_with_bounds(
            ShardId(uuid::Uuid::from_u128(0xabc)),
            OrgId(uuid::Uuid::from_u128(0xdef)),
            3,
            60_000,
        );
        let tenant = org(1);

        // Apply seq(1)..seq(4) at log indices 10, 11, 12, 13.
        for (i, log_idx) in (1..=4u64).zip(10..=13u64) {
            let mut cmd = mk_incorporate(tenant, hlc(i, 0, 1));
            if let LogCommand::IncorporateIntent {
                ref mut hashed_key, ..
            } = cmd
            {
                hashed_key[0] = u8::try_from(i).unwrap_or(0xff);
            }
            let _ = inner.apply_command_at(&cmd, log_idx, 1_000 + log_idx);
        }

        assert_eq!(
            inner.recent_incorporated.len(),
            3,
            "window must hold exactly entry-cap"
        );
        let seqs: Vec<_> = inner
            .recent_incorporated
            .iter()
            .map(|e| e.perspective_seq)
            .collect();
        assert_eq!(seqs, vec![hlc(2, 0, 1), hlc(3, 0, 1), hlc(4, 0, 1)]);
        // The evicted entry's log_index was 10 → cutoff = 11.
        assert_eq!(inner.ancient_cutoff_log_index, 11);
    }

    /// PART 8 §1 — both the cutoff and the recent-incorporated window survive
    /// a `ShardSnapshot` serde round-trip.
    #[test]
    fn snapshot_round_trips_recent_and_cutoff() {
        let recent = vec![
            RecentIncorporatedEntry {
                log_index: 100,
                perspective_seq: hlc(1, 0, 1),
                apply_ms: 5_000,
            },
            RecentIncorporatedEntry {
                log_index: 101,
                perspective_seq: hlc(2, 0, 1),
                apply_ms: 5_010,
            },
        ];
        let snap = ShardSnapshot {
            delta_count: 0,
            tip: 0,
            maintenance: false,
            deltas: vec![],
            watermarks: vec![],
            shard_id: Some([1u8; 16]),
            tenant_id: Some([2u8; 16]),
            ancient_cutoff_log_index: 99,
            recent_incorporated: recent.clone(),
        };
        let bytes = serde_json::to_vec(&snap).unwrap();
        let loaded: ShardSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.ancient_cutoff_log_index, 99);
        assert_eq!(loaded.recent_incorporated.len(), 2);
        assert_eq!(loaded.recent_incorporated[0].perspective_seq, hlc(1, 0, 1));
        assert_eq!(loaded.recent_incorporated[1].perspective_seq, hlc(2, 0, 1));
    }

    /// PART 8 §U — a batched `IncorporateIntents` runs each item through the
    /// same gate as the single variant. With one item already in the recent
    /// set (dedupe-skip), one fresh, and one ancient, exactly one delta is
    /// appended, one is skipped, one is refused-with-alarm.
    #[test]
    fn batched_incorporate_intents_applies_each_with_gate() {
        // Unique shard id so the per-shard Prometheus counter is read in
        // isolation from `ancient_cutoff_refuses` (which uses 0xABC_AA1).
        let mut inner = ShardSmInner::new_with_bounds(
            ShardId(uuid::Uuid::from_u128(0xABC_BB2)),
            OrgId(uuid::Uuid::from_u128(0xdef)),
            3,
            60_000,
        );
        let tenant = org(1);

        // Pre-seed: apply seqs 1..=4 to advance cutoff past 10.
        for (i, log_idx) in (1..=4u64).zip(10..=13u64) {
            let mut cmd = mk_incorporate(tenant, hlc(i, 0, 1));
            if let LogCommand::IncorporateIntent {
                ref mut hashed_key, ..
            } = cmd
            {
                hashed_key[0] = u8::try_from(i).unwrap_or(0xff);
            }
            let _ = inner.apply_command_at(&cmd, log_idx, 1_000 + log_idx);
        }
        // Now cutoff = 11; recent_incorporated holds seqs (2),(3),(4).

        let pre_delta_count = inner.deltas.len();
        let already_seq = hlc(3, 0, 1); // in the recent set → SKIP
        let fresh_seq = hlc(50, 0, 1); // not in recent, above cutoff → APPLY
        let _ancient_seq = hlc(99, 0, 1); // log_index < cutoff → REFUSE

        let batch_cmd = LogCommand::IncorporateIntents {
            items: vec![
                IncorporateItem {
                    tenant_id_bytes: tenant,
                    operation: 0,
                    hashed_key: [0x11; 32],
                    chunk_refs: vec![],
                    payload: vec![],
                    has_inline_data: false,
                    new_chunks: vec![],
                    perspective_seq: already_seq,
                    inline_payloads: vec![],
                },
                IncorporateItem {
                    tenant_id_bytes: tenant,
                    operation: 0,
                    hashed_key: [0x22; 32],
                    chunk_refs: vec![],
                    payload: vec![],
                    has_inline_data: false,
                    new_chunks: vec![],
                    perspective_seq: fresh_seq,
                    inline_payloads: vec![],
                },
                // Ancient: log_index for the WHOLE batch is below the cutoff
                // (the apply uses the entry's log_index for every item — by
                // design; ancient detection is keyed on log index). We
                // simulate ancient by applying the batch at a tiny log index.
                IncorporateItem {
                    tenant_id_bytes: tenant,
                    operation: 0,
                    hashed_key: [0x33; 32],
                    chunk_refs: vec![],
                    payload: vec![],
                    has_inline_data: false,
                    new_chunks: vec![],
                    perspective_seq: hlc(60, 0, 1),
                    inline_payloads: vec![],
                },
            ],
        };
        // Apply the whole batch at an ANCIENT log_index < cutoff. Now items
        // run as: (1) already in set → SKIP; (2) fresh seq + ancient
        // log_index → REFUSE; (3) fresh seq + ancient log_index → REFUSE.
        let label = inner.shard_id.0.to_string();
        let alarm_before = dedup_ancient_refused_counter()
            .with_label_values(&[&label])
            .get();
        let _ = inner.apply_command_at(&batch_cmd, 5, 2_000);
        // already → no-op, fresh → ancient-refuse, last → ancient-refuse
        assert_eq!(
            inner.deltas.len(),
            pre_delta_count,
            "no item applied: one already-set, two ancient",
        );
        let alarm_after = dedup_ancient_refused_counter()
            .with_label_values(&[&label])
            .get();
        assert_eq!(
            alarm_after,
            alarm_before + 2,
            "two ancient items incremented the alarm counter"
        );

        // Re-apply the batch at a non-ancient log_index. (1) already → SKIP;
        // (2) fresh → APPLY; (3) fresh → APPLY.
        let _ = inner.apply_command_at(&batch_cmd, 100, 2_100);
        assert_eq!(
            inner.deltas.len(),
            pre_delta_count + 2,
            "two fresh items appended on the non-ancient pass",
        );
    }
}
