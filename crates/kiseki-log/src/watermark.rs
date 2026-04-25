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

    /// Serialize watermarks as a vector of (consumer, position) pairs.
    #[must_use]
    pub fn as_vec(&self) -> Vec<(String, u64)> {
        self.watermarks
            .iter()
            .map(|(k, v)| (k.clone(), v.0))
            .collect()
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

    // --- log.feature @unit: "Delta GC respects all consumer watermarks" ---

    #[test]
    fn gc_boundary_is_minimum_of_all_consumers() {
        let mut wm = ConsumerWatermarks::new();
        // Three consumers at different positions.
        wm.advance("sp-nfs", SequenceNumber(9500));
        wm.advance("sp-s3", SequenceNumber(8000));
        wm.advance("audit", SequenceNumber(9000));

        // GC boundary = min(9500, 8000, 9000) = 8000.
        // Deltas with sequence < 8000 are eligible for GC.
        let boundary = wm.gc_boundary().unwrap();
        assert_eq!(boundary, SequenceNumber(8000));

        // Verify: deltas from 8000 onward must be retained.
        for seq in 8000..=10000 {
            assert!(
                SequenceNumber(seq) >= boundary,
                "seq {seq} should be retained"
            );
        }
        // Verify: deltas below 8000 are eligible for GC.
        for seq in 0..8000 {
            assert!(
                SequenceNumber(seq) < boundary,
                "seq {seq} should be GC-eligible"
            );
        }
    }

    // --- log.feature @unit: "Stalled consumer blocks GC" ---

    #[test]
    fn stalled_consumer_blocks_gc() {
        let mut wm = ConsumerWatermarks::new();
        // sp-analytics stalled at 1000, all others far ahead.
        wm.advance("sp-analytics", SequenceNumber(1000));
        wm.advance("sp-nfs", SequenceNumber(50000));
        wm.advance("sp-s3", SequenceNumber(55000));
        wm.advance("audit", SequenceNumber(52000));

        // GC boundary is blocked at 1000 by the stalled consumer.
        let boundary = wm.gc_boundary().unwrap();
        assert_eq!(boundary, SequenceNumber(1000));

        // No deltas after 999 should be GC'd.
        assert!(SequenceNumber(1000) >= boundary);

        // Verify sp-analytics is identifiable as stalled.
        assert!(wm.is_stalled("sp-analytics", 10000));
        // The stalled consumer's identity is known.
        assert!(!wm.is_stalled("sp-nfs", 10000));
        assert!(!wm.is_stalled("sp-s3", 10000));
        assert!(!wm.is_stalled("audit", 10000));
    }

    #[test]
    fn stalled_consumer_id_is_identifiable() {
        let mut wm = ConsumerWatermarks::new();
        wm.advance("sp-analytics", SequenceNumber(1000));
        wm.advance("sp-nfs", SequenceNumber(50000));

        // We can identify which consumer is stalled by checking each.
        let stalled: Vec<String> = wm
            .as_vec()
            .iter()
            .filter(|(name, _)| wm.is_stalled(name, 10000))
            .map(|(name, _)| name.clone())
            .collect();

        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0], "sp-analytics");
    }
}
