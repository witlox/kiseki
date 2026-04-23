//! ADR-030: Per-shard inline threshold feedback.
//!
//! The control plane computes a per-shard inline threshold from
//! cluster-wide capacity data reported by voter nodes.  This implements
//! I-SF1: min budget across voters / file count, clamped to
//! `[INLINE_FLOOR, INLINE_CEILING]`.

use kiseki_common::ids::ShardId;

/// Per-node capacity report sent by each voter.
#[derive(Debug, Clone)]
pub struct NodeCapacityReport {
    /// Reporting node identifier.
    pub node_id: u64,
    /// Total disk capacity in bytes.
    pub total_bytes: u64,
    /// Currently used bytes.
    pub used_bytes: u64,
    /// Soft limit — advisory threshold for reducing inline size.
    pub soft_limit_bytes: u64,
    /// Hard limit — emergency threshold.
    pub hard_limit_bytes: u64,
}

/// Computed inline threshold for a shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardThreshold {
    /// Shard this threshold applies to.
    pub shard_id: ShardId,
    /// Recommended inline threshold in bytes.
    pub threshold_bytes: u64,
    /// Reason for the chosen threshold.
    pub reason: ThresholdReason,
}

/// Why a particular threshold value was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThresholdReason {
    /// Capacity is healthy; normal inline budget.
    Normal,
    /// At least one voter is approaching its soft limit.
    SoftLimitApproaching,
    /// At least one voter has breached its hard limit.
    HardLimitBreached,
    /// Emergency: threshold was clamped to the floor.
    EmergencyFloor,
}

/// Inline threshold floor (bytes).  No shard threshold may go below this.
pub const INLINE_FLOOR: u64 = 128;

/// Inline threshold ceiling (bytes).  No shard threshold may exceed this.
pub const INLINE_CEILING: u64 = 65536;

/// Compute the inline threshold for a shard from voter capacity reports.
///
/// Algorithm (I-SF1):
/// 1. For each voter, compute `available = total - used`.
/// 2. Take the minimum available budget across all voters.
/// 3. Divide by `file_count_estimate` (guarding against zero).
/// 4. Clamp to `[INLINE_FLOOR, INLINE_CEILING]`.
/// 5. If any voter has breached its hard limit, force the floor.
#[must_use]
pub fn compute_shard_threshold(
    shard_id: ShardId,
    voter_reports: &[NodeCapacityReport],
    file_count_estimate: u64,
) -> ShardThreshold {
    // If any voter is in emergency, force floor immediately.
    if voter_reports.iter().any(is_emergency) {
        return ShardThreshold {
            shard_id,
            threshold_bytes: INLINE_FLOOR,
            reason: ThresholdReason::EmergencyFloor,
        };
    }

    // Minimum available budget across all voters.
    let min_available = voter_reports
        .iter()
        .map(|r| r.total_bytes.saturating_sub(r.used_bytes))
        .min()
        .unwrap_or(0);

    // Guard against division by zero.
    let divisor = file_count_estimate.max(1);
    let raw = min_available / divisor;

    // Clamp to [FLOOR, CEILING].
    let threshold_bytes = raw.clamp(INLINE_FLOOR, INLINE_CEILING);

    // Determine reason.
    let reason = if voter_reports
        .iter()
        .any(|r| r.used_bytes >= r.soft_limit_bytes)
    {
        ThresholdReason::SoftLimitApproaching
    } else if threshold_bytes == INLINE_FLOOR {
        ThresholdReason::EmergencyFloor
    } else {
        ThresholdReason::Normal
    };

    ShardThreshold {
        shard_id,
        threshold_bytes,
        reason,
    }
}

/// Returns `true` if the node has exceeded its hard capacity limit.
#[must_use]
pub fn is_emergency(report: &NodeCapacityReport) -> bool {
    report.used_bytes > report.hard_limit_bytes
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(42))
    }

    fn healthy_report(node_id: u64) -> NodeCapacityReport {
        NodeCapacityReport {
            node_id,
            total_bytes: 1_000_000,
            used_bytes: 200_000,
            soft_limit_bytes: 800_000,
            hard_limit_bytes: 900_000,
        }
    }

    #[test]
    fn normal_computation() {
        let reports = vec![healthy_report(1), healthy_report(2), healthy_report(3)];
        // min available = 1_000_000 - 200_000 = 800_000
        // raw = 800_000 / 100 = 8_000
        let result = compute_shard_threshold(shard(), &reports, 100);
        assert_eq!(result.threshold_bytes, 8_000);
        assert_eq!(result.reason, ThresholdReason::Normal);
    }

    #[test]
    fn emergency_floor_when_hard_limit_breached() {
        let mut reports = vec![healthy_report(1), healthy_report(2)];
        // Push node 2 past its hard limit.
        reports[1].used_bytes = 950_000; // > hard_limit 900_000
        let result = compute_shard_threshold(shard(), &reports, 100);
        assert_eq!(result.threshold_bytes, INLINE_FLOOR);
        assert_eq!(result.reason, ThresholdReason::EmergencyFloor);
    }

    #[test]
    fn ceiling_clamp() {
        let reports = vec![healthy_report(1)];
        // min available = 800_000, file_count = 1 => raw = 800_000
        // Clamped to INLINE_CEILING = 65_536
        let result = compute_shard_threshold(shard(), &reports, 1);
        assert_eq!(result.threshold_bytes, INLINE_CEILING);
        assert_eq!(result.reason, ThresholdReason::Normal);
    }

    #[test]
    fn emergency_triggered_when_any_voter_at_hard_limit() {
        // Even if only one voter out of many breaches the hard limit,
        // emergency floor must be triggered.
        let mut reports = vec![healthy_report(1), healthy_report(2), healthy_report(3)];
        // Only node 3 breaches hard limit.
        reports[2].used_bytes = reports[2].hard_limit_bytes + 1;

        let result = compute_shard_threshold(shard(), &reports, 100);
        assert_eq!(result.threshold_bytes, INLINE_FLOOR);
        assert_eq!(result.reason, ThresholdReason::EmergencyFloor);
    }

    #[test]
    fn soft_limit_approaching_reason() {
        let mut reports = vec![healthy_report(1), healthy_report(2)];
        // Push node 1 to soft limit (but not past hard limit).
        reports[0].used_bytes = reports[0].soft_limit_bytes;
        // used < hard_limit, so not emergency.
        let result = compute_shard_threshold(shard(), &reports, 100);
        assert_eq!(
            result.reason,
            ThresholdReason::SoftLimitApproaching,
            "should report SoftLimitApproaching when a voter hits soft limit"
        );
    }
}
