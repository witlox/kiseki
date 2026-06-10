//! Throughput + latency stats. One instance shared across workers.
//!
//! Latency is recorded as a flat `Vec<u32>` of microseconds under a
//! short-lived mutex per record. For the workloads this driver runs
//! (16-256 concurrency, 30 s) the contention is negligible compared
//! to the network round-trip cost; `HdrHistogram` would be nicer but
//! adds a dep we don't need at this scale.

use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug)]
pub struct Stats {
    samples: Mutex<Vec<u32>>,
    errors: std::sync::atomic::AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(100_000)),
            errors: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn record(&self, latency: Duration) {
        let us = u32::try_from(latency.as_micros()).unwrap_or(u32::MAX);
        if let Ok(mut s) = self.samples.lock() {
            s.push(us);
        }
    }

    pub fn record_error(&self) {
        self.errors
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn report(&self, object_size: usize, elapsed: Duration) -> Report {
        let mut samples = self.samples.lock().map(|s| s.clone()).unwrap_or_default();
        samples.sort_unstable();
        let ops = samples.len() as u64;
        let errors = self.errors.load(std::sync::atomic::Ordering::Relaxed);
        let secs = elapsed.as_secs_f64().max(1e-9);
        // Casts here are statistical-display only — losing the bottom
        // bits of a 53-bit-mantissa float for ops/MiB throughput is
        // benign at any realistic scale.
        #[allow(clippy::cast_precision_loss)]
        let ops_per_sec = ops as f64 / secs;
        #[allow(clippy::cast_precision_loss)]
        let mib_per_sec = (ops as f64 * object_size as f64) / secs / (1024.0 * 1024.0);
        let p50_us = pct(&samples, 50);
        let p95_us = pct(&samples, 95);
        let p99_us = pct(&samples, 99);
        Report {
            ops,
            errors,
            ops_per_sec,
            mib_per_sec,
            p50_us,
            p95_us,
            p99_us,
        }
    }
}

/// Nearest-rank percentile: `rank = ceil(n * p / 100)`, returning
/// `sorted[rank - 1]` clamped to `[0, n - 1]`. The previous
/// `floor(n * p / 100)` over-read the tail by one rank (p99 of 100
/// samples returned the max instead of `sorted[98]`).
fn pct(sorted: &[u32], p: u8) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() as u64 * u64::from(p)).div_ceil(100);
    let idx = usize::try_from(rank.saturating_sub(1)).unwrap_or(usize::MAX);
    sorted[idx.min(sorted.len() - 1)]
}

pub struct Report {
    pub ops: u64,
    pub errors: u64,
    pub ops_per_sec: f64,
    pub mib_per_sec: f64,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
}

#[cfg(test)]
mod tests {
    use super::pct;

    #[test]
    fn pct_empty_is_zero() {
        assert_eq!(pct(&[], 99), 0);
    }

    #[test]
    fn pct_single_sample_is_that_sample() {
        assert_eq!(pct(&[42], 50), 42);
        assert_eq!(pct(&[42], 99), 42);
    }

    #[test]
    fn pct_nearest_rank_100_samples() {
        // sorted[i] = i for 0..100. Nearest-rank: rank = ceil(n*p/100).
        let s: Vec<u32> = (0..100).collect();
        assert_eq!(pct(&s, 50), s[49]);
        assert_eq!(pct(&s, 95), s[94]);
        // p99 of 100 samples MUST be sorted[98], not the max.
        assert_eq!(pct(&s, 99), s[98]);
        assert_eq!(pct(&s, 100), s[99]);
    }

    #[test]
    fn pct_nearest_rank_non_divisible_n() {
        // n = 10: rank(p50) = ceil(5) = 5 → idx 4; rank(p99) = ceil(9.9)
        // = 10 → idx 9 (the max — correct for n < 100 at p99).
        let s: Vec<u32> = (0..10).collect();
        assert_eq!(pct(&s, 50), s[4]);
        assert_eq!(pct(&s, 99), s[9]);
        // n = 3: rank(p50) = ceil(1.5) = 2 → idx 1 (the median).
        let s3 = [10, 20, 30];
        assert_eq!(pct(&s3, 50), 20);
    }

    #[test]
    fn pct_p0_clamps_to_first_sample() {
        let s: Vec<u32> = (0..100).collect();
        assert_eq!(pct(&s, 0), s[0]);
    }
}
