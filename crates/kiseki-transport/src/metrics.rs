//! Transport metrics collection.
//!
//! Lightweight metrics struct for connection and RPC tracking.
//! Not tied to any specific metrics backend (Prometheus, etc.) —
//! callers read values via accessors.

use std::collections::VecDeque;
use std::time::Duration;

/// Sliding-window transport metrics.
///
/// Tracks connection, byte, and RPC counters plus a latency sample
/// window for percentile computation.
pub struct TransportMetrics {
    /// Total connections successfully opened.
    pub connections_opened: u64,
    /// Total connection attempts that failed.
    pub connections_failed: u64,
    /// Total bytes sent across all connections.
    pub bytes_sent: u64,
    /// Total bytes received across all connections.
    pub bytes_received: u64,
    /// Total RPCs completed successfully.
    pub rpc_count: u64,
    /// Total RPCs that returned errors.
    pub rpc_errors: u64,
    /// Sliding window of recent latency samples.
    latency_samples: VecDeque<Duration>,
    /// Maximum samples to keep (default 1000).
    max_samples: usize,
}

impl TransportMetrics {
    /// Create a new metrics instance with the given sample window size.
    #[must_use]
    pub fn new(max_samples: usize) -> Self {
        Self {
            connections_opened: 0,
            connections_failed: 0,
            bytes_sent: 0,
            bytes_received: 0,
            rpc_count: 0,
            rpc_errors: 0,
            latency_samples: VecDeque::with_capacity(max_samples),
            max_samples,
        }
    }

    /// Record a successful RPC with its latency.
    pub fn record_rpc(&mut self, latency: Duration) {
        self.rpc_count += 1;
        if self.latency_samples.len() >= self.max_samples {
            self.latency_samples.pop_front();
        }
        self.latency_samples.push_back(latency);
    }

    /// Record a failed RPC.
    pub fn record_rpc_error(&mut self) {
        self.rpc_errors += 1;
    }

    /// Record a successful connection.
    pub fn record_connect(&mut self) {
        self.connections_opened += 1;
    }

    /// Record a failed connection attempt.
    pub fn record_connect_failure(&mut self) {
        self.connections_failed += 1;
    }

    /// Record bytes sent.
    pub fn record_send(&mut self, bytes: u64) {
        self.bytes_sent += bytes;
    }

    /// Record bytes received.
    pub fn record_recv(&mut self, bytes: u64) {
        self.bytes_received += bytes;
    }

    /// Compute the p-th percentile latency from the sample window.
    ///
    /// `p` is in `[0, 100]`. Returns `None` if no samples exist.
    #[must_use]
    pub fn percentile(&self, p: u32) -> Option<Duration> {
        if self.latency_samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.latency_samples.iter().copied().collect();
        sorted.sort();
        let len = sorted.len();
        // Percentile index: p/100 * (len-1), clamped to valid range.
        #[allow(clippy::cast_precision_loss)]
        let idx = (f64::from(p) / 100.0 * (len.saturating_sub(1)) as f64).round();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (idx as usize).min(len - 1);
        Some(sorted[idx])
    }

    /// Shorthand for 50th percentile (median).
    #[must_use]
    pub fn p50(&self) -> Option<Duration> {
        self.percentile(50)
    }

    /// Shorthand for 99th percentile.
    #[must_use]
    pub fn p99(&self) -> Option<Duration> {
        self.percentile(99)
    }

    /// Shorthand for 99.9th percentile.
    ///
    /// Uses `p=999` with integer division for the 99.9th rank.
    #[must_use]
    pub fn p999(&self) -> Option<Duration> {
        if self.latency_samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.latency_samples.iter().copied().collect();
        sorted.sort();
        let len = sorted.len();
        #[allow(clippy::cast_precision_loss)]
        let idx = (0.999 * (len.saturating_sub(1)) as f64).round();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (idx as usize).min(len - 1);
        Some(sorted[idx])
    }

    /// Number of latency samples currently in the window.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.latency_samples.len()
    }
}

impl Default for TransportMetrics {
    fn default() -> Self {
        Self::new(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_percentiles_are_none() {
        let m = TransportMetrics::default();
        assert!(m.p50().is_none());
        assert!(m.p99().is_none());
        assert!(m.p999().is_none());
    }

    #[test]
    fn single_sample_percentiles() {
        let mut m = TransportMetrics::default();
        m.record_rpc(Duration::from_micros(100));
        assert_eq!(m.p50().unwrap().as_micros(), 100);
        assert_eq!(m.p99().unwrap().as_micros(), 100);
    }

    #[test]
    fn percentiles_correct_with_known_data() {
        let mut m = TransportMetrics::new(100);
        // Insert 1..=100 microseconds.
        // p50 of [1..100]: index = 0.50 * 99 = 49.5 → rounds to 50 → value 51
        // p99 of [1..100]: index = 0.99 * 99 = 98.01 → rounds to 98 → value 99
        for i in 1..=100 {
            m.record_rpc(Duration::from_micros(i));
        }
        assert_eq!(m.p50().unwrap().as_micros(), 51);
        assert_eq!(m.p99().unwrap().as_micros(), 99);
        assert_eq!(m.sample_count(), 100);
    }

    #[test]
    fn window_evicts_oldest() {
        let mut m = TransportMetrics::new(5);
        for i in 1..=10 {
            m.record_rpc(Duration::from_micros(i));
        }
        // Only last 5 kept: 6,7,8,9,10
        assert_eq!(m.sample_count(), 5);
        assert_eq!(m.p50().unwrap().as_micros(), 8);
    }

    #[test]
    fn counters_track() {
        let mut m = TransportMetrics::default();
        m.record_connect();
        m.record_connect();
        m.record_connect_failure();
        m.record_send(1024);
        m.record_recv(2048);
        m.record_rpc_error();

        assert_eq!(m.connections_opened, 2);
        assert_eq!(m.connections_failed, 1);
        assert_eq!(m.bytes_sent, 1024);
        assert_eq!(m.bytes_received, 2048);
        assert_eq!(m.rpc_errors, 1);
    }
}
