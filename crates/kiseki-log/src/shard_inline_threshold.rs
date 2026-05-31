//! Per-shard dynamic inline threshold computation + throughput guard
//! (ADR-030 §3, ADR-024 2026-05-31 amendment cross-ref).
//!
//! Two pure helpers + one stateful guard. All three live here, away
//! from any I/O, so the formula is unit-testable in isolation and the
//! leader-side recompute loop is the only place that has to know about
//! them.
//!
//! - [`compute_shard_inline_threshold`] runs the ADR-030 formula and
//!   clamps the result into `[floor, ceiling]`.
//! - [`InlineThroughputGuard`] tracks the shard's inline write rate
//!   over a sliding window and reports whether the configured
//!   threshold should be temporarily reduced to floor
//!   (SF-ADV-1 / I-SF7).
//!
//! Neither type touches Raft or the chunk fabric — the leader's
//! periodic loop calls `compute_shard_inline_threshold`, feeds the
//! result through `InlineThroughputGuard::effective_threshold`, then
//! emits a `SetShardConfig` only when the value has actually changed.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// ADR-030 §3 — compute the raw per-shard inline threshold from the
/// **minimum** small-file budget across the shard's voters and the
/// projected file count for the shard. Result is clamped to
/// `[floor, ceiling]`.
///
/// `available_bytes` should be `min(node.small_file_budget_bytes)`
/// over the voter set. `projected_files` is the shard's delta-count
/// heuristic; callers pass `1` to avoid divide-by-zero on a fresh
/// empty shard (the helper clamps internally as a defense-in-depth).
///
/// Returns the clamped threshold in bytes.
#[must_use]
pub fn compute_shard_inline_threshold(
    available_bytes: u64,
    projected_files: u64,
    floor_bytes: u64,
    ceiling_bytes: u64,
) -> u64 {
    let denom = projected_files.max(1);
    let raw = available_bytes / denom;
    // `floor.max(ceiling.min(raw))` is the standard clamp without the
    // unstable `Ord::clamp` semantic edge-case when floor > ceiling.
    raw.min(ceiling_bytes).max(floor_bytes)
}

/// Sliding-window throughput tracker for the per-shard Raft inline
/// budget (SF-ADV-1 / I-SF7). Records inline bytes written into the
/// shard's Raft log and reports the rolling MB/s so the leader can
/// drop the effective threshold to floor on a write-storm.
///
/// The window is a `VecDeque<(Instant, u64)>` — entries older than
/// `window_duration` are evicted on every record + query. Memory is
/// bounded by the inline write rate; on a 10 s window at 100 ops/s
/// that's ≤ 1 000 entries (~24 KiB).
pub struct InlineThroughputGuard {
    /// Sliding window of `(written_at, bytes)` records.
    window: VecDeque<(Instant, u64)>,
    /// How far back the window extends. ADR-030 §3 specifies 10 s.
    window_duration: Duration,
    /// Cap (`KISEKI_RAFT_INLINE_MBPS`, default 10 MB/s) above which the
    /// effective threshold drops to floor.
    mbps_limit: u64,
}

impl InlineThroughputGuard {
    /// Build a guard with the ADR-030 defaults (10 s window,
    /// 10 MB/s cap). Most callers use this; the explicit constructor
    /// below is exposed for `KISEKI_RAFT_INLINE_MBPS` overrides and
    /// for the unit tests.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(Duration::from_secs(10), 10)
    }

    /// Build a guard with a custom window + MB/s cap. The MB/s cap is
    /// in `1_000_000` byte-per-second units (decimal MB, matching the
    /// `KISEKI_RAFT_INLINE_MBPS` knob's documented units).
    #[must_use]
    pub fn with_limit(window_duration: Duration, mbps_limit: u64) -> Self {
        Self {
            window: VecDeque::new(),
            window_duration,
            mbps_limit,
        }
    }

    /// Record an inline write of `bytes` at `now`. Evicts entries that
    /// have fallen out of the window before pushing the new one, so the
    /// deque stays bounded.
    pub fn record(&mut self, now: Instant, bytes: u64) {
        self.evict_expired(now);
        self.window.push_back((now, bytes));
    }

    /// Current inline write rate in decimal MB/s over the sliding
    /// window. Returns `0` on an empty window (no inline traffic yet,
    /// or the leader just started). Computes against the window
    /// duration — not against the spread between first / last entry —
    /// so a single recent write doesn't get scored as an infinite
    /// burst rate (which would otherwise rate-limit prematurely on
    /// startup).
    #[must_use]
    pub fn current_mbps(&mut self, now: Instant) -> u64 {
        self.evict_expired(now);
        if self.window.is_empty() {
            return 0;
        }
        let total_bytes: u64 = self.window.iter().map(|(_, b)| *b).sum();
        let window_s = self.window_duration.as_secs().max(1);
        total_bytes / (1_000_000 * window_s)
    }

    /// Effective threshold: the configured value if the current
    /// rolling MB/s is at-or-below the cap, otherwise `floor` so the
    /// next writes get pushed to the chunk fabric and relieve the
    /// Raft log.
    ///
    /// Intended to be called by the leader's periodic recompute
    /// (every few seconds) and by any reader that needs the live
    /// effective number (e.g. the gateway's per-write band check, if
    /// it is wired to consult the guard directly).
    #[must_use]
    pub fn effective_threshold(&mut self, now: Instant, configured: u64, floor: u64) -> u64 {
        if self.current_mbps(now) > self.mbps_limit {
            floor
        } else {
            configured
        }
    }

    /// Cap (`KISEKI_RAFT_INLINE_MBPS`) used by this guard. Surfaced for
    /// metric labels + admin display.
    #[must_use]
    pub const fn mbps_limit(&self) -> u64 {
        self.mbps_limit
    }

    /// Drop window entries older than `now - window_duration`.
    fn evict_expired(&mut self, now: Instant) {
        while let Some(&(ts, _)) = self.window.front() {
            if now.duration_since(ts) > self.window_duration {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for InlineThroughputGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_divides_budget_by_file_count_and_clamps() {
        // 100 MB / 1 000 files = 100 KB raw — clamped to 64 KB ceiling.
        let t = compute_shard_inline_threshold(100 * 1024 * 1024, 1_000, 128, 65_536);
        assert_eq!(t, 65_536);
    }

    #[test]
    fn formula_clamps_to_floor_when_budget_starved() {
        // 1 KB / 100 files = 10 B — clamped UP to the 128 B floor.
        let t = compute_shard_inline_threshold(1_024, 100, 128, 65_536);
        assert_eq!(t, 128);
    }

    #[test]
    fn formula_handles_zero_file_count_without_panic() {
        let t = compute_shard_inline_threshold(8_192, 0, 128, 65_536);
        // Treated as 1 file → 8 KiB raw → unclamped.
        assert_eq!(t, 8_192);
    }

    #[test]
    fn formula_returns_floor_when_floor_exceeds_ceiling() {
        // Defense-in-depth: misconfig (floor > ceiling) shouldn't
        // panic. Result is floor (max wins because clamp is
        // `min(ceiling).max(floor)`).
        let t = compute_shard_inline_threshold(1_000_000, 10, 4_096, 1_024);
        assert_eq!(t, 4_096);
    }

    #[test]
    fn guard_starts_empty_and_returns_configured() {
        let mut g = InlineThroughputGuard::new();
        let now = Instant::now();
        assert_eq!(g.current_mbps(now), 0);
        assert_eq!(g.effective_threshold(now, 8_192, 128), 8_192);
    }

    #[test]
    fn guard_drops_to_floor_when_above_cap() {
        // 1 s window, 1 MB/s cap. Two writes of 2 MB each → 4 MB / 1 s
        // = 4 MB/s rolling.
        let mut g = InlineThroughputGuard::with_limit(Duration::from_secs(1), 1);
        let now = Instant::now();
        g.record(now, 2_000_000);
        g.record(now, 2_000_000);
        assert!(g.current_mbps(now) > 1);
        assert_eq!(g.effective_threshold(now, 8_192, 128), 128);
    }

    #[test]
    fn guard_evicts_expired_entries_on_record() {
        let mut g = InlineThroughputGuard::with_limit(Duration::from_secs(1), 1);
        let t0 = Instant::now();
        g.record(t0, 5_000_000);
        // Two seconds later the entry is stale.
        let t2 = t0 + Duration::from_secs(2);
        assert_eq!(g.current_mbps(t2), 0);
        assert_eq!(g.effective_threshold(t2, 8_192, 128), 8_192);
    }
}
