//! Transport health tracking with circuit breaker.
//!
//! Tracks per-transport availability and latency. A transport is marked
//! unhealthy (circuit open) after `failure_threshold` failures within
//! `failure_window`. It recovers after a successful probe.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Configuration for health tracking.
#[derive(Clone, Copy, Debug)]
pub struct HealthConfig {
    /// Number of failures within `failure_window` to trip the circuit breaker.
    /// Default: 5.
    pub failure_threshold: u32,
    /// Time window for counting failures. Default: 30s.
    pub failure_window: Duration,
    /// How long before an unhealthy transport is re-probed. Default: 10 s.
    pub reprobe_interval: Duration,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            failure_window: Duration::from_secs(30),
            reprobe_interval: Duration::from_secs(10),
        }
    }
}

/// Health state for a single transport.
#[derive(Debug)]
struct TransportState {
    /// Recent failure timestamps (within `failure_window`).
    failures: Vec<Instant>,
    /// Whether the circuit breaker is open (unhealthy).
    circuit_open: bool,
    /// When the circuit was opened (for reprobe timing).
    circuit_opened_at: Option<Instant>,
    /// Exponential moving average of latency.
    ema_latency: Option<Duration>,
}

impl TransportState {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            circuit_open: false,
            circuit_opened_at: None,
            ema_latency: None,
        }
    }
}

/// Tracks health of multiple named transports.
///
/// Thread-safe: intended to be wrapped in `Arc<Mutex<_>>` or used from
/// a single async task.
pub struct TransportHealthTracker {
    config: HealthConfig,
    states: HashMap<String, TransportState>,
}

impl TransportHealthTracker {
    /// Create a new tracker with the given configuration.
    #[must_use]
    pub fn new(config: HealthConfig) -> Self {
        Self {
            config,
            states: HashMap::new(),
        }
    }

    /// Record a successful RPC on the named transport.
    ///
    /// Resets the circuit breaker if it was open.
    pub fn record_success(&mut self, transport: &str, latency: Duration) {
        let state = self
            .states
            .entry(transport.to_owned())
            .or_insert_with(TransportState::new);

        // Close circuit on success.
        state.circuit_open = false;
        state.circuit_opened_at = None;
        state.failures.clear();

        // Update EMA (alpha = 0.2 for smoothing).
        // Latencies in the microsecond range are well within f64 precision.
        let alpha = 0.2_f64;
        #[allow(clippy::cast_precision_loss)]
        let new_us = latency.as_micros() as f64;
        let ema_us = match state.ema_latency {
            #[allow(clippy::cast_precision_loss)]
            Some(prev) => alpha.mul_add(new_us, (1.0 - alpha) * prev.as_micros() as f64),
            None => new_us,
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ema = Duration::from_micros(ema_us as u64);
        state.ema_latency = Some(ema);
    }

    /// Record a failure on the named transport.
    ///
    /// If failures exceed the threshold within the window, trips the
    /// circuit breaker.
    pub fn record_failure(&mut self, transport: &str) {
        let now = Instant::now();
        let state = self
            .states
            .entry(transport.to_owned())
            .or_insert_with(TransportState::new);

        // Add failure, prune old ones outside window.
        state.failures.push(now);
        let cutoff = now.checked_sub(self.config.failure_window).unwrap_or(now);
        state.failures.retain(|&t| t >= cutoff);

        // Trip circuit if threshold exceeded.
        if state.failures.len() >= self.config.failure_threshold as usize && !state.circuit_open {
            state.circuit_open = true;
            state.circuit_opened_at = Some(now);
        }
    }

    /// Whether the named transport is considered healthy.
    ///
    /// Returns `false` if the circuit breaker is open (too many recent
    /// failures). Returns `true` for unknown transports.
    #[must_use]
    pub fn is_healthy(&self, transport: &str) -> bool {
        !self.states.get(transport).is_some_and(|s| s.circuit_open)
    }

    /// Whether the named transport should be re-probed.
    ///
    /// Returns `true` if the circuit is open and enough time has passed
    /// since it was opened.
    #[must_use]
    pub fn should_reprobe(&self, transport: &str) -> bool {
        self.states.get(transport).is_some_and(|s| {
            s.circuit_open
                && s.circuit_opened_at
                    .is_some_and(|t| t.elapsed() >= self.config.reprobe_interval)
        })
    }

    /// Current exponential moving average latency, if any samples exist.
    #[must_use]
    pub fn current_latency(&self, transport: &str) -> Option<Duration> {
        self.states.get(transport).and_then(|s| s.ema_latency)
    }

    /// Force the circuit breaker open for the named transport.
    pub fn force_open(&mut self, transport: &str) {
        let state = self
            .states
            .entry(transport.to_owned())
            .or_insert_with(TransportState::new);
        state.circuit_open = true;
        state.circuit_opened_at = Some(Instant::now());
    }
}

impl Default for TransportHealthTracker {
    fn default() -> Self {
        Self::new(HealthConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_by_default() {
        let tracker = TransportHealthTracker::default();
        assert!(tracker.is_healthy("tcp-tls"));
        assert!(tracker.is_healthy("unknown"));
    }

    #[test]
    fn circuit_trips_after_threshold() {
        let config = HealthConfig {
            failure_threshold: 3,
            failure_window: Duration::from_secs(30),
            reprobe_interval: Duration::from_secs(10),
        };
        let mut tracker = TransportHealthTracker::new(config);

        tracker.record_failure("verbs");
        assert!(tracker.is_healthy("verbs"));
        tracker.record_failure("verbs");
        assert!(tracker.is_healthy("verbs"));
        tracker.record_failure("verbs");
        assert!(!tracker.is_healthy("verbs"), "should trip after 3 failures");
    }

    #[test]
    fn success_resets_circuit() {
        let config = HealthConfig {
            failure_threshold: 2,
            failure_window: Duration::from_secs(30),
            reprobe_interval: Duration::from_secs(10),
        };
        let mut tracker = TransportHealthTracker::new(config);

        tracker.record_failure("verbs");
        tracker.record_failure("verbs");
        assert!(!tracker.is_healthy("verbs"));

        tracker.record_success("verbs", Duration::from_micros(50));
        assert!(tracker.is_healthy("verbs"), "success should reset circuit");
    }

    #[test]
    fn latency_ema_tracks() {
        let mut tracker = TransportHealthTracker::default();

        assert!(tracker.current_latency("tcp").is_none());
        tracker.record_success("tcp", Duration::from_micros(100));
        let lat = tracker.current_latency("tcp").unwrap();
        assert_eq!(lat.as_micros(), 100);

        // Second sample: EMA = 0.2 * 200 + 0.8 * 100 = 120
        tracker.record_success("tcp", Duration::from_micros(200));
        let lat = tracker.current_latency("tcp").unwrap();
        assert_eq!(lat.as_micros(), 120);
    }

    #[test]
    fn force_open_trips_circuit() {
        let mut tracker = TransportHealthTracker::default();
        assert!(tracker.is_healthy("cxi"));

        tracker.force_open("cxi");
        assert!(!tracker.is_healthy("cxi"));
    }

    #[test]
    fn reprobe_after_interval() {
        let config = HealthConfig {
            failure_threshold: 1,
            failure_window: Duration::from_secs(30),
            reprobe_interval: Duration::from_millis(10),
        };
        let mut tracker = TransportHealthTracker::new(config);

        tracker.record_failure("verbs");
        assert!(!tracker.is_healthy("verbs"));
        assert!(!tracker.should_reprobe("verbs")); // too soon

        // Wait past reprobe interval.
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            tracker.should_reprobe("verbs"),
            "should reprobe after interval"
        );
    }
}
