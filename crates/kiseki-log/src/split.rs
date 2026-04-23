//! Shard split execution (WS 2.5).
//!
//! The [`auto_split`](crate::auto_split) module detects when a shard exceeds
//! its I-L6 ceiling. This module computes and validates the split plan,
//! determining the sequence-number midpoint and ensuring both halves
//! meet the minimum size constraint.

use std::fmt;

use kiseki_common::ids::{SequenceNumber, ShardId};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for shard split thresholds and constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitConfig {
    /// Maximum delta count before mandatory split.
    pub max_delta_count: u64,
    /// Maximum byte size before mandatory split (bytes).
    pub max_byte_size: u64,
    /// Minimum number of deltas in each half after split.
    pub min_split_size: u64,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            max_delta_count: 1_000_000,
            max_byte_size: 10 * 1024 * 1024 * 1024, // 10 GiB
            min_split_size: 1000,
        }
    }
}

impl SplitConfig {
    /// Returns `true` when either threshold is breached.
    #[must_use]
    pub fn should_split(&self, delta_count: u64, byte_size: u64) -> bool {
        delta_count >= self.max_delta_count || byte_size >= self.max_byte_size
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// Plan for splitting a shard by sequence-number midpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    /// Source shard being split.
    pub source_shard: ShardId,
    /// New shard that will take the upper sequence range.
    pub new_shard: ShardId,
    /// Sequence number dividing the deltas: source keeps `[0, split_point)`,
    /// new shard gets `[split_point, tip)`.
    pub split_point: SequenceNumber,
    /// Number of deltas going to the new shard.
    pub deltas_to_new: u64,
    /// Number of deltas staying in source.
    pub deltas_remaining: u64,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Outcome of a completed shard split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitResult {
    /// Source shard that was split.
    pub source_shard: ShardId,
    /// Newly created shard.
    pub new_shard: ShardId,
    /// Number of deltas moved to the new shard.
    pub deltas_moved: u64,
    /// Whether the split completed successfully.
    pub success: bool,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Error during split planning or execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitError {
    /// Shard is too small to split — each half would be below `min_split_size`.
    TooSmall {
        /// Current delta count.
        current: u64,
        /// Minimum required (2 * `min_split_size`).
        minimum: u64,
    },
    /// A split is already in progress for this shard.
    AlreadyInProgress(ShardId),
    /// Internal / unexpected error.
    Internal(String),
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall { current, minimum } => {
                write!(
                    f,
                    "shard too small to split: {current} deltas, need at least {minimum}"
                )
            }
            Self::AlreadyInProgress(id) => {
                write!(f, "split already in progress for shard {id:?}")
            }
            Self::Internal(msg) => write!(f, "internal split error: {msg}"),
        }
    }
}

impl std::error::Error for SplitError {}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Compute a split plan for a shard.
///
/// The split point is the sequence-number midpoint (`delta_count / 2`).
/// Returns [`SplitError::TooSmall`] when the shard cannot be divided into
/// two halves that each satisfy `config.min_split_size`.
pub fn compute_split_plan(
    shard_id: ShardId,
    delta_count: u64,
    byte_size: u64,
    config: &SplitConfig,
) -> Result<SplitPlan, SplitError> {
    let minimum = config.min_split_size.saturating_mul(2);
    if delta_count < minimum {
        return Err(SplitError::TooSmall {
            current: delta_count,
            minimum,
        });
    }

    // Guard: thresholds should actually be exceeded.
    if !config.should_split(delta_count, byte_size) {
        return Err(SplitError::Internal(
            "split requested but no threshold breached".to_string(),
        ));
    }

    let midpoint = delta_count / 2;
    let deltas_remaining = midpoint;
    let deltas_to_new = delta_count - midpoint;

    Ok(SplitPlan {
        source_shard: shard_id,
        new_shard: ShardId(uuid::Uuid::new_v4()),
        split_point: SequenceNumber(midpoint),
        deltas_to_new,
        deltas_remaining,
    })
}

// ---------------------------------------------------------------------------
// Detection-to-execution bridge
// ---------------------------------------------------------------------------

/// Check if a shard needs splitting and compute the plan if so.
///
/// Returns `Some(plan)` when either the delta count or byte size threshold
/// is breached and the shard is large enough to split. Returns `None`
/// when no split is needed or the shard is too small.
#[must_use]
pub fn check_and_plan(
    shard_id: ShardId,
    delta_count: u64,
    byte_size: u64,
    config: &SplitConfig,
) -> Option<SplitPlan> {
    if config.should_split(delta_count, byte_size) {
        compute_split_plan(shard_id, delta_count, byte_size, config).ok()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SplitConfig {
        SplitConfig {
            max_delta_count: 100,
            max_byte_size: 1024,
            min_split_size: 10,
        }
    }

    // -- SplitConfig::should_split -------------------------------------------

    #[test]
    fn should_split_detects_delta_ceiling_breach() {
        let cfg = test_config();
        assert!(cfg.should_split(100, 0));
        assert!(cfg.should_split(200, 0));
    }

    #[test]
    fn should_split_detects_byte_ceiling_breach() {
        let cfg = test_config();
        assert!(cfg.should_split(0, 1024));
        assert!(cfg.should_split(0, 2048));
    }

    #[test]
    fn should_split_returns_false_below_thresholds() {
        let cfg = test_config();
        assert!(!cfg.should_split(99, 1023));
        assert!(!cfg.should_split(0, 0));
    }

    // -- compute_split_plan --------------------------------------------------

    #[test]
    fn compute_split_plan_generates_valid_midpoint() {
        let cfg = test_config();
        let shard = ShardId(uuid::Uuid::from_u128(1));

        let plan = compute_split_plan(shard, 200, 0, &cfg).unwrap();

        assert_eq!(plan.source_shard, shard);
        assert_eq!(plan.split_point, SequenceNumber(100));
        assert_eq!(plan.deltas_remaining, 100);
        assert_eq!(plan.deltas_to_new, 100);
        // New shard should have a different ID.
        assert_ne!(plan.new_shard, shard);
    }

    #[test]
    fn compute_split_plan_odd_count_distributes_correctly() {
        let cfg = test_config();
        let shard = ShardId(uuid::Uuid::from_u128(2));

        let plan = compute_split_plan(shard, 201, 0, &cfg).unwrap();

        // Integer division: 201 / 2 = 100 remaining, 101 to new.
        assert_eq!(plan.deltas_remaining, 100);
        assert_eq!(plan.deltas_to_new, 101);
        assert_eq!(
            plan.deltas_remaining + plan.deltas_to_new,
            201,
            "delta distribution must be exhaustive"
        );
    }

    #[test]
    fn too_small_error_when_shard_is_tiny() {
        let cfg = test_config(); // min_split_size = 10, so minimum = 20
        let shard = ShardId(uuid::Uuid::from_u128(3));

        // 19 < 20 => TooSmall, even though threshold is exceeded.
        let err = compute_split_plan(shard, 19, 2048, &cfg).unwrap_err();
        assert_eq!(
            err,
            SplitError::TooSmall {
                current: 19,
                minimum: 20
            }
        );
    }

    #[test]
    fn split_plan_has_correct_delta_distribution() {
        let cfg = test_config();
        let shard = ShardId(uuid::Uuid::from_u128(4));

        let plan = compute_split_plan(shard, 500, 0, &cfg).unwrap();

        assert_eq!(plan.deltas_remaining, 250);
        assert_eq!(plan.deltas_to_new, 250);
        assert_eq!(
            plan.deltas_remaining + plan.deltas_to_new,
            500,
            "all deltas must be accounted for"
        );
    }

    #[test]
    fn error_when_no_threshold_breached() {
        let cfg = test_config();
        let shard = ShardId(uuid::Uuid::from_u128(5));

        // 50 deltas, 500 bytes — both below thresholds.
        let err = compute_split_plan(shard, 50, 500, &cfg).unwrap_err();
        assert!(matches!(err, SplitError::Internal(_)));
    }

    #[test]
    fn default_config_has_sensible_values() {
        let cfg = SplitConfig::default();
        assert_eq!(cfg.max_delta_count, 1_000_000);
        assert_eq!(cfg.max_byte_size, 10 * 1024 * 1024 * 1024);
        assert_eq!(cfg.min_split_size, 1000);
    }

    #[test]
    fn split_error_display() {
        let err = SplitError::TooSmall {
            current: 5,
            minimum: 20,
        };
        let msg = err.to_string();
        assert!(msg.contains('5'));
        assert!(msg.contains("20"));
    }

    // -- check_and_plan -------------------------------------------------------

    #[test]
    fn check_and_plan_returns_some_when_threshold_exceeded() {
        let cfg = test_config(); // max_delta_count=100, max_byte_size=1024, min_split_size=10
        let shard = ShardId(uuid::Uuid::from_u128(10));

        // 200 deltas > 100 threshold, and 200 >= 2*min_split_size(10)
        let plan = check_and_plan(shard, 200, 0, &cfg);
        assert!(plan.is_some());
        let plan = plan.unwrap();
        assert_eq!(plan.source_shard, shard);
        assert_eq!(plan.split_point, SequenceNumber(100));
    }

    #[test]
    fn check_and_plan_returns_none_when_below_threshold() {
        let cfg = test_config();
        let shard = ShardId(uuid::Uuid::from_u128(11));

        // 50 deltas < 100 threshold, 500 bytes < 1024 threshold
        let plan = check_and_plan(shard, 50, 500, &cfg);
        assert!(plan.is_none());
    }
}
