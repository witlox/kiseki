//! Shared test helpers for step definitions.

use kiseki_common::ids::*;
use kiseki_common::time::*;
use kiseki_log::delta::OperationType;
use kiseki_log::traits::AppendDeltaRequest;

use crate::KisekiWorld;

/// Build an AppendDeltaRequest from common parameters.
pub fn make_append_request(
    world: &KisekiWorld,
    shard_id: ShardId,
    tenant_name: &str,
    operation: OperationType,
    hashed_key: [u8; 32],
    payload: Vec<u8>,
    has_inline_data: bool,
) -> AppendDeltaRequest {
    let tenant_id = world
        .tenant_ids
        .get(tenant_name)
        .copied()
        .unwrap_or_else(|| OrgId(uuid::Uuid::new_v4()));

    AppendDeltaRequest {
        shard_id,
        tenant_id,
        operation,
        timestamp: world.timestamp(),
        hashed_key,
        chunk_refs: vec![],
        payload,
        has_inline_data,
    }
}
