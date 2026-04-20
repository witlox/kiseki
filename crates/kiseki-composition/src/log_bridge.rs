//! Bridge between Composition and Log contexts.
//!
//! When attached to a `CompositionStore`, composition mutations
//! (create, update, delete) emit deltas to the log shard. This
//! wires the Composition → Log data path per api-contracts.md.

use kiseki_common::ids::{ChunkId, NodeId, OrgId, ShardId};
use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
use kiseki_log::delta::OperationType;
use kiseki_log::traits::{AppendDeltaRequest, LogOps};

/// Emit a delta to the log for a composition mutation.
///
/// Called by `CompositionStore` after a successful create/update/delete.
/// The payload is the composition ID serialized as bytes (opaque to
/// the log per I-L7).
pub(crate) fn emit_delta<L: LogOps + ?Sized>(
    log: &L,
    shard_id: ShardId,
    tenant_id: OrgId,
    operation: OperationType,
    hashed_key: [u8; 32],
    chunk_refs: Vec<ChunkId>,
    payload: Vec<u8>,
) {
    let timestamp = now_timestamp();
    let req = AppendDeltaRequest {
        shard_id,
        tenant_id,
        operation,
        timestamp,
        hashed_key,
        chunk_refs,
        payload,
        has_inline_data: false,
    };
    // Best-effort: log errors are not propagated to the composition
    // caller because the composition mutation already succeeded in
    // the local store. The delta will be retried or detected as
    // missing by the stream processor.
    let _ = log.append_delta(req);
}

fn now_timestamp() -> DeltaTimestamp {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    DeltaTimestamp {
        hlc: HybridLogicalClock {
            physical_ms: now_ms,
            logical: 0,
            node_id: NodeId(0),
        },
        wall: WallTime {
            millis_since_epoch: now_ms,
            timezone: "UTC".into(),
        },
        quality: ClockQuality::Ntp,
    }
}
