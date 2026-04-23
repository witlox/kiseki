//! Federation delta replication types.
//!
//! Defines the data structures for async cross-site delta replication.
//! Replication streams carry ciphertext only (I-F1) — the receiving
//! site never sees plaintext keys or payloads.

use std::collections::HashMap;
use std::fmt;

use kiseki_common::ids::ShardId;

/// A single entry in a replication stream (ciphertext-only).
#[derive(Debug, Clone)]
pub struct ReplicationEntry {
    /// Shard this delta belongs to.
    pub shard_id: ShardId,
    /// Sequence number within the shard (must be gap-free).
    pub sequence: u64,
    /// Serialized delta header bytes.
    pub delta_header_bytes: Vec<u8>,
    /// Encrypted delta payload bytes.
    pub delta_payload_bytes: Vec<u8>,
    /// Timestamp when the entry was created (epoch millis).
    pub timestamp_ms: u64,
}

/// Tracks replication progress for a single remote peer.
#[derive(Debug, Clone)]
pub struct ReplicationState {
    /// Identifier of the remote peer site.
    pub peer_id: String,
    /// Last successfully replicated sequence number per shard.
    pub last_replicated_seq: HashMap<ShardId, u64>,
    /// Number of entries pending replication.
    pub entries_pending: u64,
    /// Total bytes pending replication.
    pub bytes_pending: u64,
}

impl ReplicationState {
    /// Create a new replication state for a peer with no progress.
    #[must_use]
    pub fn new(peer_id: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            last_replicated_seq: HashMap::new(),
            entries_pending: 0,
            bytes_pending: 0,
        }
    }

    /// Record that replication has advanced for `shard_id` up to `seq`.
    pub fn advance(&mut self, shard_id: ShardId, seq: u64) {
        self.last_replicated_seq.insert(shard_id, seq);
    }

    /// Returns `true` if there is a gap: the last replicated sequence
    /// for `shard_id` is not `expected_seq - 1` (i.e. we haven't
    /// replicated the entry just before the expected one).
    #[must_use]
    pub fn has_gap(&self, shard_id: ShardId, expected_seq: u64) -> bool {
        match self.last_replicated_seq.get(&shard_id) {
            None => expected_seq > 0,
            Some(&last) => last + 1 != expected_seq,
        }
    }
}

/// Errors that can occur during replication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicationError {
    /// The remote peer is unreachable.
    PeerUnreachable(String),
    /// The specified shard does not exist.
    ShardNotFound(ShardId),
    /// A sequence gap was detected in the replication stream.
    SequenceGap {
        /// Expected next sequence number.
        expected: u64,
        /// Actual sequence number received.
        got: u64,
    },
}

impl fmt::Display for ReplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PeerUnreachable(id) => write!(f, "peer unreachable: {id}"),
            Self::ShardNotFound(id) => write!(f, "shard not found: {id:?}"),
            Self::SequenceGap { expected, got } => {
                write!(f, "sequence gap: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for ReplicationError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn shard(n: u128) -> ShardId {
        ShardId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn new_state_is_empty() {
        let state = ReplicationState::new("peer-eu");
        assert_eq!(state.peer_id, "peer-eu");
        assert!(state.last_replicated_seq.is_empty());
        assert_eq!(state.entries_pending, 0);
        assert_eq!(state.bytes_pending, 0);
    }

    #[test]
    fn advance_updates_sequence() {
        let mut state = ReplicationState::new("peer-us");
        state.advance(shard(1), 10);
        assert_eq!(state.last_replicated_seq.get(&shard(1)), Some(&10));

        // Advancing again overwrites.
        state.advance(shard(1), 20);
        assert_eq!(state.last_replicated_seq.get(&shard(1)), Some(&20));
    }

    #[test]
    fn gap_detection() {
        let mut state = ReplicationState::new("peer-ch");

        // No entry for shard yet, expecting 0 => no gap.
        assert!(!state.has_gap(shard(1), 0));

        // No entry for shard yet, expecting 5 => gap.
        assert!(state.has_gap(shard(1), 5));

        // Advance to seq 4.
        state.advance(shard(1), 4);

        // Expecting 5 => no gap (last=4, 4+1=5).
        assert!(!state.has_gap(shard(1), 5));

        // Expecting 7 => gap (last=4, 4+1!=7).
        assert!(state.has_gap(shard(1), 7));
    }
}
