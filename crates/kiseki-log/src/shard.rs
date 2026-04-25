//! Shard metadata and lifecycle types.
//!
//! A shard is the smallest unit of totally-ordered deltas, backed by
//! one Raft group. Automatic lifecycle: created when a namespace is
//! created, splits when thresholds are exceeded (I-L6).

use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};

/// Shard lifecycle state (ADR-033/034).
///
/// State machine transitions (F-O6):
/// ```text
/// Healthy → Splitting (split trigger)
/// Healthy → Merging   (merge trigger)
/// Splitting → Healthy (split complete)
/// Merging → Healthy   (merge complete, for merged output shard)
/// Merging → Retiring  (merge complete, for input shards)
/// Retiring → removed  (after grace period)
/// ```
///
/// A shard in `Splitting` or `Merging` rejects the other operation
/// with `ShardBusy`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ShardState {
    /// All replicas healthy, leader elected.
    Healthy,
    /// Leader election in progress — writes rejected with retriable error.
    Election,
    /// Quorum lost — writes rejected, reads may continue if stale-ok.
    QuorumLost,
    /// Split in progress — writes accepted to the original shard,
    /// deltas for the new key range are buffered until the new shard
    /// is ready.
    Splitting,
    /// Merge in progress (ADR-034) — writes accepted during copy phase,
    /// rejected during cutover (< 50ms). Input shards transition to
    /// `Retiring` after cutover; output shard transitions to `Healthy`.
    Merging,
    /// Input shard after merge cutover (ADR-034) — reads continue for
    /// grace period (default 5 minutes), then Raft group is torn down.
    Retiring,
    /// Maintenance mode — writes rejected (retriable), reads and
    /// health queries continue (I-O6).
    Maintenance,
}

impl ShardState {
    /// Whether this shard accepts writes.
    #[must_use]
    pub fn accepts_writes(self) -> bool {
        matches!(self, Self::Healthy | Self::Splitting | Self::Merging)
    }

    /// Whether this shard is busy with a lifecycle operation (split or merge)
    /// and rejects the other operation (F-O6).
    #[must_use]
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Splitting | Self::Merging)
    }
}

/// Multi-dimension split thresholds (I-L6) and inline data config (ADR-030).
///
/// Any single dimension exceeding its ceiling forces a mandatory split.
/// The inline threshold determines whether small-file content is stored
/// in `small/objects.redb` (metadata tier) or as a chunk extent on a
/// raw block device (data tier). See I-SF1, I-L9.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShardConfig {
    /// Maximum number of deltas before mandatory split.
    pub max_delta_count: u64,
    /// Maximum total byte size before mandatory split.
    pub max_byte_size: u64,
    /// Inline data threshold in bytes (ADR-030, I-SF1).
    ///
    /// Files with encrypted payload <= this size are stored inline
    /// in `small/objects.redb`. Files above go to chunk block devices.
    /// Dynamic: computed from cluster topology, clamped to
    /// `[inline_floor, inline_ceiling]`. Changes are prospective only (I-L9).
    pub inline_threshold_bytes: u64,
    /// Hard lower bound for inline threshold (ADR-030).
    /// Metadata-like payloads (empty files, symlinks) always inline.
    pub inline_floor_bytes: u64,
    /// Hard upper bound for inline threshold (ADR-030).
    pub inline_ceiling_bytes: u64,
}

impl Default for ShardConfig {
    fn default() -> Self {
        Self {
            max_delta_count: 10_000_000,
            max_byte_size: 10 * 1024 * 1024 * 1024, // 10 GB
            inline_threshold_bytes: 4096,
            inline_floor_bytes: 128,
            inline_ceiling_bytes: 65536,
        }
    }
}

/// Shard metadata — returned by `shard_health`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShardInfo {
    /// Shard identifier.
    pub shard_id: ShardId,
    /// Owning tenant.
    pub tenant_id: OrgId,
    /// Current Raft members.
    pub raft_members: Vec<NodeId>,
    /// Current leader (if elected).
    pub leader: Option<NodeId>,
    /// Highest committed sequence number.
    pub tip: SequenceNumber,
    /// Total number of committed deltas.
    pub delta_count: u64,
    /// Total byte size of committed deltas.
    pub byte_size: u64,
    /// Current lifecycle state.
    pub state: ShardState,
    /// Split thresholds.
    pub config: ShardConfig,
    /// Key range: `[range_start, range_end)`. Full range = `[0x00..00, 0xFF..FF]`.
    /// Key range lower bound (inclusive).
    pub range_start: [u8; 32],
    /// Key range upper bound (exclusive).
    pub range_end: [u8; 32],
}
