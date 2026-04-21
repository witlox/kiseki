//! Write coalescing — batches small writes into larger I/O requests.
//!
//! Small writes (below the batch threshold) are accumulated in a buffer.
//! When the buffer reaches the target size or the flush interval expires,
//! the batch is sent as a single write request.

use std::time::{Duration, Instant};

/// Batching configuration.
#[derive(Clone, Debug)]
pub struct BatchConfig {
    /// Target batch size in bytes.
    pub target_size: usize,
    /// Maximum buffer size before forced flush (OOM protection).
    pub max_buffer_size: usize,
    /// Maximum time to hold a partial batch before flushing.
    pub max_delay: Duration,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            target_size: 64 * 1024,            // 64 KB
            max_buffer_size: 16 * 1024 * 1024, // 16 MB hard cap
            max_delay: Duration::from_millis(10),
        }
    }
}

/// Write batcher — accumulates small writes.
pub struct WriteBatcher {
    config: BatchConfig,
    buffer: Vec<u8>,
    first_write: Option<Instant>,
}

impl WriteBatcher {
    /// Create a new batcher with the given config.
    #[must_use]
    pub fn new(config: BatchConfig) -> Self {
        Self {
            config,
            buffer: Vec::new(),
            first_write: None,
        }
    }

    /// Add data to the batch. Returns `Some(batch)` if the batch is full
    /// or the buffer has reached the hard cap.
    pub fn add(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.first_write.is_none() {
            self.first_write = Some(Instant::now());
        }
        self.buffer.extend_from_slice(data);

        if self.buffer.len() >= self.config.target_size
            || self.buffer.len() >= self.config.max_buffer_size
        {
            Some(self.flush())
        } else {
            None
        }
    }

    /// Flush the current batch regardless of size.
    pub fn flush(&mut self) -> Vec<u8> {
        self.first_write = None;
        std::mem::take(&mut self.buffer)
    }

    /// Check if the batch should be flushed due to timeout.
    #[must_use]
    pub fn should_flush(&self) -> bool {
        self.first_write
            .is_some_and(|t| t.elapsed() >= self.config.max_delay)
    }

    /// Current buffer size.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_accumulates() {
        let mut batcher = WriteBatcher::new(BatchConfig {
            target_size: 100,
            max_buffer_size: 1024,
            max_delay: Duration::from_secs(1),
        });

        assert!(batcher.add(b"hello").is_none());
        assert_eq!(batcher.pending_bytes(), 5);
    }

    #[test]
    fn batch_flushes_at_target() {
        let mut batcher = WriteBatcher::new(BatchConfig {
            target_size: 10,
            max_buffer_size: 1024,
            max_delay: Duration::from_secs(1),
        });

        assert!(batcher.add(b"12345").is_none());
        let batch = batcher.add(b"678901234").unwrap();
        assert_eq!(batch.len(), 14);
        assert!(batcher.is_empty());
    }

    #[test]
    fn manual_flush() {
        let mut batcher = WriteBatcher::new(BatchConfig::default());
        batcher.add(b"partial");
        let batch = batcher.flush();
        assert_eq!(batch, b"partial");
        assert!(batcher.is_empty());
    }

    #[test]
    fn empty_flush() {
        let mut batcher = WriteBatcher::new(BatchConfig::default());
        let batch = batcher.flush();
        assert!(batch.is_empty());
    }
}
