//! TRIM/DISCARD batching for SSD/NVMe devices (WS 2.3).
//!
//! Freed extents are queued and coalesced before issuing TRIM commands
//! to the device. Reduces TRIM storm impact and optimizes SSD wear.
//! HDD devices skip TRIM (no-op).

use std::time::{Duration, Instant};

use crate::extent::Extent;

/// Configuration for TRIM batching.
#[derive(Clone, Debug)]
pub struct TrimConfig {
    /// Maximum pending extents before auto-flush. Default: 256.
    pub max_batch: usize,
    /// Maximum time between flushes. Default: 5 seconds.
    pub flush_interval: Duration,
    /// Whether TRIM is enabled (false for HDD). Default: true.
    pub enabled: bool,
}

impl Default for TrimConfig {
    fn default() -> Self {
        Self {
            max_batch: 256,
            flush_interval: Duration::from_secs(5),
            enabled: true,
        }
    }
}

/// Batched TRIM queue with extent coalescing.
pub struct TrimQueue {
    pending: Vec<Extent>,
    config: TrimConfig,
    last_flush: Instant,
    /// Total extents flushed (lifetime counter).
    total_flushed: u64,
}

impl TrimQueue {
    /// Create a new TRIM queue.
    #[must_use]
    pub fn new(config: TrimConfig) -> Self {
        Self {
            pending: Vec::new(),
            config,
            last_flush: Instant::now(),
            total_flushed: 0,
        }
    }

    /// Enqueue a freed extent for TRIM.
    ///
    /// If the queue is at capacity, returns `true` (flush recommended).
    pub fn enqueue(&mut self, extent: Extent) -> bool {
        if !self.config.enabled {
            return false;
        }
        self.pending.push(extent);
        self.pending.len() >= self.config.max_batch
    }

    /// Whether a flush is recommended (batch full or interval elapsed).
    #[must_use]
    pub fn should_flush(&self) -> bool {
        if !self.config.enabled || self.pending.is_empty() {
            return false;
        }
        self.pending.len() >= self.config.max_batch
            || self.last_flush.elapsed() >= self.config.flush_interval
    }

    /// Flush: coalesce adjacent extents and return the merged list.
    ///
    /// The caller is responsible for issuing the actual TRIM/DISCARD
    /// commands to the device (platform-specific ioctl).
    pub fn flush(&mut self) -> Vec<Extent> {
        if self.pending.is_empty() {
            return Vec::new();
        }

        // Sort by offset for coalescing.
        self.pending.sort_by_key(|e| e.offset);

        // Coalesce adjacent extents.
        let mut merged: Vec<Extent> = Vec::new();
        for extent in &self.pending {
            if let Some(last) = merged.last_mut() {
                if last.offset + last.length == extent.offset {
                    // Adjacent — merge.
                    last.length += extent.length;
                    continue;
                }
            }
            merged.push(*extent);
        }

        self.total_flushed += self.pending.len() as u64;
        self.pending.clear();
        self.last_flush = Instant::now();

        merged
    }

    /// Number of pending extents.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Total extents flushed (lifetime).
    #[must_use]
    pub fn total_flushed(&self) -> u64 {
        self.total_flushed
    }

    /// Whether TRIM is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_and_flush() {
        let mut q = TrimQueue::new(TrimConfig::default());
        q.enqueue(Extent {
            offset: 0,
            length: 4096,
        });
        q.enqueue(Extent {
            offset: 8192,
            length: 4096,
        });
        assert_eq!(q.pending_count(), 2);

        let flushed = q.flush();
        assert_eq!(flushed.len(), 2); // non-adjacent, not merged
        assert_eq!(q.pending_count(), 0);
        assert_eq!(q.total_flushed(), 2);
    }

    #[test]
    fn coalesces_adjacent_extents() {
        let mut q = TrimQueue::new(TrimConfig::default());
        q.enqueue(Extent {
            offset: 0,
            length: 4096,
        });
        q.enqueue(Extent {
            offset: 4096,
            length: 4096,
        });
        q.enqueue(Extent {
            offset: 8192,
            length: 4096,
        });

        let flushed = q.flush();
        assert_eq!(flushed.len(), 1); // all adjacent → one extent
        assert_eq!(flushed[0].offset, 0);
        assert_eq!(flushed[0].length, 12288);
    }

    #[test]
    fn disabled_queue_ignores_enqueue() {
        let config = TrimConfig {
            enabled: false,
            ..TrimConfig::default()
        };
        let mut q = TrimQueue::new(config);
        let needs_flush = q.enqueue(Extent {
            offset: 0,
            length: 4096,
        });
        assert!(!needs_flush);
        assert_eq!(q.pending_count(), 0);
        assert!(!q.should_flush());
    }

    #[test]
    fn batch_full_triggers_flush_recommendation() {
        let config = TrimConfig {
            max_batch: 3,
            ..TrimConfig::default()
        };
        let mut q = TrimQueue::new(config);
        q.enqueue(Extent {
            offset: 0,
            length: 4096,
        });
        q.enqueue(Extent {
            offset: 4096,
            length: 4096,
        });
        let full = q.enqueue(Extent {
            offset: 8192,
            length: 4096,
        });
        assert!(full);
        assert!(q.should_flush());
    }

    #[test]
    fn interval_triggers_flush_recommendation() {
        let config = TrimConfig {
            flush_interval: Duration::from_millis(0),
            ..TrimConfig::default()
        };
        let mut q = TrimQueue::new(config);
        q.enqueue(Extent {
            offset: 0,
            length: 4096,
        });
        std::thread::sleep(Duration::from_millis(1));
        assert!(q.should_flush());
    }

    #[test]
    fn flush_empty_returns_empty() {
        let mut q = TrimQueue::new(TrimConfig::default());
        assert!(q.flush().is_empty());
    }

    #[test]
    fn out_of_order_extents_coalesced() {
        let mut q = TrimQueue::new(TrimConfig::default());
        // Insert out of order.
        q.enqueue(Extent {
            offset: 8192,
            length: 4096,
        });
        q.enqueue(Extent {
            offset: 0,
            length: 4096,
        });
        q.enqueue(Extent {
            offset: 4096,
            length: 4096,
        });

        let flushed = q.flush();
        assert_eq!(flushed.len(), 1); // sorted + coalesced
        assert_eq!(flushed[0].offset, 0);
        assert_eq!(flushed[0].length, 12288);
    }
}
