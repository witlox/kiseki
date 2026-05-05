//! Prometheus metrics for the block-storage backend.
//!
//! Per-device IO observability — IOPS, bytes, latency, errors —
//! so operators can compare devices and spot a slow one.
//!
//! 6 metrics, all labeled by `device` (UUID hex):
//! 1. `kiseki_block_read_total{device, outcome}` — read IOP count.
//! 2. `kiseki_block_write_total{device, outcome}` — write IOP count.
//! 3. `kiseki_block_read_bytes_total{device}` — bytes read.
//! 4. `kiseki_block_write_bytes_total{device}` — bytes written.
//! 5. `kiseki_block_read_duration_seconds{device}` — read latency.
//! 6. `kiseki_block_write_duration_seconds{device}` — write
//!    latency.
//!
//! The `outcome` label distinguishes `ok` from `crc_mismatch` /
//! `io_error` so dashboards can highlight bit rot and disk
//! failures separately.

use std::time::Duration;

use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry};

/// Outcome label values.
pub mod outcome {
    /// Operation completed successfully.
    pub const OK: &str = "ok";
    /// CRC32 trailer mismatch — bit rot or torn write.
    pub const CRC_MISMATCH: &str = "crc_mismatch";
    /// kernel-reported `Error::Io` (`EIO`, `ENOSPC`, etc).
    pub const IO_ERROR: &str = "io_error";
    /// Bitmap allocation failure (`AllocError`).
    pub const ALLOC_ERROR: &str = "alloc_error";
    /// Other / unclassified.
    pub const ERROR: &str = "error";
}

/// Prometheus metrics for the block-storage backend.
#[derive(Clone)]
pub struct BlockMetrics {
    /// Per-device read IOP count.
    pub read_total: IntCounterVec,
    /// Per-device write IOP count.
    pub write_total: IntCounterVec,
    /// Per-device read byte count.
    pub read_bytes_total: IntCounterVec,
    /// Per-device write byte count.
    pub write_bytes_total: IntCounterVec,
    /// Per-device read latency histogram.
    pub read_duration: HistogramVec,
    /// Per-device write latency histogram.
    pub write_duration: HistogramVec,
}

impl BlockMetrics {
    /// Build all metrics and register with `registry`.
    ///
    /// # Errors
    /// Returns `prometheus::Error` on name collisions.
    pub fn register(registry: &Registry) -> Result<Self, prometheus::Error> {
        let read_total = IntCounterVec::new(
            Opts::new(
                "kiseki_block_read_total",
                "Per-device block read IOP count by outcome.",
            ),
            &["device", "outcome"],
        )?;
        registry.register(Box::new(read_total.clone()))?;

        let write_total = IntCounterVec::new(
            Opts::new(
                "kiseki_block_write_total",
                "Per-device block write IOP count by outcome.",
            ),
            &["device", "outcome"],
        )?;
        registry.register(Box::new(write_total.clone()))?;

        let read_bytes_total = IntCounterVec::new(
            Opts::new(
                "kiseki_block_read_bytes_total",
                "Per-device cumulative bytes read.",
            ),
            &["device"],
        )?;
        registry.register(Box::new(read_bytes_total.clone()))?;

        let write_bytes_total = IntCounterVec::new(
            Opts::new(
                "kiseki_block_write_bytes_total",
                "Per-device cumulative bytes written.",
            ),
            &["device"],
        )?;
        registry.register(Box::new(write_bytes_total.clone()))?;

        let read_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_block_read_duration_seconds",
                "Per-device block read latency.",
            )
            .buckets(vec![
                0.00001, 0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0,
            ]),
            &["device"],
        )?;
        registry.register(Box::new(read_duration.clone()))?;

        let write_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_block_write_duration_seconds",
                "Per-device block write latency.",
            )
            .buckets(vec![
                0.00001, 0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0,
            ]),
            &["device"],
        )?;
        registry.register(Box::new(write_duration.clone()))?;

        Ok(Self {
            read_total,
            write_total,
            read_bytes_total,
            write_bytes_total,
            read_duration,
            write_duration,
        })
    }

    /// Record a read completion with outcome + latency + byte count.
    pub fn record_read(&self, device: &str, outcome: &str, dur: Duration, bytes: u64) {
        self.read_total.with_label_values(&[device, outcome]).inc();
        self.read_bytes_total
            .with_label_values(&[device])
            .inc_by(bytes);
        self.read_duration
            .with_label_values(&[device])
            .observe(dur.as_secs_f64());
    }

    /// Record a write completion with outcome + latency + byte count.
    pub fn record_write(&self, device: &str, outcome: &str, dur: Duration, bytes: u64) {
        self.write_total.with_label_values(&[device, outcome]).inc();
        self.write_bytes_total
            .with_label_values(&[device])
            .inc_by(bytes);
        self.write_duration
            .with_label_values(&[device])
            .observe(dur.as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_record_round_trip() {
        let reg = Registry::new();
        let m = BlockMetrics::register(&reg).expect("register ok");
        m.record_read("dev-a", outcome::OK, Duration::from_micros(80), 4096);
        m.record_write("dev-a", outcome::IO_ERROR, Duration::from_micros(20), 0);
        assert_eq!(
            m.read_total
                .with_label_values(&["dev-a", outcome::OK])
                .get(),
            1,
        );
        assert_eq!(m.read_bytes_total.with_label_values(&["dev-a"]).get(), 4096,);
        let names: std::collections::HashSet<_> =
            reg.gather().iter().map(|f| f.name().to_owned()).collect();
        for expected in &[
            "kiseki_block_read_total",
            "kiseki_block_write_total",
            "kiseki_block_read_bytes_total",
            "kiseki_block_write_bytes_total",
            "kiseki_block_read_duration_seconds",
            "kiseki_block_write_duration_seconds",
        ] {
            assert!(names.contains(*expected), "{expected} missing");
        }
    }
}
