//! Chaos testing framework — fault injection primitives.
//!
//! Provides injectable faults for testing resilience: network
//! partitions, slow I/O, clock skew, process failures.
//! Used by integration tests and the chaos test suite.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fault injection controller — shared across components.
///
/// Each fault type has an independent enable flag and configuration.
/// Components check these flags on their hot paths; when disabled
/// (default), the check is a single atomic load with no overhead.
#[derive(Clone)]
pub struct FaultInjector {
    inner: Arc<FaultState>,
}

struct FaultState {
    /// Network partition: drop all RPCs.
    network_partition: AtomicBool,
    /// Slow I/O: add latency to every read/write.
    slow_io_enabled: AtomicBool,
    /// Slow I/O latency in microseconds.
    slow_io_us: AtomicU64,
    /// Clock skew: offset HLC by this many milliseconds.
    clock_skew_enabled: AtomicBool,
    /// Clock skew offset in milliseconds (positive = ahead).
    clock_skew_ms: AtomicU64,
    /// Kill signal: trigger process shutdown.
    kill_signal: AtomicBool,
    /// Total faults injected (lifetime counter).
    faults_injected: AtomicU64,
}

impl FaultInjector {
    /// Create a new fault injector with all faults disabled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(FaultState {
                network_partition: AtomicBool::new(false),
                slow_io_enabled: AtomicBool::new(false),
                slow_io_us: AtomicU64::new(0),
                clock_skew_enabled: AtomicBool::new(false),
                clock_skew_ms: AtomicU64::new(0),
                kill_signal: AtomicBool::new(false),
                faults_injected: AtomicU64::new(0),
            }),
        }
    }

    // --- Network partition ---

    /// Enable network partition (drop all RPCs).
    pub fn partition_on(&self) {
        self.inner.network_partition.store(true, Ordering::Release);
        self.inner.faults_injected.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("chaos: network partition ENABLED");
    }

    /// Disable network partition.
    pub fn partition_off(&self) {
        self.inner.network_partition.store(false, Ordering::Release);
        tracing::info!("chaos: network partition disabled");
    }

    /// Whether network partition is active.
    #[must_use]
    pub fn is_partitioned(&self) -> bool {
        self.inner.network_partition.load(Ordering::Acquire)
    }

    // --- Slow I/O ---

    /// Enable slow I/O with the given latency.
    pub fn slow_io_on(&self, latency: Duration) {
        self.inner.slow_io_us.store(
            u64::try_from(latency.as_micros()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
        self.inner.slow_io_enabled.store(true, Ordering::Release);
        self.inner.faults_injected.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(latency_us = latency.as_micros(), "chaos: slow I/O ENABLED");
    }

    /// Disable slow I/O.
    pub fn slow_io_off(&self) {
        self.inner.slow_io_enabled.store(false, Ordering::Release);
        tracing::info!("chaos: slow I/O disabled");
    }

    /// Whether slow I/O is active.
    #[must_use]
    pub fn is_slow_io(&self) -> bool {
        self.inner.slow_io_enabled.load(Ordering::Acquire)
    }

    /// Get the slow I/O latency (zero if disabled).
    #[must_use]
    pub fn slow_io_latency(&self) -> Duration {
        if self.is_slow_io() {
            Duration::from_micros(self.inner.slow_io_us.load(Ordering::Acquire))
        } else {
            Duration::ZERO
        }
    }

    // --- Clock skew ---

    /// Enable clock skew with the given offset.
    pub fn clock_skew_on(&self, offset: Duration) {
        self.inner.clock_skew_ms.store(
            u64::try_from(offset.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
        self.inner.clock_skew_enabled.store(true, Ordering::Release);
        self.inner.faults_injected.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(offset_ms = offset.as_millis(), "chaos: clock skew ENABLED");
    }

    /// Disable clock skew.
    pub fn clock_skew_off(&self) {
        self.inner
            .clock_skew_enabled
            .store(false, Ordering::Release);
        tracing::info!("chaos: clock skew disabled");
    }

    /// Get the clock skew offset (zero if disabled).
    #[must_use]
    pub fn clock_skew_offset(&self) -> Duration {
        if self.inner.clock_skew_enabled.load(Ordering::Acquire) {
            Duration::from_millis(self.inner.clock_skew_ms.load(Ordering::Acquire))
        } else {
            Duration::ZERO
        }
    }

    // --- Kill signal ---

    /// Send a kill signal (simulates process crash).
    pub fn kill(&self) {
        self.inner.kill_signal.store(true, Ordering::Release);
        self.inner.faults_injected.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("chaos: kill signal sent");
    }

    /// Whether a kill signal has been received.
    #[must_use]
    pub fn should_die(&self) -> bool {
        self.inner.kill_signal.load(Ordering::Acquire)
    }

    /// Reset the kill signal.
    pub fn resurrect(&self) {
        self.inner.kill_signal.store(false, Ordering::Release);
    }

    // --- Stats ---

    /// Total faults injected (lifetime).
    #[must_use]
    pub fn total_faults(&self) -> u64 {
        self.inner.faults_injected.load(Ordering::Relaxed)
    }

    /// Reset all faults to disabled.
    pub fn reset_all(&self) {
        self.partition_off();
        self.slow_io_off();
        self.clock_skew_off();
        self.resurrect();
    }
}

impl Default for FaultInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_all_disabled() {
        let fi = FaultInjector::new();
        assert!(!fi.is_partitioned());
        assert!(!fi.is_slow_io());
        assert_eq!(fi.clock_skew_offset(), Duration::ZERO);
        assert!(!fi.should_die());
        assert_eq!(fi.total_faults(), 0);
    }

    #[test]
    fn network_partition_toggle() {
        let fi = FaultInjector::new();
        fi.partition_on();
        assert!(fi.is_partitioned());
        fi.partition_off();
        assert!(!fi.is_partitioned());
    }

    #[test]
    fn slow_io_with_latency() {
        let fi = FaultInjector::new();
        fi.slow_io_on(Duration::from_millis(50));
        assert!(fi.is_slow_io());
        assert_eq!(fi.slow_io_latency(), Duration::from_millis(50));
        fi.slow_io_off();
        assert_eq!(fi.slow_io_latency(), Duration::ZERO);
    }

    #[test]
    fn clock_skew_offset() {
        let fi = FaultInjector::new();
        fi.clock_skew_on(Duration::from_secs(2));
        assert_eq!(fi.clock_skew_offset(), Duration::from_secs(2));
        fi.clock_skew_off();
        assert_eq!(fi.clock_skew_offset(), Duration::ZERO);
    }

    #[test]
    fn kill_and_resurrect() {
        let fi = FaultInjector::new();
        assert!(!fi.should_die());
        fi.kill();
        assert!(fi.should_die());
        fi.resurrect();
        assert!(!fi.should_die());
    }

    #[test]
    fn reset_all_clears_everything() {
        let fi = FaultInjector::new();
        fi.partition_on();
        fi.slow_io_on(Duration::from_millis(100));
        fi.clock_skew_on(Duration::from_secs(5));
        fi.kill();

        fi.reset_all();
        assert!(!fi.is_partitioned());
        assert!(!fi.is_slow_io());
        assert_eq!(fi.clock_skew_offset(), Duration::ZERO);
        assert!(!fi.should_die());
    }

    #[test]
    fn clone_shares_state() {
        let fi1 = FaultInjector::new();
        let fi2 = fi1.clone();
        fi1.partition_on();
        assert!(fi2.is_partitioned(), "clone should share state");
    }

    #[test]
    fn slow_io_latency_returned_correctly() {
        let fi = FaultInjector::new();
        let latency = Duration::from_millis(123);
        fi.slow_io_on(latency);

        assert!(fi.is_slow_io());
        assert_eq!(
            fi.slow_io_latency(),
            latency,
            "slow_io_latency should return the exact configured latency"
        );

        // Verify update works.
        let new_latency = Duration::from_millis(456);
        fi.slow_io_on(new_latency);
        assert_eq!(fi.slow_io_latency(), new_latency);
    }

    #[test]
    fn fault_counter_increments() {
        let fi = FaultInjector::new();
        fi.partition_on();
        fi.slow_io_on(Duration::from_millis(10));
        fi.kill();
        assert_eq!(fi.total_faults(), 3);
    }
}
