//! Consumer watermark tracking for GC (I-L4).
//!
//! GC of deltas requires that ALL consumers (views + audit log) have
//! advanced past the delta's position. A stalled consumer blocks GC.

use std::collections::HashMap;

use kiseki_common::ids::SequenceNumber;

/// Tracks the consumption position of all downstream consumers.
///
/// The GC boundary is `min(all watermarks) - 1`: deltas at or below
/// this position are eligible for truncation.
#[derive(Clone, Debug, Default)]
pub struct ConsumerWatermarks {
    /// Named consumer → last consumed sequence number.
    watermarks: HashMap<String, SequenceNumber>,
}

impl ConsumerWatermarks {
    /// Create an empty watermark tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or update a consumer's watermark.
    pub fn advance(&mut self, consumer: &str, position: SequenceNumber) {
        let entry = self
            .watermarks
            .entry(consumer.to_owned())
            .or_insert(position);
        // Only advance forward — a consumer cannot go backwards.
        if position > *entry {
            *entry = position;
        }
    }

    /// Register a consumer at a starting position.
    pub fn register(&mut self, consumer: &str, position: SequenceNumber) {
        self.watermarks
            .entry(consumer.to_owned())
            .or_insert(position);
    }

    /// The GC boundary: the minimum watermark across all consumers.
    /// Deltas with `sequence < gc_boundary` are eligible for GC.
    ///
    /// Returns `None` if no consumers are registered.
    #[must_use]
    pub fn gc_boundary(&self) -> Option<SequenceNumber> {
        self.watermarks.values().min().copied()
    }

    /// Check if a specific consumer is stalled (its watermark is far
    /// behind the others).
    #[must_use]
    pub fn is_stalled(&self, consumer: &str, threshold_behind: u64) -> bool {
        let Some(consumer_pos) = self.watermarks.get(consumer) else {
            return false;
        };
        let Some(max_pos) = self.watermarks.values().max() else {
            return false;
        };
        max_pos.0.saturating_sub(consumer_pos.0) > threshold_behind
    }

    /// Number of registered consumers.
    #[must_use]
    pub fn consumer_count(&self) -> usize {
        self.watermarks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_boundary_is_minimum() {
        let mut wm = ConsumerWatermarks::new();
        wm.advance("sp-nfs", SequenceNumber(9500));
        wm.advance("sp-s3", SequenceNumber(8000));
        wm.advance("audit", SequenceNumber(9000));

        assert_eq!(wm.gc_boundary(), Some(SequenceNumber(8000)));
    }

    #[test]
    fn gc_boundary_empty() {
        let wm = ConsumerWatermarks::new();
        assert_eq!(wm.gc_boundary(), None);
    }

    #[test]
    fn watermark_only_advances_forward() {
        let mut wm = ConsumerWatermarks::new();
        wm.advance("sp-nfs", SequenceNumber(100));
        wm.advance("sp-nfs", SequenceNumber(50)); // should not go back
        assert_eq!(wm.gc_boundary(), Some(SequenceNumber(100)));
    }

    #[test]
    fn stalled_consumer_detected() {
        let mut wm = ConsumerWatermarks::new();
        wm.advance("sp-fast", SequenceNumber(50000));
        wm.advance("sp-slow", SequenceNumber(1000));

        assert!(wm.is_stalled("sp-slow", 10000));
        assert!(!wm.is_stalled("sp-fast", 10000));
    }
}
