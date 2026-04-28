//! Chunk storage errors.

use kiseki_common::error::{KisekiError, PermanentError, RetriableError};
use kiseki_common::ids::{ChunkId, ShardId};

/// Errors from chunk storage operations.
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    /// Chunk not found.
    #[error("chunk not found: {0}")]
    NotFound(ChunkId),

    /// Chunk data corrupted (AEAD auth tag failed).
    #[error("chunk corrupted: {0}")]
    Corrupted(ChunkId),

    /// Retention hold prevents deletion.
    #[error("retention hold active on chunk {0}")]
    RetentionHoldActive(ChunkId),

    /// Refcount would underflow.
    #[error("refcount underflow on chunk {0}")]
    RefcountUnderflow(ChunkId),

    /// Pool is full.
    #[error("affinity pool full: {0}")]
    PoolFull(String),

    /// EC configuration invalid (zero shards, too few devices).
    #[error("EC configuration invalid")]
    EcInvalidConfig,

    /// EC encode failed.
    #[error("EC encode failed")]
    EcEncodeFailed,

    /// Chunk lost — too many fragments missing to reconstruct.
    #[error("chunk lost: insufficient fragments for reconstruction")]
    ChunkLost,

    /// Device unavailable — chunk fragment not accessible (fault injection or real failure).
    #[error("device unavailable for chunk {0}")]
    DeviceUnavailable(ChunkId),

    /// I/O error from persistent storage backend.
    #[error("chunk I/O error: {0}")]
    Io(String),

    /// Cross-node fragment fan-out failed to reach the configured
    /// minimum number of acks (Phase 16a, D-5). Caller should back
    /// off and retry — likely transient (peer flap, partition).
    #[error("quorum lost: only {acks}/{required} replicas acked")]
    QuorumLost {
        /// How many replicas (local + peers) acked the put.
        acks: usize,
        /// How many were required for durability.
        required: usize,
    },
}

impl From<ChunkError> for KisekiError {
    fn from(e: ChunkError) -> Self {
        match e {
            ChunkError::NotFound(id) => KisekiError::Permanent(PermanentError::ChunkLost(id)),
            ChunkError::Corrupted(id) | ChunkError::RefcountUnderflow(id) => {
                KisekiError::Permanent(PermanentError::InvariantViolation(format!(
                    "chunk error: {id}"
                )))
            }
            ChunkError::RetentionHoldActive(id) => KisekiError::Permanent(
                PermanentError::InvariantViolation(format!("hold active: {id}")),
            ),
            ChunkError::PoolFull(pool) => KisekiError::Permanent(
                PermanentError::InvariantViolation(format!("pool full: {pool}")),
            ),
            ChunkError::EcInvalidConfig | ChunkError::EcEncodeFailed => {
                KisekiError::Permanent(PermanentError::InvariantViolation("EC error".into()))
            }
            ChunkError::DeviceUnavailable(id) => {
                KisekiError::Permanent(PermanentError::ChunkLost(id))
            }
            ChunkError::ChunkLost => {
                KisekiError::Permanent(PermanentError::ChunkLost(ChunkId([0; 32])))
            }
            ChunkError::Io(msg) => KisekiError::Permanent(PermanentError::InvariantViolation(msg)),
            // QuorumLost is the cross-node fan-out shortfall — retriable
            // by the caller. ShardId is filled at the gateway boundary
            // where the request's shard binding is in scope; here we
            // surface the generic shard-unavailable signal.
            ChunkError::QuorumLost { .. } => KisekiError::Retriable(
                RetriableError::ShardUnavailable(ShardId(uuid::Uuid::nil())),
            ),
        }
    }
}
