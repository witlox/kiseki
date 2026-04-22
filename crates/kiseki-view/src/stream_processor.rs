//! Stream processor — consumes deltas from the Log and advances View watermarks.
//!
//! Implements the Log → View data path. For each tracked view, reads deltas
//! from its source shards starting at the view's current watermark,
//! then advances the watermark to the highest consumed sequence.
//!
//! The `DeltaHandler` trait allows callers to process deltas as they arrive
//! (e.g., decrypt payloads for view materialization, update directory indexes).
//! The default `NoopHandler` just advances watermarks without processing.
//!
//! The stream processor is pull-based: call `poll()` to process one
//! batch of deltas per view. Production would run this in a background
//! task on a timer or triggered by `DeltaCommitted` events.

use kiseki_common::ids::SequenceNumber;
use kiseki_log::delta::Delta;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};

use crate::view::{ViewOps, ViewState};

/// Callback for processing deltas during stream consumption.
///
/// Implement this trait to materialize view content from deltas.
/// The handler is called for each batch of deltas consumed from a shard.
pub trait DeltaHandler {
    /// Process a batch of deltas consumed from the log.
    ///
    /// Called after deltas are read but before the watermark is advanced.
    /// Implementations may decrypt payloads, update directory indexes,
    /// or build materialized view content.
    fn handle_deltas(&mut self, deltas: &[Delta]);
}

/// No-op handler — advances watermarks without processing delta content.
pub struct NoopHandler;

impl DeltaHandler for NoopHandler {
    fn handle_deltas(&mut self, _deltas: &[Delta]) {}
}

/// Decrypting handler — decrypts delta payloads via a caller-provided
/// function. The decrypt function receives ciphertext and returns
/// plaintext (or the original bytes on failure).
///
/// Used by the server runtime to materialize decrypted view content
/// without pulling `kiseki-crypto` into the view crate.
pub struct DecryptingHandler<F: FnMut(&[u8]) -> Vec<u8>> {
    decrypt: F,
    /// Accumulated decrypted payloads from the last `handle_deltas` call.
    pub decrypted: Vec<Vec<u8>>,
}

impl<F: FnMut(&[u8]) -> Vec<u8>> DecryptingHandler<F> {
    /// Create a decrypting handler with the given decrypt function.
    pub fn new(decrypt: F) -> Self {
        Self {
            decrypt,
            decrypted: Vec::new(),
        }
    }
}

impl<F: FnMut(&[u8]) -> Vec<u8>> DeltaHandler for DecryptingHandler<F> {
    fn handle_deltas(&mut self, deltas: &[Delta]) {
        self.decrypted.clear();
        for delta in deltas {
            if delta.payload.ciphertext.is_empty() {
                continue;
            }
            let plaintext = (self.decrypt)(&delta.payload.ciphertext);
            self.decrypted.push(plaintext);
        }
    }
}

/// Stream processor with explicit view ID tracking.
///
/// Bridges Log → View by polling deltas from source shards
/// and advancing view watermarks. Optionally processes delta
/// content via a `DeltaHandler`.
pub struct TrackedStreamProcessor<'a, L: LogOps + ?Sized, V: ViewOps, H: DeltaHandler = NoopHandler>
{
    log: &'a L,
    views: &'a mut V,
    tracked_views: Vec<kiseki_common::ids::ViewId>,
    handler: H,
}

impl<'a, L: LogOps + ?Sized, V: ViewOps> TrackedStreamProcessor<'a, L, V, NoopHandler> {
    /// Create a new tracked stream processor (no delta processing).
    pub fn new(log: &'a L, views: &'a mut V) -> Self {
        Self {
            log,
            views,
            tracked_views: Vec::new(),
            handler: NoopHandler,
        }
    }
}

impl<'a, L: LogOps + ?Sized, V: ViewOps, H: DeltaHandler> TrackedStreamProcessor<'a, L, V, H> {
    /// Create a stream processor with a custom delta handler.
    pub fn with_handler(log: &'a L, views: &'a mut V, handler: H) -> Self {
        Self {
            log,
            views,
            tracked_views: Vec::new(),
            handler,
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
    /// For each view, reads deltas from all source shards starting at
    /// the current watermark, passes them to the `DeltaHandler`, then
    /// advances the watermark. Multi-shard views consume from each
    /// shard independently and advance to the minimum consumed position
    /// across all shards (consistent prefix, I-V2).
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

            // Track the minimum max_seq across shards for consistent prefix.
            let mut min_max_seq: Option<SequenceNumber> = None;

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
                    // No new deltas on this shard — don't advance past watermark.
                    min_max_seq = Some(watermark);
                    continue;
                }

                // Pass deltas to handler for processing.
                self.handler.handle_deltas(&deltas);

                let max_seq = deltas
                    .iter()
                    .map(|d| d.header.sequence)
                    .max()
                    .unwrap_or(watermark);

                total += deltas.len() as u64;

                // For multi-shard: advance to the minimum across shards.
                min_max_seq = Some(match min_max_seq {
                    Some(current) if current < max_seq => current,
                    Some(current) => current,
                    None => max_seq,
                });
            }

            if let Some(advance_to) = min_max_seq {
                let _ = self.views.advance_watermark(view_id, advance_to, now_ms);
            }
        }

        total
    }
}
