//! Readahead detection and prefetching.
//!
//! Detects sequential read patterns and prefetches upcoming data
//! to hide latency. Tracks per-file read history to distinguish
//! sequential from random access.

use std::collections::HashMap;

/// Prefetch configuration.
#[derive(Clone, Debug)]
pub struct PrefetchConfig {
    /// Minimum sequential reads before triggering prefetch.
    pub sequential_threshold: usize,
    /// Prefetch window size in bytes.
    pub window_bytes: u64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            sequential_threshold: 3,
            window_bytes: 256 * 1024, // 256 KB
        }
    }
}

/// Per-file read history.
#[derive(Debug, Default)]
struct ReadHistory {
    /// Last read offset.
    last_offset: u64,
    /// Last read length.
    last_length: u64,
    /// Count of consecutive sequential reads.
    sequential_count: usize,
}

/// Prefetch advisor — tracks reads and suggests prefetch ranges.
pub struct PrefetchAdvisor {
    config: PrefetchConfig,
    history: HashMap<u64, ReadHistory>, // keyed by inode/file ID
}

impl PrefetchAdvisor {
    /// Create a new advisor.
    #[must_use]
    pub fn new(config: PrefetchConfig) -> Self {
        Self {
            config,
            history: HashMap::new(),
        }
    }

    /// Record a read and return a prefetch suggestion if sequential access is detected.
    ///
    /// Returns `Some((prefetch_offset, prefetch_length))` if prefetch is recommended.
    pub fn record_read(&mut self, file_id: u64, offset: u64, length: u64) -> Option<(u64, u64)> {
        let history = self.history.entry(file_id).or_default();

        // Check if this read is sequential (starts where the last one ended).
        let is_sequential =
            offset == history.last_offset + history.last_length && history.last_length > 0;

        if is_sequential {
            history.sequential_count += 1;
        } else {
            history.sequential_count = 0;
        }

        history.last_offset = offset;
        history.last_length = length;

        // Suggest prefetch if we've seen enough sequential reads.
        if history.sequential_count >= self.config.sequential_threshold {
            let Some(prefetch_offset) = offset.checked_add(length) else {
                return None; // overflow near u64::MAX — skip prefetch
            };
            Some((prefetch_offset, self.config.window_bytes))
        } else {
            None
        }
    }

    /// Reset history for a file (e.g., on close).
    pub fn reset(&mut self, file_id: u64) {
        self.history.remove(&file_id);
    }

    /// Number of tracked files.
    #[must_use]
    pub fn tracked_files(&self) -> usize {
        self.history.len()
    }
}

/// A structured prefetch suggestion.
#[derive(Clone, Debug)]
pub struct PrefetchSuggestion {
    /// The file this suggestion applies to.
    pub file_id: u64,
    /// Offset to begin prefetching from.
    pub next_offset: u64,
    /// Number of bytes to prefetch.
    pub window_bytes: u64,
}

impl PrefetchSuggestion {
    /// Convert to an advisory hint for the advisory channel.
    #[must_use]
    pub fn to_hint(&self) -> crate::advisory::AdvisoryHint {
        crate::advisory::AdvisoryHint::Prefetch {
            file_id: self.file_id,
            offset: self.next_offset,
            length: self.window_bytes,
        }
    }
}

impl PrefetchAdvisor {
    /// Like [`record_read`](Self::record_read) but returns a structured
    /// [`PrefetchSuggestion`] that can be converted to an advisory hint.
    pub fn record_read_suggestion(
        &mut self,
        file_id: u64,
        offset: u64,
        length: u64,
    ) -> Option<PrefetchSuggestion> {
        self.record_read(file_id, offset, length)
            .map(|(next_offset, window_bytes)| PrefetchSuggestion {
                file_id,
                next_offset,
                window_bytes,
            })
    }
}

impl Default for PrefetchAdvisor {
    fn default() -> Self {
        Self::new(PrefetchConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_prefetch_on_random_reads() {
        let mut advisor = PrefetchAdvisor::new(PrefetchConfig::default());
        assert!(advisor.record_read(1, 0, 4096).is_none());
        assert!(advisor.record_read(1, 100_000, 4096).is_none()); // random jump
        assert!(advisor.record_read(1, 0, 4096).is_none()); // back to start
    }

    #[test]
    fn prefetch_on_sequential_reads() {
        let mut advisor = PrefetchAdvisor::new(PrefetchConfig {
            sequential_threshold: 3,
            window_bytes: 65536,
        });

        assert!(advisor.record_read(1, 0, 4096).is_none()); // 1st
        assert!(advisor.record_read(1, 4096, 4096).is_none()); // 2nd seq
        assert!(advisor.record_read(1, 8192, 4096).is_none()); // 3rd seq
        let pf = advisor.record_read(1, 12288, 4096).unwrap(); // 4th seq → prefetch!
        assert_eq!(pf.0, 16384); // next offset
        assert_eq!(pf.1, 65536); // window
    }

    #[test]
    fn reset_clears_history() {
        let mut advisor = PrefetchAdvisor::new(PrefetchConfig::default());
        advisor.record_read(1, 0, 4096);
        advisor.record_read(1, 4096, 4096);
        advisor.reset(1);
        assert_eq!(advisor.tracked_files(), 0);
    }

    #[test]
    fn prefetch_suggestion_to_hint_conversion() {
        let suggestion = PrefetchSuggestion {
            file_id: 42,
            next_offset: 8192,
            window_bytes: 65536,
        };
        let hint = suggestion.to_hint();
        match hint {
            crate::advisory::AdvisoryHint::Prefetch {
                file_id,
                offset,
                length,
            } => {
                assert_eq!(file_id, 42);
                assert_eq!(offset, 8192);
                assert_eq!(length, 65536);
            }
            _ => panic!("expected Prefetch hint"),
        }
    }

    #[test]
    fn record_read_suggestion_returns_structured() {
        let mut advisor = PrefetchAdvisor::new(PrefetchConfig {
            sequential_threshold: 2,
            window_bytes: 1024,
        });
        advisor.record_read(1, 0, 100);
        advisor.record_read(1, 100, 100);
        let suggestion = advisor.record_read_suggestion(1, 200, 100).unwrap();
        assert_eq!(suggestion.file_id, 1);
        assert_eq!(suggestion.next_offset, 300);
        assert_eq!(suggestion.window_bytes, 1024);
    }

    #[test]
    fn independent_files() {
        let mut advisor = PrefetchAdvisor::new(PrefetchConfig {
            sequential_threshold: 2,
            window_bytes: 1024,
        });

        // File 1: sequential.
        advisor.record_read(1, 0, 100);
        advisor.record_read(1, 100, 100);
        let pf = advisor.record_read(1, 200, 100);
        assert!(pf.is_some());

        // File 2: random.
        advisor.record_read(2, 0, 100);
        advisor.record_read(2, 50000, 100);
        assert!(advisor.record_read(2, 0, 100).is_none());
    }
}
