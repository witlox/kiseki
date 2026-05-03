//! Throughput + latency stats. One instance shared across workers.
//!
//! Latency is recorded as a flat `Vec<u32>` of microseconds under a
//! short-lived mutex per record. For the workloads this driver runs
//! (16-256 concurrency, 30 s) the contention is negligible compared
//! to the network round-trip cost; HdrHistogram would be nicer but
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
        let mut samples = self
            .samples
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        samples.sort_unstable();
        let ops = samples.len() as u64;
        let errors = self
            .errors
            .load(std::sync::atomic::Ordering::Relaxed);
        let secs = elapsed.as_secs_f64().max(1e-9);
        let ops_per_sec = ops as f64 / secs;
        let mib_per_sec =
            (ops as f64 * object_size as f64) / secs / (1024.0 * 1024.0);
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

fn pct(sorted: &[u32], p: u8) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as u64 * u64::from(p)) / 100) as usize;
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
