//! Per-pool admin overrides (ADR-025 W5 — `SetPoolThresholds`,
//! `RebalancePool`).
//!
//! These are settings that the chunk store doesn't natively
//! track — capacity warning/critical/readonly thresholds,
//! target-fill percentages, and tracker entries for in-flight
//! rebalance jobs. They're admin-driven, low-cardinality, and
//! best kept in a side-table rather than weighing down
//! `AffinityPool` (which is also serialized into the local chunk
//! store on every write path).
//!
//! ## Thresholds
//!
//! [`PoolThresholds`] mirrors the proto fields. Per-pool defaults
//! come from ADR-024 (warning 70%, critical 85%, readonly 95%,
//! `target_fill` 70% for SSD / 80% for HDD); the admin RPC stores
//! overrides keyed by pool name. `GetPool` / `PoolStatus` query
//! this store and merge with the underlying `AffinityPool` for
//! the response.
//!
//! ## Rebalance tracking
//!
//! [`RebalanceTracker`] holds in-flight `rebalance_id`s with a
//! timestamp + pool target. Today the rebalance worker is a
//! placeholder (no actual rebalance code exists in this crate);
//! the tracker exists so the admin RPC can return a deterministic
//! `rebalance_id` and operators can see active jobs via
//! `ListRebalances` (W5 follow-up if needed).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Per-pool capacity threshold overrides. `None` fields fall
/// back to the ADR-024 defaults at read time.
#[allow(clippy::struct_field_names)] // mirroring proto field names
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct PoolThresholds {
    /// Capacity warning level (% full). `None` → ADR-024 default 70.
    pub warning_pct: Option<u32>,
    /// Capacity critical level (% full). `None` → ADR-024 default 85.
    pub critical_pct: Option<u32>,
    /// Capacity read-only level (% full). `None` → ADR-024 default 95.
    pub readonly_pct: Option<u32>,
    /// Target fill level for rebalance (% full). `None` → 70 (SSD) /
    /// 80 (HDD); the device-class fallback is applied by the caller
    /// since the pool's class is known there.
    pub target_fill_pct: Option<u32>,
}

impl PoolThresholds {
    /// Bounds enforcement matching ADR-025 §"Per-pool tuning":
    /// `warning_pct ∈ 50..=95`, `critical_pct ∈ 60..=98`,
    /// `readonly_pct ∈ 70..=99`, `target_fill_pct ∈ 50..=90`.
    /// Also enforces `warning < critical < readonly` so a
    /// stranded operator can't lock themselves into an invalid
    /// state. `value == 0` is treated as "not set" (the proto
    /// default) and skipped.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(v) = self.warning_pct.filter(|v| *v != 0) {
            if !(50..=95).contains(&v) {
                return Err(format!("warning_pct {v} out of [50, 95]"));
            }
        }
        if let Some(v) = self.critical_pct.filter(|v| *v != 0) {
            if !(60..=98).contains(&v) {
                return Err(format!("critical_pct {v} out of [60, 98]"));
            }
        }
        if let Some(v) = self.readonly_pct.filter(|v| *v != 0) {
            if !(70..=99).contains(&v) {
                return Err(format!("readonly_pct {v} out of [70, 99]"));
            }
        }
        if let Some(v) = self.target_fill_pct.filter(|v| *v != 0) {
            if !(50..=90).contains(&v) {
                return Err(format!("target_fill_pct {v} out of [50, 90]"));
            }
        }
        // Treat `Some(0)` as "unset" (matches the Filter chain
        // above) so an all-zeros proto-default request validates
        // against the ADR-024 defaults rather than nonsensical
        // 0 < 0 < 0.
        let w = self.warning_pct.filter(|v| *v != 0).unwrap_or(70);
        let c = self.critical_pct.filter(|v| *v != 0).unwrap_or(85);
        let r = self.readonly_pct.filter(|v| *v != 0).unwrap_or(95);
        if !(w < c && c < r) {
            return Err(format!(
                "thresholds must satisfy warning ({w}) < critical ({c}) < readonly ({r})",
            ));
        }
        Ok(())
    }
}

/// Cluster-wide store of per-pool threshold overrides. Cheap to
/// clone via `Arc`. Today in-memory only — persisted overrides
/// land alongside the W3 tuning store in a follow-up.
#[derive(Debug, Default)]
pub struct PoolOverridesStore {
    inner: Mutex<HashMap<String, PoolThresholds>>,
}

impl PoolOverridesStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set or replace the override for `pool_name`. Validates
    /// before writing so a partial set never lands.
    pub fn set(&self, pool_name: &str, thresholds: PoolThresholds) -> Result<(), String> {
        thresholds.validate()?;
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.insert(pool_name.to_owned(), thresholds);
        Ok(())
    }

    /// Get the override for `pool_name`, or `None` if no admin has
    /// set one (callers fall back to ADR-024 defaults).
    #[must_use]
    pub fn get(&self, pool_name: &str) -> Option<PoolThresholds> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.get(pool_name).copied()
    }
}

/// One in-flight rebalance entry.
///
/// Field reads land in the upcoming `ListRebalances` admin RPC
/// (W6 CLI surface); kept `allow(dead_code)` until then so the
/// type's shape stays stable.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct RebalanceEntry {
    /// Stable identifier returned to operators.
    pub rebalance_id: String,
    /// Target pool.
    pub pool_name: String,
    /// Throttle (MB/s); 0 = use cluster default.
    pub throughput_mb_s: u64,
    /// Wall-clock when the trigger landed.
    pub started_at_ms: u64,
}

/// Tracker for in-flight rebalance triggers. Bounded buffer; we
/// keep at most 64 entries (more than any sane operator would
/// run concurrently). Real rebalance work isn't implemented in
/// this crate; the tracker exists so the admin RPC returns a
/// stable id and so a future `ListRebalances` RPC has a backing
/// store.
#[derive(Debug, Default)]
pub struct RebalanceTracker {
    inner: Mutex<Vec<RebalanceEntry>>,
}

impl RebalanceTracker {
    /// Construct an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new rebalance trigger; returns the assigned id.
    pub fn record(&self, pool_name: String, throughput_mb_s: u64) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let entry = RebalanceEntry {
            rebalance_id: id.clone(),
            pool_name,
            throughput_mb_s,
            started_at_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        };
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.len() >= 64 {
            g.remove(0);
        }
        g.push(entry);
        id
    }

    /// Snapshot the current tracker contents — newest last.
    /// Used by W6's `ListRebalances` admin RPC; kept on the
    /// public surface so the type's shape is stable.
    #[allow(dead_code)]
    #[must_use]
    pub fn snapshot(&self) -> Vec<RebalanceEntry> {
        let g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.clone()
    }
}

/// Combined handle handed to the storage admin grpc handler.
#[derive(Clone, Debug)]
pub struct PoolMutationDeps {
    /// Threshold overrides (`SetPoolThresholds`).
    pub thresholds: Arc<PoolOverridesStore>,
    /// Rebalance tracker (`RebalancePool`).
    pub rebalance: Arc<RebalanceTracker>,
}

impl PoolMutationDeps {
    /// Construct fresh in-memory deps.
    #[must_use]
    pub fn new() -> Self {
        Self {
            thresholds: Arc::new(PoolOverridesStore::new()),
            rebalance: Arc::new(RebalanceTracker::new()),
        }
    }
}

impl Default for PoolMutationDeps {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_none() {
        let p = PoolThresholds::default();
        assert!(p.warning_pct.is_none());
        assert!(p.critical_pct.is_none());
        assert!(p.readonly_pct.is_none());
        assert!(p.target_fill_pct.is_none());
        // Validate uses the defaults (70/85/95/70) which are all
        // in range and ordered, so this passes.
        p.validate().expect("defaults are valid");
    }

    #[test]
    fn validate_rejects_warning_below_50() {
        let p = PoolThresholds {
            warning_pct: Some(40),
            ..PoolThresholds::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_warning_above_95() {
        let p = PoolThresholds {
            warning_pct: Some(96),
            ..PoolThresholds::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_warning_above_critical() {
        let p = PoolThresholds {
            warning_pct: Some(80),
            critical_pct: Some(75),
            ..PoolThresholds::default()
        };
        let err = p.validate().expect_err("must reject");
        assert!(err.contains("warning"));
        assert!(err.contains("critical"));
    }

    #[test]
    fn validate_rejects_critical_above_readonly() {
        let p = PoolThresholds {
            critical_pct: Some(90),
            readonly_pct: Some(85),
            ..PoolThresholds::default()
        };
        let err = p.validate().expect_err("must reject");
        assert!(err.contains("critical"));
        assert!(err.contains("readonly"));
    }

    #[test]
    fn validate_treats_zero_as_unset() {
        // Proto-default 0 must pass without colliding with the
        // 50..=95 lower bound.
        let p = PoolThresholds {
            warning_pct: Some(0),
            critical_pct: Some(0),
            readonly_pct: Some(0),
            target_fill_pct: Some(0),
        };
        p.validate().expect("zeros are treated as unset");
    }

    #[test]
    fn store_set_then_get_round_trips() {
        let s = PoolOverridesStore::new();
        let p = PoolThresholds {
            warning_pct: Some(60),
            critical_pct: Some(80),
            readonly_pct: Some(90),
            target_fill_pct: Some(75),
        };
        s.set("hot", p).expect("valid");
        assert_eq!(s.get("hot"), Some(p));
        assert_eq!(s.get("cold"), None);
    }

    #[test]
    fn store_set_rejects_invalid_without_persisting() {
        let s = PoolOverridesStore::new();
        let bad = PoolThresholds {
            warning_pct: Some(80),
            critical_pct: Some(70),
            ..PoolThresholds::default()
        };
        s.set("p", bad).expect_err("must reject");
        assert_eq!(s.get("p"), None);
    }

    #[test]
    fn rebalance_tracker_assigns_unique_ids() {
        let t = RebalanceTracker::new();
        let a = t.record("hot".to_owned(), 0);
        let b = t.record("hot".to_owned(), 0);
        assert_ne!(a, b);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn rebalance_tracker_caps_at_64() {
        let t = RebalanceTracker::new();
        for i in 0..70u32 {
            t.record(format!("p-{i}"), 0);
        }
        assert_eq!(t.snapshot().len(), 64, "must drop oldest at cap");
    }
}
