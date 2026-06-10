//! `LogOps` trait — the public API of the Log context.
//!
//! Spec: `api-contracts.md` §Log, `data-models/log.rs`.

use kiseki_common::ids::{ChunkId, NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_common::time::DeltaTimestamp;

use crate::delta::{Delta, OperationType};
use crate::error::LogError;
use crate::raft::state_machine::ClusterChunkStateEntry;
use crate::raft_store::NewChunkMeta;
use crate::shard::{ShardConfig, ShardInfo, ShardState};

/// Request to append a delta to a shard.
#[derive(Clone, Debug)]
pub struct AppendDeltaRequest {
    /// Target shard.
    pub shard_id: ShardId,
    /// Tenant.
    pub tenant_id: OrgId,
    /// Operation type.
    pub operation: OperationType,
    /// Timestamp for the delta.
    pub timestamp: DeltaTimestamp,
    /// Routing key — `sha256(parent_dir_id || name)`.
    pub hashed_key: [u8; 32],
    /// Chunk references (empty for inline data).
    pub chunk_refs: Vec<kiseki_common::ids::ChunkId>,
    /// Encrypted payload (opaque to the Log).
    pub payload: Vec<u8>,
    /// Whether payload includes inline data.
    pub has_inline_data: bool,
}

/// Combined "create `cluster_chunk_state` entries + append delta"
/// request (Phase 16b — gateway emits this whenever the delta
/// references newly-created chunks). Applied atomically by the
/// per-shard Raft state machine: `cluster_chunk_state` entries are
/// created BEFORE the delta is appended, so any reader observing the
/// delta is guaranteed to find the matching `cluster_chunk_state` row
/// (D-4).
#[derive(Clone, Debug)]
pub struct AppendChunkAndDeltaRequest {
    /// The delta side — same fields as [`AppendDeltaRequest`].
    pub delta: AppendDeltaRequest,
    /// Newly created chunks for this delta. Each entry seeds a
    /// `cluster_chunk_state[(tenant, chunk_id)]` row with refcount=1
    /// and the leader-side placement list.
    pub new_chunks: Vec<NewChunkMeta>,
    /// #129 — inline small-file payloads to write to each
    /// replica's `InlineStore` (ADR-022 rev-5 fjall-backed
    /// `SmallObjectStore`) at apply time. `(chunk_id, env_bytes)`
    /// per entry; the gateway read path resolves `chunk_refs[i]`
    /// via `small_store.get(&chunk_id.0)` first before falling
    /// back to the chunk store. Empty for non-inline writes;
    /// every pre-#129 emitter passes `Vec::new()` and pre-prod
    /// wipe-and-redeploy clears persisted history of the legacy
    /// `has_inline_data` offload path. Keyed by `chunk_id` per
    /// ADR-030 §2 (replaces the pre-#129
    /// `derive_inline_key(hashed_key, seq)` shape).
    pub inline_payloads: Vec<(ChunkId, Vec<u8>)>,
}

/// Forwards an already-built `ChunkAndDelta` append to the shard's
/// Raft **leader** on another node (#111).
///
/// When the local node is a follower for the target shard,
/// `append_chunk_and_delta_with_forwarding` returns
/// [`LogError::ForwardToLeader`]. Rather than surface that to the
/// client (the old behaviour — a 500 on S3/NFS), the gateway re-issues
/// the *same* built append to the leader through this trait; the leader
/// `client_write`s it locally and replicates back to all members.
///
/// One mechanism covers every metadata mutation — write, delete,
/// multipart-complete — because they all funnel through the shard's
/// append (refcount inc/dec ride inside those flows). The production
/// impl dials the leader's `LogService.AppendChunkAndDelta`; tests use
/// a mock.
#[async_trait::async_trait]
pub trait AppendForwarder: Send + Sync {
    /// Re-issue `req` against `leader_node`'s shard log and return the
    /// assigned sequence. The implementation MUST be loop-safe (it dials
    /// the leader's `LogService`, which commits locally and does not
    /// re-forward).
    async fn forward_append(
        &self,
        leader_node: NodeId,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError>;
}

/// Canonical consumer name for the composition hydrator's delta-log
/// watermark (P3 / I-L4). Each node's hydrator reports its position
/// under this ONE name via [`LogOps::report_consumer_position`]; the
/// per-NODE distinction happens at the gather layer (the shard
/// leader's supervisor collects every voter's local position and
/// proposes `min` as the replicated watermark), not in the name.
pub const HYDRATOR_CONSUMER: &str = "hydrator";

/// Request to read a range of deltas.
#[derive(Clone, Debug)]
pub struct ReadDeltasRequest {
    /// Shard to read from.
    pub shard_id: ShardId,
    /// Start position (inclusive).
    pub from: SequenceNumber,
    /// End position (inclusive).
    pub to: SequenceNumber,
}

/// The Log context API.
///
/// All mutation methods take `&self` (not `&mut self`) because the
/// Raft-backed implementation uses interior mutability — mutations go
/// through the consensus layer, not direct field access. In-memory
/// implementations use `Mutex` or `RefCell` internally.
///
/// Implementations: `MemShardStore` (in-memory, for testing),
/// `RaftShardStore` (production, with openraft).
///
/// All methods are async (ADR-032) to avoid thread starvation when
/// bridging to the Raft consensus layer under concurrent load.
#[async_trait::async_trait]
pub trait LogOps: Send + Sync {
    /// Append a delta to a shard. Returns the assigned sequence number.
    ///
    /// Fails if the shard is in maintenance mode, splitting (for
    /// out-of-range keys), or has lost Raft quorum.
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError>;

    /// Append a delta, surfacing the openraft `ForwardToLeader` hint
    /// (ADR-042 §4). Callers that opt into server-side proxy /
    /// client-side leader hints use this method; others stay on
    /// [`Self::append_delta`] which collapses the hint onto
    /// `LeaderUnavailable` for backwards compatibility.
    ///
    /// Default impl forwards to `append_delta` so in-memory stores
    /// (which always behave as their own leader) need no change.
    /// The openraft-backed store overrides with the
    /// hint-preserving mapping.
    async fn append_delta_with_forwarding(
        &self,
        req: AppendDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        self.append_delta(req).await
    }

    /// Atomic "create `cluster_chunk_state` + append delta" — Phase 16b
    /// D-4 contract. Same failure modes as `append_delta`. Default
    /// impl forwards to `append_delta` and ignores `new_chunks` (used
    /// by the in-memory store and by tests that don't care about
    /// cluster-wide refcount metadata); the Raft-backed
    /// implementation overrides this with an atomic `ChunkAndDelta`
    /// proposal.
    async fn append_chunk_and_delta(
        &self,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        self.append_delta(req.delta).await
    }

    /// ADR-042 §4 — `append_chunk_and_delta` with `ForwardToLeader`
    /// hint preserved. Default impl drops `new_chunks` and forwards
    /// to `append_delta_with_forwarding` (the same simplification
    /// as `append_chunk_and_delta` itself); the Raft-backed store
    /// overrides with an atomic `ChunkAndDelta` proposal that maps
    /// the openraft hint via `map_raft_error_with_forwarding`.
    async fn append_chunk_and_delta_with_forwarding(
        &self,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        self.append_delta_with_forwarding(req.delta).await
    }

    /// Bump a chunk's `cluster_chunk_state` refcount on an existing
    /// entry — Phase 16b. No-op default (in-memory store does not
    /// track `cluster_chunk_state`). Production override proposes
    /// `IncrementChunkRefcount`.
    async fn increment_chunk_refcount(
        &self,
        _shard_id: ShardId,
        _tenant_id: OrgId,
        _chunk_id: ChunkId,
    ) -> Result<(), LogError> {
        Ok(())
    }

    /// Decrement a chunk's `cluster_chunk_state` refcount — Phase 16b.
    /// On reaching zero the entry is tombstoned and the leader is
    /// expected to fan `DeleteFragment` out to its placement list.
    /// Phase 16c returns `true` iff this call transitioned the entry
    /// to tombstoned; default `Ok(false)` keeps existing in-memory
    /// implementations side-effect-free.
    async fn decrement_chunk_refcount(
        &self,
        _shard_id: ShardId,
        _tenant_id: OrgId,
        _chunk_id: ChunkId,
    ) -> Result<bool, LogError> {
        Ok(false)
    }

    /// ADR-047 decoupled-ack: durably record `intent` on a quorum so the
    /// gateway can fast-ack a write BEFORE the synchronous Raft round.
    ///
    /// This is THE write path for async-eligible surfaces (S3, Native) —
    /// no capability gate. Implementations must record the local per-shard
    /// intent store (one durable copy) and fan the intent to the shard's
    /// voter peers in parallel, returning `Ok` ONLY once the total durable
    /// copies reach `min_acks`. On a shortfall they return `Err` and the
    /// gateway propagates the failure to the client — an acked write is
    /// guaranteed on `≥ min_acks` replicas (no-loss, I-L2/I-CS1).
    ///
    /// Single-node stores ([`crate::MemShardStore`],
    /// [`crate::PersistentShardStore`]) satisfy `min_acks = 1` with a
    /// synchronous local append (no peers to fan to). The Raft-backed
    /// [`crate::RaftShardStore`] does the real quorum intent-write.
    ///
    /// # Errors
    /// [`LogError::Unavailable`] when the shard's intent store is non-durable
    /// (e.g. an in-memory test cluster) or the local store write fails;
    /// [`LogError::QuorumLost`] when durable copies fall short of `min_acks`;
    /// otherwise a shard-lookup or transport error.
    async fn put_intent_and_fan(
        &self,
        shard_id: ShardId,
        intent: crate::intent::WriteIntent,
    ) -> Result<(), LogError>;

    /// Read deltas in `[from, to]` inclusive from a shard.
    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError>;

    /// Get shard health and metadata.
    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError>;

    /// The lowest sequence number still visible to readers of this
    /// shard's delta log, after any truncate/compact GC. Returns
    /// `SequenceNumber(0)` for a shard that has either never accepted
    /// any deltas (fresh provisioning) or whose entire delta history
    /// has been GC'd away — both are "no compaction-gap evidence",
    /// distinct from a non-zero earliest visible sequence which is
    /// positive proof that earlier deltas are gone.
    ///
    /// Required by the composition hydrator's gap-detection rule
    /// (ADR-040 §D6.3, amended for issue #87). The default impl
    /// returns `Ok(SequenceNumber(0))` so test doubles and in-memory
    /// stores that don't GC don't need to implement it; production
    /// log backends override.
    async fn earliest_visible_seq(&self, _shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        Ok(SequenceNumber(0))
    }

    /// The shard's consumer GC boundary (I-L4): deltas with
    /// `sequence < boundary` may have been pruned — by the Raft state
    /// machine's watermark-advance GC (P3a) or an explicit
    /// `truncate_log`. Full-history replay operations (split
    /// redistribution, merge copy) MUST refuse with
    /// [`LogError::DeltaLogPruned`] when this exceeds 1: they replay
    /// from sequence 1 and would silently lose pruned keys.
    ///
    /// Default `0` (= never pruned) so test doubles that don't GC
    /// need no override; backends that prune override.
    async fn gc_boundary(&self, _shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        Ok(SequenceNumber(0))
    }

    /// Set or clear maintenance mode on a shard (I-O6).
    async fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError>;

    /// Run GC: truncate deltas below the minimum consumer watermark.
    /// Returns the new GC boundary.
    async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError>;

    /// Run compaction on a shard: merge deltas by `(hashed_key, sequence)`.
    ///
    /// Newer deltas (higher sequence) supersede older ones for the same
    /// `hashed_key`. Tombstones are removed if all consumers have
    /// advanced past them. Payloads are carried opaquely — never
    /// decrypted (I-L7). Returns the number of deltas removed.
    async fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError>;

    // --- Shard management (ADR-036) ---

    /// Create a new shard with the given parameters.
    ///
    /// Idempotent: if the shard already exists, this is a no-op.
    /// Sync because shard metadata is local state (control plane Raft
    /// handles distributed coordination separately).
    fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    );

    /// Update a shard's key range (used during split/merge, ADR-033/034).
    fn update_shard_range(&self, shard_id: ShardId, range_start: [u8; 32], range_end: [u8; 32]);

    /// Transition a shard's lifecycle state (ADR-034 merge protocol).
    fn set_shard_state(&self, shard_id: ShardId, state: ShardState);

    /// Update a shard's split thresholds.
    fn set_shard_config(&self, shard_id: ShardId, config: ShardConfig);

    /// Split a shard at the midpoint of its key range, returning
    /// the id of the newly created shard. Mirrors
    /// `LogStore::split_shard` so the storage admin RPC has a
    /// trait-level seam (ADR-025 W5 — `SplitShard`). Default impl
    /// returns `Err(LogError::ShardNotFound)` so stores that don't
    /// implement split signal the gap clearly.
    fn split_shard(
        &self,
        shard_id: ShardId,
        new_shard_id: ShardId,
        node_id: NodeId,
    ) -> Result<ShardId, LogError> {
        let _ = (new_shard_id, node_id);
        Err(LogError::ShardNotFound(shard_id))
    }

    /// Merge `source_shard_id` into `target_shard_id`. Returns
    /// `Ok(())` on success. Mirrors the ADR-034 merge protocol —
    /// `LogStore` provides the building blocks
    /// (`update_shard_range` + `set_shard_state`) but no single
    /// `merge_shards()` method exists today, so this trait method
    /// stitches them together (ADR-025 W5 — `MergeShards`).
    /// Default impl errors so stores that don't implement merge
    /// signal the gap.
    fn merge_shards(
        &self,
        target_shard_id: ShardId,
        source_shard_id: ShardId,
    ) -> Result<(), LogError> {
        let _ = source_shard_id;
        Err(LogError::ShardNotFound(target_shard_id))
    }

    // --- Consumer watermarks (ADR-036, I-L4) ---

    /// Register a consumer at a starting position.
    ///
    /// Async because on Raft-backed stores, consumer state is part of
    /// the replicated state machine.
    async fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError>;

    /// Advance a consumer's watermark. Only moves forward.
    ///
    /// Callers advance watermarks BEFORE calling `truncate_log` — GC
    /// uses `min(all watermarks)` as the boundary (I-L4).
    async fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError>;

    /// Report this NODE's local consumption position for `consumer`
    /// on `shard_id` (P3 delta pruning, I-L4). Synchronous and
    /// infallible by design: consumers (the composition hydrator)
    /// call it after every poll, and a node-local record must never
    /// fail or block on consensus — on a Raft-backed store the
    /// register/advance path forwards to the leader and FAILS forever
    /// on followers, which is exactly the trap this seam exists to
    /// avoid (the leader's supervisor gathers every voter's reported
    /// position and proposes `min` as the replicated watermark).
    ///
    /// Positions are monotonic per `(shard, consumer)`: a lower
    /// report than the recorded maximum is ignored.
    ///
    /// Default no-op so test doubles and stores that never prune need
    /// no override; [`crate::RaftShardStore`] records into its
    /// node-local map, [`crate::MemShardStore`] advances its local
    /// watermark machinery directly (single-node: the local position
    /// IS the global position).
    fn report_consumer_position(
        &self,
        _shard_id: ShardId,
        _consumer: &str,
        _position: SequenceNumber,
    ) {
    }

    /// Phase 16c step 3: read a single `cluster_chunk_state` row.
    /// Used by the orphan-fragment scrub (does this chunk have any
    /// metadata?) and the under-replication scrub (is this entry
    /// tombstoned? what's its placement?). Default returns `None`
    /// so in-memory stores stay pass-through.
    async fn cluster_chunk_state_get(
        &self,
        _shard_id: ShardId,
        _tenant_id: OrgId,
        _chunk_id: ChunkId,
    ) -> Result<Option<ClusterChunkStateEntry>, LogError> {
        Ok(None)
    }

    /// Phase 16c step 3: iterate every `cluster_chunk_state` row on
    /// the given shard. Used by the under-replication scrub to walk
    /// the metadata layer and confirm each row's placement is still
    /// healthy.
    async fn cluster_chunk_state_iter(
        &self,
        _shard_id: ShardId,
    ) -> Result<Vec<(OrgId, ChunkId, ClusterChunkStateEntry)>, LogError> {
        Ok(Vec::new())
    }
}
