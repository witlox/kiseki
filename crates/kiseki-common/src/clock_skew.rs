//! Clock skew detection across cluster peers.
//!
//! Tracks observed HLC differences between this node and its peers,
//! classifying the overall skew as Normal, Warning (soft limit), or
//! Critical (hard limit — refuse writes). Uses a sliding window of
//! observations to avoid stale data dominating the assessment.
//!
//! Spec: I-T6 (per-node clock quality reporting).

use std::time::{Duration, Instant};

/// Severity of observed clock skew.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkewSeverity {
    /// Skew is within acceptable bounds.
    Normal,
    /// Skew exceeds the soft limit — operations continue but operators
    /// should be alerted.
    Warning,
    /// Skew exceeds the hard limit — writes must be refused to preserve
    /// ordering guarantees.
    Critical,
}

/// A single clock-skew observation from a peer heartbeat or RPC.
#[derive(Clone, Debug)]
pub struct SkewObservation {
    /// The remote node whose clock was compared.
    pub peer_node_id: u64,
    /// Local HLC physical component at observation time (ms).
    pub local_hlc_ms: u64,
    /// Remote HLC physical component received from the peer (ms).
    pub remote_hlc_ms: u64,
    /// Wall-clock instant when the observation was recorded (for
    /// sliding-window eviction).
    pub observed_at: Instant,
}

impl SkewObservation {
    /// Absolute skew between local and remote HLC components.
    #[must_use]
    pub fn skew(&self) -> Duration {
        let diff = self.local_hlc_ms.abs_diff(self.remote_hlc_ms);
        Duration::from_millis(diff)
    }
}

/// Detects and classifies clock skew across cluster peers.
pub struct ClockSkewDetector {
    /// Skew above this triggers a warning.
    soft_limit: Duration,
    /// Skew above this triggers write refusal.
    hard_limit: Duration,
    /// Sliding window of recent observations.
    observations: Vec<SkewObservation>,
    /// Maximum number of observations to retain.
    max_observations: usize,
}

impl ClockSkewDetector {
    /// Create a new detector with the given thresholds.
    #[must_use]
    pub fn new(soft_limit: Duration, hard_limit: Duration) -> Self {
        Self {
            soft_limit,
            hard_limit,
            observations: Vec::new(),
            max_observations: 100,
        }
    }

    /// Create a detector with default thresholds (500 ms soft, 5 s hard).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(5))
    }

    /// Record a new observation, evicting the oldest if the window is full.
    pub fn record_observation(&mut self, peer_node_id: u64, local_hlc_ms: u64, remote_hlc_ms: u64) {
        let obs = SkewObservation {
            peer_node_id,
            local_hlc_ms,
            remote_hlc_ms,
            observed_at: Instant::now(),
        };

        let skew = obs.skew();
        if skew > self.hard_limit {
            tracing::error!(
                peer_node_id,
                skew_ms = u64::try_from(skew.as_millis()).unwrap_or(u64::MAX),
                "clock skew exceeds hard limit — writes should be refused"
            );
        } else if skew > self.soft_limit {
            tracing::warn!(
                peer_node_id,
                skew_ms = u64::try_from(skew.as_millis()).unwrap_or(u64::MAX),
                "clock skew exceeds soft limit"
            );
        }

        if self.observations.len() >= self.max_observations {
            self.observations.remove(0);
        }
        self.observations.push(obs);
    }

    /// The largest observed skew across all observations in the window.
    ///
    /// Returns `Duration::ZERO` if there are no observations.
    #[must_use]
    pub fn max_skew(&self) -> Duration {
        self.observations
            .iter()
            .map(SkewObservation::skew)
            .max()
            .unwrap_or(Duration::ZERO)
    }

    /// Current severity classification based on `max_skew()`.
    #[must_use]
    pub fn severity(&self) -> SkewSeverity {
        let skew = self.max_skew();
        if skew > self.hard_limit {
            SkewSeverity::Critical
        } else if skew > self.soft_limit {
            SkewSeverity::Warning
        } else {
            SkewSeverity::Normal
        }
    }

    /// Returns `true` if the current skew is critical and writes should
    /// be refused.
    #[must_use]
    pub fn should_refuse_writes(&self) -> bool {
        self.severity() == SkewSeverity::Critical
    }

    /// Per-peer maximum observed skew, most recent observation per peer.
    ///
    /// Returns a vec of `(peer_node_id, max_skew)` pairs.
    #[must_use]
    pub fn peers_with_skew(&self) -> Vec<(u64, Duration)> {
        use std::collections::HashMap;
        let mut per_peer: HashMap<u64, Duration> = HashMap::new();
        for obs in &self.observations {
            let skew = obs.skew();
            per_peer
                .entry(obs.peer_node_id)
                .and_modify(|current| {
                    if skew > *current {
                        *current = skew;
                    }
                })
                .or_insert(skew);
        }
        let mut result: Vec<_> = per_peer.into_iter().collect();
        result.sort_by_key(|(id, _)| *id);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_observations_is_normal() {
        let detector = ClockSkewDetector::with_defaults();
        assert_eq!(detector.severity(), SkewSeverity::Normal);
        assert_eq!(detector.max_skew(), Duration::ZERO);
        assert!(!detector.should_refuse_writes());
    }

    #[test]
    fn skew_within_soft_limit_is_normal() {
        let mut detector = ClockSkewDetector::with_defaults();
        // 200 ms skew, well under 500 ms soft limit
        detector.record_observation(2, 1000, 1200);
        assert_eq!(detector.severity(), SkewSeverity::Normal);
        assert!(!detector.should_refuse_writes());
    }

    #[test]
    fn skew_above_soft_limit_is_warning() {
        let mut detector = ClockSkewDetector::with_defaults();
        // 600 ms skew, above 500 ms soft limit but under 5 s hard limit
        detector.record_observation(2, 1000, 1600);
        assert_eq!(detector.severity(), SkewSeverity::Warning);
        assert!(!detector.should_refuse_writes());
    }

    #[test]
    fn skew_above_hard_limit_is_critical() {
        let mut detector = ClockSkewDetector::with_defaults();
        // 6000 ms skew, above 5 s hard limit
        detector.record_observation(2, 1000, 7000);
        assert_eq!(detector.severity(), SkewSeverity::Critical);
        assert!(detector.should_refuse_writes());
    }

    #[test]
    fn peers_with_skew_reports_per_peer_max() {
        let mut detector = ClockSkewDetector::with_defaults();
        detector.record_observation(2, 1000, 1100); // 100 ms
        detector.record_observation(2, 1000, 1300); // 300 ms — larger
        detector.record_observation(3, 1000, 1050); // 50 ms
        let peers = detector.peers_with_skew();
        assert_eq!(peers.len(), 2);
        // Sorted by node_id
        assert_eq!(peers[0], (2, Duration::from_millis(300)));
        assert_eq!(peers[1], (3, Duration::from_millis(50)));
    }

    #[test]
    fn sliding_window_evicts_oldest() {
        let mut detector =
            ClockSkewDetector::new(Duration::from_millis(500), Duration::from_secs(5));
        detector.max_observations = 3;

        // Fill to capacity with small skew
        detector.record_observation(1, 1000, 1010);
        detector.record_observation(2, 1000, 1020);
        detector.record_observation(3, 1000, 1030);
        assert_eq!(detector.max_skew(), Duration::from_millis(30));

        // Adding a 4th evicts the first (10 ms). New max stays 30 ms.
        detector.record_observation(4, 1000, 1005);
        assert_eq!(detector.observations.len(), 3);
        // Remaining: obs for peers 2 (20ms), 3 (30ms), 4 (5ms)
        assert_eq!(detector.max_skew(), Duration::from_millis(30));
    }
}
