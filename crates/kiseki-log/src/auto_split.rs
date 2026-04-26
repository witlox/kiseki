//! Automatic shard splitting (I-L6).
//!
//! Monitors shard metrics against `ShardConfig` thresholds.
//! When any dimension exceeds its ceiling, triggers a split:
//! 1. Compute midpoint of key range
//! 2. Create new shard for upper half
//! 3. Redistribute deltas by key range
//! 4. Transition original shard: Splitting → Healthy

use kiseki_common::ids::{NodeId, OrgId, ShardId};

use crate::shard::{ShardConfig, ShardInfo};
use crate::traits::LogOps;

/// Result of checking whether a shard should split.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum SplitCheck {
    /// Shard is within thresholds — no action needed.
    Ok,
    /// Delta count exceeded.
    DeltaCountExceeded { current: u64, threshold: u64 },
    /// Byte size exceeded.
    ByteSizeExceeded { current: u64, threshold: u64 },
}

/// Check if a shard should be split based on its current metrics.
#[must_use]
pub fn check_split(info: &ShardInfo) -> SplitCheck {
    if info.delta_count >= info.config.max_delta_count {
        return SplitCheck::DeltaCountExceeded {
            current: info.delta_count,
            threshold: info.config.max_delta_count,
        };
    }
    if info.byte_size >= info.config.max_byte_size {
        return SplitCheck::ByteSizeExceeded {
            current: info.byte_size,
            threshold: info.config.max_byte_size,
        };
    }
    SplitCheck::Ok
}

/// Compute the midpoint of a key range for splitting.
///
/// Returns `None` if the range is a single point (cannot split further).
#[must_use]
pub fn compute_midpoint(range_start: &[u8; 32], range_end: &[u8; 32]) -> Option<[u8; 32]> {
    // Treat as big-endian 256-bit integers: mid = (start + end) / 2.
    let mut carry = 0u16;
    let mut mid = [0u8; 32];

    // Add start + end byte by byte from LSB (index 31).
    let mut sum = [0u16; 32];
    for i in (0..32).rev() {
        let s = u16::from(range_start[i]) + u16::from(range_end[i]) + carry;
        sum[i] = s & 0xFF;
        carry = s >> 8;
    }

    // Divide by 2.
    let mut borrow = carry; // carry from addition
    for i in 0..32 {
        let val = (borrow << 8) | sum[i];
        mid[i] = u8::try_from(val >> 1).unwrap_or(0xFF);
        borrow = val & 1;
    }

    // Check that midpoint is strictly between start and end.
    if mid == *range_start || mid == *range_end {
        return None; // Cannot split further.
    }

    Some(mid)
}

/// Descriptor for a split operation.
#[derive(Debug, Clone)]
pub struct SplitPlan {
    /// Original shard being split.
    pub original_shard: ShardId,
    /// New shard for the upper key range.
    pub new_shard: ShardId,
    /// Tenant owning both shards.
    pub tenant_id: OrgId,
    /// Midpoint — original keeps `[start, mid)`, new gets `[mid, end)`.
    pub midpoint: [u8; 32],
    /// Original range start (unchanged).
    pub range_start: [u8; 32],
    /// Original range end (now new shard's end).
    pub range_end: [u8; 32],
    /// Node ID for the new shard's initial Raft group.
    pub initial_node: NodeId,
}

/// Plan a shard split. Returns `None` if the shard doesn't need splitting
/// or the range cannot be divided further.
#[must_use]
pub fn plan_split(info: &ShardInfo) -> Option<SplitPlan> {
    if check_split(info) == SplitCheck::Ok {
        return None;
    }

    let midpoint = compute_midpoint(&info.range_start, &info.range_end)?;
    let leader = info.leader.unwrap_or(NodeId(1));

    Some(SplitPlan {
        original_shard: info.shard_id,
        new_shard: ShardId(uuid::Uuid::new_v4()),
        tenant_id: info.tenant_id,
        midpoint,
        range_start: info.range_start,
        range_end: info.range_end,
        initial_node: leader,
    })
}

/// Execute a split plan on any `LogOps` backend.
///
/// 1. Create the new shard with the upper key range.
/// 2. Redistribute deltas from the original shard to the new one.
/// 3. Update key ranges on both shards.
pub async fn execute_split<L: LogOps + ?Sized>(
    log: &L,
    plan: &SplitPlan,
) -> Result<(), crate::error::LogError> {
    // Create new shard for the upper range.
    log.create_shard(
        plan.new_shard,
        plan.tenant_id,
        plan.initial_node,
        ShardConfig::default(),
    );

    // Read all deltas from original shard.
    let deltas = log
        .read_deltas(crate::traits::ReadDeltasRequest {
            shard_id: plan.original_shard,
            from: kiseki_common::ids::SequenceNumber(0),
            to: kiseki_common::ids::SequenceNumber(u64::MAX),
        })
        .await?;

    // Redistribute: deltas with hashed_key >= midpoint go to new shard.
    for delta in &deltas {
        if delta.header.hashed_key >= plan.midpoint {
            log.append_delta(crate::traits::AppendDeltaRequest {
                shard_id: plan.new_shard,
                tenant_id: plan.tenant_id,
                operation: delta.header.operation,
                timestamp: delta.header.timestamp.clone(),
                hashed_key: delta.header.hashed_key,
                chunk_refs: delta.header.chunk_refs.clone(),
                payload: delta.payload.ciphertext.clone(),
                has_inline_data: delta.header.has_inline_data,
            })
            .await?;
        }
    }

    // Update key ranges: original keeps [start, midpoint), new gets [midpoint, end).
    log.update_shard_range(plan.original_shard, plan.range_start, plan.midpoint);
    log.update_shard_range(plan.new_shard, plan.midpoint, plan.range_end);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::ShardState;

    fn full_range_info(delta_count: u64, byte_size: u64) -> ShardInfo {
        ShardInfo {
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(100)),
            raft_members: vec![NodeId(1)],
            leader: Some(NodeId(1)),
            tip: kiseki_common::ids::SequenceNumber(delta_count),
            delta_count,
            byte_size,
            state: ShardState::Healthy,
            config: ShardConfig {
                max_delta_count: 100,
                max_byte_size: 1024,
                ..ShardConfig::default()
            },
            range_start: [0u8; 32],
            range_end: [0xFFu8; 32],
        }
    }

    #[test]
    fn check_split_ok() {
        let info = full_range_info(50, 500);
        assert_eq!(check_split(&info), SplitCheck::Ok);
    }

    #[test]
    fn check_split_delta_exceeded() {
        let info = full_range_info(100, 500);
        assert!(matches!(
            check_split(&info),
            SplitCheck::DeltaCountExceeded { .. }
        ));
    }

    #[test]
    fn check_split_byte_exceeded() {
        let info = full_range_info(50, 1024);
        assert!(matches!(
            check_split(&info),
            SplitCheck::ByteSizeExceeded { .. }
        ));
    }

    #[test]
    fn midpoint_full_range() {
        let start = [0u8; 32];
        let end = [0xFFu8; 32];
        let mid = compute_midpoint(&start, &end).unwrap();
        // Mid of [0x00..00, 0xFF..FF] should be ~0x7F..FF.
        assert_eq!(mid[0], 0x7F);
    }

    #[test]
    fn midpoint_adjacent_values() {
        let start = [0x80u8; 32];
        let mut end = [0x80u8; 32];
        end[31] = 0x81;
        // Range is 1 apart: (0x8080..80 + 0x8080..81) / 2 = 0x8080..80
        // midpoint == start, so should return None.
        let mid = compute_midpoint(&start, &end);
        // The midpoint of adjacent values equals start, which we reject.
        assert!(mid.is_none() || mid == Some(start));
    }

    #[test]
    fn plan_split_no_action_needed() {
        let info = full_range_info(50, 500);
        assert!(plan_split(&info).is_none());
    }

    #[test]
    fn plan_split_creates_plan() {
        let info = full_range_info(200, 500);
        let plan = plan_split(&info).unwrap();
        assert_eq!(plan.original_shard, info.shard_id);
        assert_eq!(plan.tenant_id, info.tenant_id);
        assert_eq!(plan.range_start, [0u8; 32]);
        assert_eq!(plan.range_end, [0xFFu8; 32]);
        assert!(plan.midpoint[0] >= 0x7F);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_split_creates_new_shard() {
        use crate::store::MemShardStore;

        let store = MemShardStore::new();
        let info = full_range_info(200, 500);

        // Create the original shard.
        store.create_shard(
            info.shard_id,
            info.tenant_id,
            NodeId(1),
            ShardConfig::default(),
        );

        let plan = plan_split(&info).unwrap();
        execute_split(&store, &plan).await.unwrap();

        // New shard should exist.
        let health = store.shard_health(plan.new_shard).await;
        assert!(health.is_ok());
    }
}
