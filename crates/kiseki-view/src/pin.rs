//! MVCC read pins (I-V4).
//!
//! A read pin holds a snapshot at a specific sequence position.
//! Pins have a bounded TTL to prevent long-running reads from
//! blocking compaction/GC.

use kiseki_common::ids::SequenceNumber;

/// An MVCC read pin — holds a consistent snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadPin {
    /// Pin identifier.
    pub pin_id: u64,
    /// Snapshot position (sequence number).
    pub position: SequenceNumber,
    /// TTL in milliseconds. After this, the pin expires and the
    /// snapshot guarantee is revoked.
    pub ttl_ms: u64,
    /// When the pin was acquired (wall-clock ms since epoch).
    pub acquired_at_ms: u64,
}

impl ReadPin {
    /// Check if the pin has expired given the current wall-clock time.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.acquired_at_ms) > self.ttl_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_expiry() {
        let pin = ReadPin {
            pin_id: 1,
            position: SequenceNumber(100),
            ttl_ms: 5000,
            acquired_at_ms: 1000,
        };

        assert!(!pin.is_expired(3000)); // 2s < 5s TTL
        assert!(!pin.is_expired(6000)); // 5s == 5s TTL (not strictly expired)
        assert!(pin.is_expired(6001)); // 5.001s > 5s TTL
    }
}
