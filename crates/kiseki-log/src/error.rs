//! Log-specific errors.

use kiseki_common::error::{KisekiError, PermanentError, RetriableError};
use kiseki_common::ids::{NodeId, ShardId};

/// Errors from Log operations.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// Shard not found.
    #[error("shard not found: {0:?}")]
    ShardNotFound(ShardId),

    /// Shard is in maintenance mode (I-O6).
    #[error("shard in maintenance mode: {0:?}")]
    MaintenanceMode(ShardId),

    /// Shard is splitting — delta buffered or must be re-routed.
    #[error("shard splitting: {0:?}")]
    ShardSplitting(ShardId),

    /// Raft leader unavailable (election in progress).
    #[error("leader unavailable: {0:?}")]
    LeaderUnavailable(ShardId),

    /// Raft `ForwardToLeader` hint — the leader for this shard is
    /// known and the caller should re-issue against `leader_node_id`.
    /// Surfaced only by callers that opt into the
    /// `*_with_forwarding`-suffix methods on the openraft store
    /// (ADR-042 §4 §"Implementation map"). The legacy methods
    /// (`append_delta`, `append_delta_raw`, `append_chunk_and_delta`,
    /// `set_shard_*`, `advance_watermark`) still collapse this onto
    /// [`Self::LeaderUnavailable`] for backwards compatibility — no
    /// existing S3 / NFS / mem-gateway caller regresses.
    ///
    /// Distinct from [`Self::LeaderUnavailable`] which is returned
    /// when the leader is **unknown** (election in progress, no
    /// quorum yet). With `ForwardToLeader` the leader **is** known;
    /// the receiving node is simply a follower and the request must
    /// be re-issued against the named leader. Server-side handlers
    /// that have `KISEKI_NATIVE_PROXY_FALLBACK=on` proxy the request
    /// to the leader transparently (ADR-042 §4 + ADR-042 §4 native
    /// row); others surface this error to the client which then
    /// dials the leader directly (ADR-042 §4 S3 307 row / ADR-008
    /// rev 2 client-side hint path).
    #[error("forward to leader: shard={shard_id:?} leader={leader_node_id:?}")]
    ForwardToLeader {
        /// The shard whose leader is on a different node.
        shard_id: ShardId,
        /// The node that owns the leader replica for this shard.
        /// Sourced from `openraft::error::ForwardToLeader::leader_id`
        /// (the openraft hint embedded inside
        /// `ClientWriteError::ForwardToLeader`).
        leader_node_id: NodeId,
    },

    /// Raft quorum lost.
    #[error("quorum lost: {0:?}")]
    QuorumLost(ShardId),

    /// Delta's `hashed_key` is outside this shard's key range.
    #[error("key out of range for shard {0:?}")]
    KeyOutOfRange(ShardId),

    /// Shard is busy with a lifecycle operation — split or merge in progress (F-O6).
    #[error("shard busy: {reason}")]
    ShardBusy {
        /// The busy shard.
        shard_id: ShardId,
        /// Reason: "merge in progress" or "split in progress".
        reason: &'static str,
    },

    /// Requested sequence range is invalid or beyond the shard tip.
    #[error("invalid sequence range for shard {0:?}")]
    InvalidRange(ShardId),

    /// The shard's delta log has been pruned at the consumer GC
    /// boundary (watermark-advance GC, I-L4/I-SF6). Full-history
    /// replay operations — split redistribution, merge copy — replay
    /// from sequence 1 and would silently drop every key whose only
    /// deltas were pruned, so they refuse with this error instead.
    ///
    /// TODO(compacted-replay): replay from a compacted per-key image
    /// (latest delta per `hashed_key`) instead of the raw delta log,
    /// then lift this refusal.
    #[error(
        "delta log pruned for shard {shard_id:?} (gc boundary {gc_boundary} > 1); \
         full-replay lifecycle op refused"
    )]
    DeltaLogPruned {
        /// The shard whose history is no longer fully replayable.
        shard_id: ShardId,
        /// The consumer GC boundary at refusal time.
        gc_boundary: u64,
    },

    /// Raft unavailable (bootstrap, leader election, or consensus failure).
    #[error("raft unavailable")]
    Unavailable,

    /// `OpenRaftLogStore::new` failed during construction — fjall open
    /// error, openraft initialization error, or transport setup
    /// failure. Carries the underlying cause as a string so operators
    /// can distinguish "fjall corrupt" from "openraft handshake
    /// failed" without grepping container logs. The runtime maps this
    /// to a typed startup failure rather than panicking the worker
    /// thread (which previously surfaced as SIGSEGV with no
    /// diagnostic).
    #[error("raft log store construction failed: {0}")]
    StoreConstruction(String),

    /// Backing I/O failure (inline store, persistent log, etc.).
    #[error("log I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<LogError> for KisekiError {
    fn from(e: LogError) -> Self {
        match e {
            LogError::ShardNotFound(id) => {
                KisekiError::Permanent(PermanentError::DataCorruption(id))
            }
            LogError::MaintenanceMode(id) => {
                KisekiError::Retriable(RetriableError::MaintenanceMode(id))
            }
            LogError::LeaderUnavailable(id) | LogError::ShardSplitting(id) => {
                KisekiError::Retriable(RetriableError::ShardUnavailable(id))
            }
            // Retriable from the caller's perspective: the leader is
            // known, dial it. The S3 / native server layers may
            // intercept this variant earlier (before the conversion)
            // and return a 307 redirect / proxy-fallback respectively
            // — only fully-unhandled `ForwardToLeader` flows reach
            // `KisekiError`, where the shard-unavailable mapping
            // gives the upstream caller the same retry semantics as
            // `LeaderUnavailable`.
            LogError::ForwardToLeader { shard_id, .. } | LogError::ShardBusy { shard_id, .. } => {
                KisekiError::Retriable(RetriableError::ShardUnavailable(shard_id))
            }
            LogError::QuorumLost(id) => KisekiError::Retriable(RetriableError::QuorumLost(id)),
            LogError::KeyOutOfRange(id) | LogError::InvalidRange(id) => KisekiError::Permanent(
                PermanentError::InvariantViolation(format!("log error on shard {id:?}")),
            ),
            // Permanent: retrying cannot un-prune history. The
            // operator-facing remedy is the compacted-replay follow-up
            // (see the variant's TODO), not a retry loop.
            LogError::DeltaLogPruned {
                shard_id,
                gc_boundary,
            } => KisekiError::Permanent(PermanentError::InvariantViolation(format!(
                "delta log pruned for shard {shard_id:?} (gc boundary {gc_boundary})"
            ))),
            LogError::Unavailable => {
                KisekiError::Retriable(RetriableError::ShardUnavailable(ShardId(uuid::Uuid::nil())))
            }
            LogError::StoreConstruction(msg) => KisekiError::Permanent(
                PermanentError::InvariantViolation(format!("raft store construction: {msg}")),
            ),
            LogError::Io(e) => KisekiError::Permanent(PermanentError::InvariantViolation(format!(
                "log I/O error: {e}"
            ))),
        }
    }
}
