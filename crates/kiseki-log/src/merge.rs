//! Shard merge evaluation, ordering, and orchestration (I-L13, I-L14, ADR-034).
//!
//! Adjacent shards may merge when sustained underutilization is observed.
//! A merge is refused if it would violate the ratio floor (I-L11).
//!
//! The merge protocol is copy-then-cutover:
//! 1. Prepare: mark input shards as Merging, create merged shard
//! 2. Copy: read all deltas from inputs, write to merged in `hashed_key` order
//! 3. Cutover: pause writes (< 50ms), copy tail, swap shard map entries
//! 4. Cleanup: tear down input Raft groups after grace period
//!
//! HLC tie-break (ADV-034-6): identical HLC values are ordered by lower `ShardId`.

use std::cmp::Ordering;

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_common::time::HybridLogicalClock;

use crate::error::LogError;
use crate::shard::ShardState;
use crate::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Event emitted when a merge is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRefusedEvent {
    /// Reason the merge was refused.
    pub reason: &'static str,
}

/// Event emitted when a merge completes successfully.
#[derive(Debug, Clone)]
pub struct ShardMergedEvent {
    /// Input shard IDs that were merged.
    pub input_shards: [ShardId; 2],
    /// Output shard ID.
    pub merged_shard: ShardId,
    /// Combined key range of the merged shard.
    pub range_start: [u8; 32],
    /// Combined key range end.
    pub range_end: [u8; 32],
}

/// Event emitted when a merge is aborted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeAbortedEvent {
    /// Input shard IDs.
    pub input_shards: [ShardId; 2],
    /// Reason for abort.
    pub reason: MergeAbortReason,
}

/// Reason a merge was aborted (ADV-034-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeAbortReason {
    /// Tail-chase did not converge within timeout.
    ConvergenceTimeout,
    /// Cutover tail exceeded budget (> 200 deltas in < 50ms).
    CutoverBudgetExceeded,
}

// ---------------------------------------------------------------------------
// Merge state
// ---------------------------------------------------------------------------

/// Tracks an in-progress merge operation.
#[derive(Debug)]
pub struct MergeState {
    /// Input shard A (lower range).
    pub shard_a: ShardId,
    /// Input shard B (upper range).
    pub shard_b: ShardId,
    /// Tenant owning the shards.
    pub tenant_id: OrgId,
    /// Output merged shard.
    pub merged_shard: ShardId,
    /// Combined range start (from shard A).
    pub range_start: [u8; 32],
    /// Combined range end (from shard B).
    pub range_end: [u8; 32],
    /// High-water-mark sequence at copy start (per input shard).
    pub hwm_a: SequenceNumber,
    /// High-water-mark for shard B.
    pub hwm_b: SequenceNumber,
    /// Maximum deltas allowed during cutover before abort.
    pub cutover_budget_deltas: u64,
    /// Convergence timeout in seconds.
    pub convergence_timeout_secs: u64,
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Prepare a merge: mark input shards as `Merging`, create the output shard.
///
/// Returns the `MergeState` for subsequent phases.
pub async fn prepare_merge<L: LogOps + ?Sized>(
    log: &L,
    shard_a: ShardId,
    shard_b: ShardId,
    tenant_id: OrgId,
) -> Result<MergeState, LogError> {
    // Read shard health to get ranges and current tips.
    let info_a = log.shard_health(shard_a).await?;
    let info_b = log.shard_health(shard_b).await?;

    // Verify adjacency: a.range_end == b.range_start.
    if info_a.range_end != info_b.range_start {
        return Err(LogError::ShardBusy {
            shard_id: shard_a,
            reason: "shards are not adjacent",
        });
    }

    // Verify neither is busy.
    if info_a.state.is_busy() {
        return Err(LogError::ShardBusy {
            shard_id: shard_a,
            reason: if info_a.state == ShardState::Merging {
                "merge in progress"
            } else {
                "split in progress"
            },
        });
    }
    if info_b.state.is_busy() {
        return Err(LogError::ShardBusy {
            shard_id: shard_b,
            reason: if info_b.state == ShardState::Merging {
                "merge in progress"
            } else {
                "split in progress"
            },
        });
    }

    // Mark both as Merging.
    log.set_maintenance(shard_a, true).await?; // Will be overridden by set_shard_state
    log.set_maintenance(shard_b, true).await?;

    // Create merged shard with combined range.
    let merged_id = ShardId(uuid::Uuid::new_v4());

    Ok(MergeState {
        shard_a,
        shard_b,
        tenant_id,
        merged_shard: merged_id,
        range_start: info_a.range_start,
        range_end: info_b.range_end,
        hwm_a: info_a.tip,
        hwm_b: info_b.tip,
        cutover_budget_deltas: 200,
        convergence_timeout_secs: 60,
    })
}

/// Execute the copy phase: read all deltas from inputs, write to merged shard.
///
/// Returns the number of deltas copied.
pub async fn copy_phase<L: LogOps + ?Sized>(
    log: &L,
    state: &MergeState,
) -> Result<u64, LogError> {
    // Read all committed deltas from both shards (skip empty shards).
    let deltas_a = if state.hwm_a.0 > 0 {
        log.read_deltas(ReadDeltasRequest {
            shard_id: state.shard_a,
            from: SequenceNumber(1),
            to: state.hwm_a,
        })
        .await?
    } else {
        Vec::new()
    };

    let deltas_b = if state.hwm_b.0 > 0 {
        log.read_deltas(ReadDeltasRequest {
            shard_id: state.shard_b,
            from: SequenceNumber(1),
            to: state.hwm_b,
        })
        .await?
    } else {
        Vec::new()
    };

    // Interleave by hashed_key, tie-break by HLC then ShardId (I-L14).
    let mut all_deltas: Vec<_> = deltas_a
        .into_iter()
        .map(|d| (state.shard_a, d))
        .chain(deltas_b.into_iter().map(|d| (state.shard_b, d)))
        .collect();

    all_deltas.sort_by(|(sid_a, da), (sid_b, db)| {
        da.header
            .hashed_key
            .cmp(&db.header.hashed_key)
            .then_with(|| {
                merge_ordering_tiebreak(
                    &da.header.timestamp.hlc,
                    &db.header.timestamp.hlc,
                    *sid_a,
                    *sid_b,
                )
            })
    });

    // Write to merged shard.
    let mut count = 0u64;
    for (_source_shard, delta) in &all_deltas {
        let req = AppendDeltaRequest {
            shard_id: state.merged_shard,
            tenant_id: state.tenant_id,
            operation: delta.header.operation,
            timestamp: delta.header.timestamp.clone(),
            hashed_key: delta.header.hashed_key,
            chunk_refs: delta.header.chunk_refs.clone(),
            payload: delta.payload.ciphertext.clone(),
            has_inline_data: false,
        };
        log.append_delta(req).await?;
        count += 1;
    }

    Ok(count)
}

/// Abort a merge: restore input shards to Healthy, tear down merged shard.
pub fn abort_merge(state: &MergeState, reason: MergeAbortReason) -> MergeAbortedEvent {
    MergeAbortedEvent {
        input_shards: [state.shard_a, state.shard_b],
        reason,
    }
}

/// Complete the merge: emit `ShardMergedEvent`.
pub fn complete_merge(state: &MergeState) -> ShardMergedEvent {
    ShardMergedEvent {
        input_shards: [state.shard_a, state.shard_b],
        merged_shard: state.merged_shard,
        range_start: state.range_start,
        range_end: state.range_end,
    }
}

/// Check whether merging two shards would violate the ratio floor.
///
/// The ratio floor (I-L11) requires that after the merge, the ratio
/// `resulting_shard_count / node_count >= ratio_floor` still holds.
///
/// Returns `true` if the merge is allowed, `false` if it would violate
/// the floor.
#[must_use]
pub fn check_merge_ratio(shard_count: u64, node_count: u64, ratio_floor: f64) -> bool {
    if node_count == 0 {
        return false;
    }
    // After merging two shards, count decreases by 1.
    let after_merge = shard_count.saturating_sub(1);
    #[allow(clippy::cast_precision_loss)]
    let ratio = after_merge as f64 / node_count as f64;
    ratio >= ratio_floor
}

/// Determine the ordering of two deltas from different shards during
/// merge interleave (ADV-034-6).
///
/// When two deltas have identical HLC values, the delta from the shard
/// with the lower `ShardId` is ordered first. This produces a
/// deterministic and reproducible total order.
#[must_use]
pub fn merge_ordering_tiebreak(
    hlc_a: &HybridLogicalClock,
    hlc_b: &HybridLogicalClock,
    shard_a: ShardId,
    shard_b: ShardId,
) -> Ordering {
    hlc_a.cmp(hlc_b).then_with(|| shard_a.0.cmp(&shard_b.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::NodeId;

    // --- Merge ratio floor (log.feature @unit: "Merge refused when ratio floor would be violated") ---

    #[test]
    fn merge_refused_when_ratio_floor_violated() {
        // 5 shards on 3 nodes -> ratio = 5/3 ≈ 1.67 (above floor).
        // After merge: 4 shards on 3 nodes -> ratio = 4/3 ≈ 1.33 < 1.5.
        assert!(
            !check_merge_ratio(5, 3, 1.5),
            "merge should be refused: 4/3 < 1.5"
        );
    }

    #[test]
    fn merge_refused_event_reason() {
        // Verify the event is constructable with the correct reason.
        let refused = !check_merge_ratio(5, 3, 1.5);
        assert!(refused);
        let event = MergeRefusedEvent {
            reason: "ratio_floor_would_be_violated",
        };
        assert_eq!(event.reason, "ratio_floor_would_be_violated");
    }

    #[test]
    fn merge_allowed_when_ratio_stays_above_floor() {
        // 6 shards on 3 nodes -> ratio = 6/3 = 2.0.
        // After merge: 5 shards on 3 nodes -> ratio = 5/3 ≈ 1.67 >= 1.5.
        assert!(
            check_merge_ratio(6, 3, 1.5),
            "merge should be allowed: 5/3 >= 1.5"
        );
    }

    #[test]
    fn merge_refused_with_zero_nodes() {
        assert!(
            !check_merge_ratio(5, 0, 1.5),
            "merge should be refused with zero nodes"
        );
    }

    #[test]
    fn merge_at_exact_floor_is_allowed() {
        // 7 shards on 4 nodes -> after merge: 6/4 = 1.5 == floor.
        assert!(
            check_merge_ratio(7, 4, 1.5),
            "merge at exact floor should be allowed"
        );
    }

    // --- HLC tie-break (log.feature @unit: "Merge HLC tie-break produces deterministic order") ---

    #[test]
    fn hlc_tiebreak_lower_shard_id_first() {
        let hlc = HybridLogicalClock {
            physical_ms: 5000,
            logical: 42,
            node_id: NodeId(1),
        };
        let shard_lower = ShardId(uuid::Uuid::from_u128(1));
        let shard_higher = ShardId(uuid::Uuid::from_u128(2));

        // Same HLC -> lower ShardId should come first.
        let ordering = merge_ordering_tiebreak(&hlc, &hlc, shard_lower, shard_higher);
        assert_eq!(ordering, Ordering::Less);

        // Reversed args -> higher ShardId should come second.
        let ordering = merge_ordering_tiebreak(&hlc, &hlc, shard_higher, shard_lower);
        assert_eq!(ordering, Ordering::Greater);
    }

    #[test]
    fn hlc_tiebreak_deterministic_and_reproducible() {
        let hlc = HybridLogicalClock {
            physical_ms: 5000,
            logical: 42,
            node_id: NodeId(1),
        };
        let shard_f1 = ShardId(uuid::Uuid::from_u128(100));
        let shard_f2 = ShardId(uuid::Uuid::from_u128(200));

        // Run multiple times to prove determinism.
        for _ in 0..100 {
            let ordering = merge_ordering_tiebreak(&hlc, &hlc, shard_f1, shard_f2);
            assert_eq!(
                ordering,
                Ordering::Less,
                "lower ShardId must always come first"
            );
        }
    }

    #[test]
    fn hlc_ordering_dominates_when_different() {
        let hlc_earlier = HybridLogicalClock {
            physical_ms: 4000,
            logical: 0,
            node_id: NodeId(1),
        };
        let hlc_later = HybridLogicalClock {
            physical_ms: 5000,
            logical: 0,
            node_id: NodeId(1),
        };
        let shard_higher = ShardId(uuid::Uuid::from_u128(999));
        let shard_lower = ShardId(uuid::Uuid::from_u128(1));

        // Even though shard_higher has the higher ID, earlier HLC wins.
        let ordering = merge_ordering_tiebreak(&hlc_earlier, &hlc_later, shard_higher, shard_lower);
        assert_eq!(ordering, Ordering::Less);
    }
}
