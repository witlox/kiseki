//! Log-specific errors.

use kiseki_common::error::{KisekiError, PermanentError, RetriableError};
use kiseki_common::ids::ShardId;

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

    /// Raft unavailable (bootstrap, leader election, or consensus failure).
    #[error("raft unavailable")]
    Unavailable,
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
            LogError::ShardBusy { shard_id, .. } => {
                KisekiError::Retriable(RetriableError::ShardUnavailable(shard_id))
            }
            LogError::QuorumLost(id) => KisekiError::Retriable(RetriableError::QuorumLost(id)),
            LogError::KeyOutOfRange(id) | LogError::InvalidRange(id) => KisekiError::Permanent(
                PermanentError::InvariantViolation(format!("log error on shard {id:?}")),
            ),
            LogError::Unavailable => {
                KisekiError::Retriable(RetriableError::ShardUnavailable(ShardId(uuid::Uuid::nil())))
            }
        }
    }
}
