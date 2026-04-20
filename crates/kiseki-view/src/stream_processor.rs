//! Stream processor — consumes deltas from the Log and advances View watermarks.
//!
//! Implements the Log → View data path. For each tracked view, reads deltas
//! from its source shards starting at the view's current watermark,
//! then advances the watermark to the highest consumed sequence.
//!
//! The stream processor is pull-based: call `poll()` to process one
//! batch of deltas per view. Production would run this in a background
//! task on a timer or triggered by `DeltaCommitted` events.

use kiseki_common::ids::SequenceNumber;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};

use crate::view::{ViewOps, ViewState};

/// Stream processor with explicit view ID tracking.
///
/// Bridges Log → View by polling deltas from source shards
/// and advancing view watermarks.
pub struct TrackedStreamProcessor<'a, L: LogOps + ?Sized, V: ViewOps> {
    log: &'a L,
    views: &'a mut V,
    tracked_views: Vec<kiseki_common::ids::ViewId>,
}

impl<'a, L: LogOps + ?Sized, V: ViewOps> TrackedStreamProcessor<'a, L, V> {
    /// Create a new tracked stream processor.
    pub fn new(log: &'a L, views: &'a mut V) -> Self {
        Self {
            log,
            views,
            tracked_views: Vec::new(),
        }
    }

    /// Register a view for delta consumption.
    pub fn track(&mut self, view_id: kiseki_common::ids::ViewId) {
        if !self.tracked_views.contains(&view_id) {
            self.tracked_views.push(view_id);
        }
    }

    /// Poll all tracked views, consuming available deltas from the log.
    ///
    /// Returns the total number of deltas consumed across all views.
    pub fn poll(&mut self, now_ms: u64) -> u64 {
        let mut total = 0;

        for i in 0..self.tracked_views.len() {
            let view_id = self.tracked_views[i];
            let Ok(view) = self.views.get_view(view_id) else {
                continue;
            };

            if view.state == ViewState::Discarded {
                continue;
            }

            let source_shards = view.descriptor.source_shards.clone();
            let watermark = view.watermark;

            for shard_id in source_shards {
                let from = SequenceNumber(watermark.0 + 1);
                let to = SequenceNumber(from.0.saturating_add(999));

                let Ok(deltas) = self
                    .log
                    .read_deltas(ReadDeltasRequest { shard_id, from, to })
                else {
                    continue;
                };

                if deltas.is_empty() {
                    continue;
                }

                let max_seq = deltas
                    .iter()
                    .map(|d| d.header.sequence)
                    .max()
                    .unwrap_or(watermark);

                total += deltas.len() as u64;
                let _ = self.views.advance_watermark(view_id, max_seq, now_ms);
            }
        }

        total
    }
}
