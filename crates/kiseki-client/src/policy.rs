//! Cache policy resolution and enforcement (ADR-031 §5).
//!
//! Resolves effective cache policy from data-path gRPC, gateway,
//! persisted last-known, or conservative defaults. Includes key
//! health check for crypto-shred detection and disconnect detection.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cache::{CacheConfig, CacheMode};

/// Cache policy as resolved from the tenant hierarchy.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CachePolicy {
    /// Whether caching is enabled for this tenant/workload.
    pub cache_enabled: bool,
    /// Allowed cache modes (subset of {pinned, organic, bypass}).
    pub allowed_modes: Vec<String>,
    /// Maximum L2 cache bytes per process.
    pub max_cache_bytes: u64,
    /// Maximum L2 cache bytes per node.
    pub max_node_cache_bytes: u64,
    /// Metadata TTL in milliseconds.
    pub metadata_ttl_ms: u64,
    /// Maximum disconnect duration in seconds.
    pub max_disconnect_seconds: u64,
    /// Key health check interval in milliseconds.
    pub key_health_interval_ms: u64,
    /// Whether staging is enabled.
    pub staging_enabled: bool,
    /// Default cache mode.
    pub default_mode: String,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            allowed_modes: vec!["pinned".into(), "organic".into(), "bypass".into()],
            max_cache_bytes: 50 * 1024 * 1024 * 1024, // 50 GB
            max_node_cache_bytes: 0,                  // 0 = auto (80% of fs)
            metadata_ttl_ms: 5000,
            max_disconnect_seconds: 300,
            key_health_interval_ms: 30_000,
            staging_enabled: true,
            default_mode: "organic".into(),
        }
    }
}

/// Conservative defaults used when policy is unreachable (I-CC9).
#[must_use]
pub fn conservative_defaults() -> CachePolicy {
    CachePolicy {
        cache_enabled: true,
        allowed_modes: vec!["organic".into(), "bypass".into()],
        max_cache_bytes: 10 * 1024 * 1024 * 1024, // 10 GB
        max_node_cache_bytes: 0,
        metadata_ttl_ms: 5000,
        max_disconnect_seconds: 300,
        key_health_interval_ms: 30_000,
        staging_enabled: false, // conservative: no staging without policy
        default_mode: "organic".into(),
    }
}

/// Resolve effective cache policy via fallback chain.
///
/// 1. Data-path gRPC `GetCachePolicy` (not yet implemented — returns None)
/// 2. Persisted last-known policy from pool directory
/// 3. Conservative defaults (I-CC9)
#[must_use]
pub fn resolve_policy(pool_dir: Option<&Path>) -> CachePolicy {
    // Try persisted policy.
    if let Some(dir) = pool_dir {
        if let Some(policy) = load_persisted_policy(dir) {
            tracing::debug!("cache policy loaded from persisted file");
            return policy;
        }
    }

    // Fallback to conservative defaults.
    tracing::info!("cache policy unreachable, using conservative defaults (I-CC9)");
    conservative_defaults()
}

/// Apply a resolved policy to produce a `CacheConfig`.
///
/// Client-requested mode is clamped to allowed modes.
#[must_use]
pub fn apply_policy(policy: &CachePolicy, requested_mode: Option<CacheMode>) -> CacheConfig {
    let mode = if policy.cache_enabled {
        match requested_mode {
            Some(m) if is_mode_allowed(policy, m) => m,
            _ => parse_mode(&policy.default_mode),
        }
    } else {
        CacheMode::Bypass
    };

    CacheConfig {
        mode,
        max_memory_bytes: 256 * 1024 * 1024, // L1 always 256 MB
        max_cache_bytes: policy.max_cache_bytes,
        metadata_ttl: Duration::from_millis(policy.metadata_ttl_ms),
        cache_dir: PathBuf::from("/tmp/kiseki-cache"), // overridden by env/API
        max_disconnect_seconds: policy.max_disconnect_seconds,
    }
}

/// Persist policy to pool directory for stale-tolerance (I-CC9).
pub fn persist_policy(pool_dir: &Path, policy: &CachePolicy) {
    let path = pool_dir.join("policy.json");
    if let Ok(json) = serde_json::to_string_pretty(policy) {
        let _ = std::fs::write(path, json);
    }
}

/// Load persisted policy from pool directory.
fn load_persisted_policy(pool_dir: &Path) -> Option<CachePolicy> {
    let path = pool_dir.join("policy.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn is_mode_allowed(policy: &CachePolicy, mode: CacheMode) -> bool {
    let name = match mode {
        CacheMode::Pinned => "pinned",
        CacheMode::Organic => "organic",
        CacheMode::Bypass => "bypass",
    };
    policy.allowed_modes.iter().any(|m| m == name)
}

fn parse_mode(name: &str) -> CacheMode {
    match name {
        "pinned" => CacheMode::Pinned,
        "bypass" => CacheMode::Bypass,
        _ => CacheMode::Organic,
    }
}

// ---------------------------------------------------------------------------
// Key health check (crypto-shred detection, I-CC12)
// ---------------------------------------------------------------------------

/// Key health status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyHealth {
    /// KEK is healthy — normal operation.
    Healthy,
    /// KEK has been destroyed — trigger cache wipe.
    Destroyed,
    /// KMS unreachable — start disconnect timer.
    Unreachable,
}

/// Key health checker — periodic probe of tenant KMS.
pub struct KeyHealthChecker {
    interval: Duration,
    last_check: Instant,
    last_status: KeyHealth,
}

impl KeyHealthChecker {
    /// Create a new key health checker.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_check: Instant::now(),
            last_status: KeyHealth::Healthy,
        }
    }

    /// Whether a health check is due.
    #[must_use]
    pub fn is_check_due(&self) -> bool {
        self.last_check.elapsed() >= self.interval
    }

    /// Record the result of a key health check.
    pub fn record_check(&mut self, status: KeyHealth) {
        self.last_check = Instant::now();
        self.last_status = status;
    }

    /// Last known status.
    #[must_use]
    pub fn status(&self) -> KeyHealth {
        self.last_status
    }

    /// Check interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

// ---------------------------------------------------------------------------
// Disconnect detection (I-CC6)
// ---------------------------------------------------------------------------

/// Tracks fabric connectivity for disconnect-based cache wipe.
pub struct DisconnectDetector {
    /// Timestamp of last successful RPC to any canonical endpoint.
    last_successful_rpc: Instant,
    /// Maximum disconnect duration before cache wipe.
    threshold: Duration,
}

impl DisconnectDetector {
    /// Create a new disconnect detector.
    #[must_use]
    pub fn new(threshold_seconds: u64) -> Self {
        Self {
            last_successful_rpc: Instant::now(),
            threshold: Duration::from_secs(threshold_seconds),
        }
    }

    /// Record a successful RPC.
    pub fn record_success(&mut self) {
        self.last_successful_rpc = Instant::now();
    }

    /// Whether the disconnect threshold has been exceeded.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.last_successful_rpc.elapsed() > self.threshold
    }

    /// Time since last successful RPC.
    #[must_use]
    pub fn elapsed_since_last_rpc(&self) -> Duration {
        self.last_successful_rpc.elapsed()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservative_defaults_are_safe() {
        let d = conservative_defaults();
        assert!(d.cache_enabled);
        assert_eq!(d.max_cache_bytes, 10 * 1024 * 1024 * 1024);
        assert!(!d.staging_enabled);
        assert_eq!(d.default_mode, "organic");
    }

    #[test]
    fn policy_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let policy = CachePolicy::default();
        persist_policy(dir.path(), &policy);
        let loaded = load_persisted_policy(dir.path()).unwrap();
        assert_eq!(loaded.max_cache_bytes, policy.max_cache_bytes);
        assert_eq!(loaded.metadata_ttl_ms, policy.metadata_ttl_ms);
    }

    #[test]
    fn apply_policy_clamps_to_allowed() {
        let policy = CachePolicy {
            allowed_modes: vec!["organic".into(), "bypass".into()],
            ..CachePolicy::default()
        };
        // Request pinned (not allowed) → falls back to default (organic).
        let config = apply_policy(&policy, Some(CacheMode::Pinned));
        assert_eq!(config.mode, CacheMode::Organic);
    }

    #[test]
    fn apply_policy_disabled_forces_bypass() {
        let policy = CachePolicy {
            cache_enabled: false,
            ..CachePolicy::default()
        };
        let config = apply_policy(&policy, Some(CacheMode::Organic));
        assert_eq!(config.mode, CacheMode::Bypass);
    }

    #[test]
    fn resolve_falls_back_to_defaults() {
        let policy = resolve_policy(None);
        assert!(policy.cache_enabled);
        assert!(!policy.staging_enabled); // conservative
    }

    #[test]
    fn key_health_check_due() {
        let checker = KeyHealthChecker::new(Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(1));
        assert!(checker.is_check_due());
    }

    #[test]
    fn key_health_records_status() {
        let mut checker = KeyHealthChecker::new(Duration::from_secs(30));
        assert_eq!(checker.status(), KeyHealth::Healthy);
        checker.record_check(KeyHealth::Destroyed);
        assert_eq!(checker.status(), KeyHealth::Destroyed);
    }

    #[test]
    fn disconnect_detector_initial_connected() {
        let detector = DisconnectDetector::new(300);
        assert!(!detector.is_disconnected());
    }

    #[test]
    fn disconnect_detector_threshold() {
        let detector = DisconnectDetector::new(0); // 0 seconds
        std::thread::sleep(Duration::from_millis(10));
        assert!(detector.is_disconnected());
    }

    #[test]
    fn disconnect_reset_on_success() {
        // Use a 5-second threshold so there's no race.
        let mut detector = DisconnectDetector::new(5);
        // Not disconnected initially (just created).
        assert!(!detector.is_disconnected());
        // Record success — still not disconnected.
        detector.record_success();
        assert!(!detector.is_disconnected());
    }
}
