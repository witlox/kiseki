//! gRPC service implementation for the Log context.
//!
//! Wraps a `dyn LogOps` behind the tonic-generated `LogService` trait.
//! Maps domain errors to `tonic::Status`.

use kiseki_common::ids::{ChunkId, OrgId, SequenceNumber, ShardId};
use kiseki_proto::v1::log_service_server::LogService;
use kiseki_proto::v1::{
    AppendChunkAndDeltaRequest as ProtoChunkAppendReq, AppendDeltaRequest as ProtoAppendReq,
    AppendDeltaResponse, ReadDeltasRequest as ProtoReadReq, ReadDeltasResponse,
    SetMaintenanceRequest, SetMaintenanceResponse, ShardHealthRequest, ShardHealthResponse,
};
use tonic::{Request, Response, Status};

use crate::error::LogError;
use crate::traits::LogOps;

/// gRPC handler wrapping a `LogOps` implementation.
pub struct LogGrpc {
    ops: std::sync::Arc<dyn LogOps + Send + Sync>,
}

impl LogGrpc {
    /// Create a new gRPC handler.
    #[must_use]
    pub fn new(ops: std::sync::Arc<dyn LogOps + Send + Sync>) -> Self {
        Self { ops }
    }
}

fn to_status(e: &LogError) -> Status {
    match e {
        LogError::ShardNotFound(_) => Status::not_found(e.to_string()),
        LogError::MaintenanceMode(_) => Status::failed_precondition(e.to_string()),
        LogError::ShardSplitting(_)
        | LogError::LeaderUnavailable(_)
        | LogError::ForwardToLeader { .. }
        | LogError::QuorumLost(_)
        | LogError::ShardBusy { .. }
        | LogError::Unavailable => Status::unavailable(e.to_string()),
        LogError::KeyOutOfRange(_) => Status::out_of_range(e.to_string()),
        LogError::InvalidRange(_) => Status::invalid_argument(e.to_string()),
        LogError::Io(_) => Status::internal(e.to_string()),
    }
}

#[allow(clippy::result_large_err)]
fn proto_op_to_domain(op: i32) -> Result<crate::delta::OperationType, Status> {
    match op {
        1 => Ok(crate::delta::OperationType::Create),
        2 => Ok(crate::delta::OperationType::Update),
        3 => Ok(crate::delta::OperationType::Delete),
        4 => Ok(crate::delta::OperationType::Rename),
        5 => Ok(crate::delta::OperationType::SetAttribute),
        6 => Ok(crate::delta::OperationType::Finalize),
        7 => Ok(crate::delta::OperationType::NamespaceCreate),
        8 => Ok(crate::delta::OperationType::MigrateChunkLocations),
        _ => Err(Status::invalid_argument(format!(
            "unknown operation type: {op}"
        ))),
    }
}

/// Convert a Rust `AppendChunkAndDeltaRequest` to its proto form for
/// inter-node forwarding (#111). The inverse of the
/// `append_chunk_and_delta` handler's parse — a follower gateway uses
/// this to re-issue an append against the shard leader's `LogService`.
#[must_use]
pub fn append_chunk_and_delta_request_to_proto(
    req: &crate::traits::AppendChunkAndDeltaRequest,
) -> ProtoChunkAppendReq {
    let d = &req.delta;
    ProtoChunkAppendReq {
        shard_id: Some(kiseki_proto::v1::ShardId {
            value: d.shard_id.0.to_string(),
        }),
        tenant_id: Some(kiseki_proto::v1::OrgId {
            value: d.tenant_id.0.to_string(),
        }),
        operation: domain_op_to_proto(d.operation),
        timestamp: Some(to_proto_timestamp(&d.timestamp)),
        hashed_key: d.hashed_key.to_vec(),
        chunk_refs: d
            .chunk_refs
            .iter()
            .map(|c| kiseki_proto::v1::ChunkId {
                value: c.0.to_vec(),
            })
            .collect(),
        payload: d.payload.clone(),
        has_inline_data: d.has_inline_data,
        new_chunks: req
            .new_chunks
            .iter()
            .map(|n| kiseki_proto::v1::NewChunkMeta {
                chunk_id: n.chunk_id.to_vec(),
                placement: n.placement.clone(),
                original_len: n.original_len,
            })
            .collect(),
    }
}

/// Decode a proto `AppendChunkAndDeltaRequest` into its domain form.
///
/// The inverse of [`append_chunk_and_delta_request_to_proto`], factored
/// out of the `LogService::append_chunk_and_delta` handler so the
/// ADR-047 durable [`crate::intent::FjallIntentStore`] can round-trip an
/// intent's built append through the proto wire form without depending
/// on tonic. Returns `Err(String)` (a human-readable reason) so the
/// handler can map it to `Status::invalid_argument` while non-tonic
/// callers (the intent decoder) surface it as a codec error.
///
/// # Errors
/// Returns `Err` with a description when a required field is missing or
/// a fixed-width field (`shard_id`, `tenant_id`, `hashed_key`,
/// `chunk_id`, `operation`, `timestamp`) is malformed.
pub fn proto_to_append_chunk_and_delta(
    req: ProtoChunkAppendReq,
) -> Result<crate::traits::AppendChunkAndDeltaRequest, String> {
    let shard_id = {
        let s = req.shard_id.ok_or("shard_id required")?;
        let uuid = uuid::Uuid::parse_str(&s.value).map_err(|e| format!("invalid shard_id: {e}"))?;
        ShardId(uuid)
    };
    let tenant_id = req.tenant_id.ok_or("tenant_id required")?;
    let org_id =
        uuid::Uuid::parse_str(&tenant_id.value).map_err(|e| format!("invalid tenant_id: {e}"))?;
    let operation = proto_op_to_domain_str(req.operation)?;
    let timestamp = from_proto_timestamp_str(req.timestamp)?;
    let hashed_key: [u8; 32] = req
        .hashed_key
        .try_into()
        .map_err(|_| "hashed_key must be 32 bytes".to_string())?;
    let chunk_refs: Vec<ChunkId> = req
        .chunk_refs
        .into_iter()
        .map(|c| {
            let bytes: [u8; 32] = c
                .value
                .try_into()
                .map_err(|_| "chunk_id must be 32 bytes".to_string())?;
            Ok(ChunkId(bytes))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let new_chunks: Vec<crate::raft_store::NewChunkMeta> = req
        .new_chunks
        .into_iter()
        .map(|n| {
            let chunk_id: [u8; 32] = n
                .chunk_id
                .try_into()
                .map_err(|_| "new_chunks.chunk_id must be 32 bytes".to_string())?;
            Ok(crate::raft_store::NewChunkMeta {
                chunk_id,
                placement: n.placement,
                original_len: n.original_len,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(crate::traits::AppendChunkAndDeltaRequest {
        delta: crate::traits::AppendDeltaRequest {
            shard_id,
            tenant_id: OrgId(org_id),
            operation,
            timestamp,
            hashed_key,
            chunk_refs,
            payload: req.payload,
            has_inline_data: req.has_inline_data,
        },
        new_chunks,
    })
}

/// `proto_op_to_domain` with a `String` error, for the tonic-free
/// [`proto_to_append_chunk_and_delta`] decode path.
fn proto_op_to_domain_str(op: i32) -> Result<crate::delta::OperationType, String> {
    match op {
        1 => Ok(crate::delta::OperationType::Create),
        2 => Ok(crate::delta::OperationType::Update),
        3 => Ok(crate::delta::OperationType::Delete),
        4 => Ok(crate::delta::OperationType::Rename),
        5 => Ok(crate::delta::OperationType::SetAttribute),
        6 => Ok(crate::delta::OperationType::Finalize),
        7 => Ok(crate::delta::OperationType::NamespaceCreate),
        8 => Ok(crate::delta::OperationType::MigrateChunkLocations),
        _ => Err(format!("unknown operation type: {op}")),
    }
}

/// `from_proto_timestamp` with a `String` error, for the tonic-free
/// [`proto_to_append_chunk_and_delta`] decode path.
fn from_proto_timestamp_str(
    ts: Option<kiseki_proto::v1::DeltaTimestamp>,
) -> Result<kiseki_common::time::DeltaTimestamp, String> {
    let ts = ts.ok_or("timestamp required")?;
    let hlc = ts.hlc.ok_or("timestamp.hlc required")?;
    let wall = ts.wall.ok_or("timestamp.wall required")?;
    Ok(kiseki_common::time::DeltaTimestamp {
        hlc: kiseki_common::time::HybridLogicalClock {
            physical_ms: hlc.physical_ms,
            logical: hlc.logical,
            node_id: kiseki_common::ids::NodeId(hlc.node_id),
        },
        wall: kiseki_common::time::WallTime {
            millis_since_epoch: wall.millis_since_epoch,
            timezone: wall.timezone,
        },
        quality: match ts.quality {
            2 => kiseki_common::time::ClockQuality::Ptp,
            3 => kiseki_common::time::ClockQuality::Gps,
            4 => kiseki_common::time::ClockQuality::Unsync,
            _ => kiseki_common::time::ClockQuality::Ntp,
        },
    })
}

fn domain_op_to_proto(op: crate::delta::OperationType) -> i32 {
    match op {
        crate::delta::OperationType::Create => 1,
        crate::delta::OperationType::Update => 2,
        crate::delta::OperationType::Delete => 3,
        crate::delta::OperationType::Rename => 4,
        crate::delta::OperationType::SetAttribute => 5,
        crate::delta::OperationType::Finalize => 6,
        crate::delta::OperationType::NamespaceCreate => 7,
        crate::delta::OperationType::MigrateChunkLocations => 8,
    }
}

fn proto_shard_state(state: crate::shard::ShardState) -> i32 {
    match state {
        crate::shard::ShardState::Healthy => 1,
        crate::shard::ShardState::Election => 2,
        crate::shard::ShardState::QuorumLost => 3,
        crate::shard::ShardState::Splitting => 4,
        crate::shard::ShardState::Maintenance => 5,
        crate::shard::ShardState::Merging => 6,
        crate::shard::ShardState::Retiring => 7,
    }
}

#[allow(clippy::result_large_err)]
fn extract_shard_id(proto: Option<kiseki_proto::v1::ShardId>) -> Result<ShardId, Status> {
    let s = proto.ok_or_else(|| Status::invalid_argument("shard_id required"))?;
    let uuid = uuid::Uuid::parse_str(&s.value)
        .map_err(|e| Status::invalid_argument(format!("invalid shard_id: {e}")))?;
    Ok(ShardId(uuid))
}

fn to_proto_timestamp(
    ts: &kiseki_common::time::DeltaTimestamp,
) -> kiseki_proto::v1::DeltaTimestamp {
    kiseki_proto::v1::DeltaTimestamp {
        hlc: Some(kiseki_proto::v1::HybridLogicalClock {
            physical_ms: ts.hlc.physical_ms,
            logical: ts.hlc.logical,
            node_id: ts.hlc.node_id.0,
        }),
        wall: Some(kiseki_proto::v1::WallTime {
            millis_since_epoch: ts.wall.millis_since_epoch,
            timezone: ts.wall.timezone.clone(),
        }),
        quality: match ts.quality {
            kiseki_common::time::ClockQuality::Ntp => 1,
            kiseki_common::time::ClockQuality::Ptp => 2,
            kiseki_common::time::ClockQuality::Gps => 3,
            kiseki_common::time::ClockQuality::Unsync => 4,
        },
    }
}

#[allow(clippy::result_large_err)]
fn from_proto_timestamp(
    ts: Option<kiseki_proto::v1::DeltaTimestamp>,
) -> Result<kiseki_common::time::DeltaTimestamp, Status> {
    let ts = ts.ok_or_else(|| Status::invalid_argument("timestamp required"))?;
    let hlc = ts
        .hlc
        .ok_or_else(|| Status::invalid_argument("timestamp.hlc required"))?;
    let wall = ts
        .wall
        .ok_or_else(|| Status::invalid_argument("timestamp.wall required"))?;
    Ok(kiseki_common::time::DeltaTimestamp {
        hlc: kiseki_common::time::HybridLogicalClock {
            physical_ms: hlc.physical_ms,
            logical: hlc.logical,
            node_id: kiseki_common::ids::NodeId(hlc.node_id),
        },
        wall: kiseki_common::time::WallTime {
            millis_since_epoch: wall.millis_since_epoch,
            timezone: wall.timezone,
        },
        quality: match ts.quality {
            2 => kiseki_common::time::ClockQuality::Ptp,
            3 => kiseki_common::time::ClockQuality::Gps,
            4 => kiseki_common::time::ClockQuality::Unsync,
            _ => kiseki_common::time::ClockQuality::Ntp,
        },
    })
}

fn domain_delta_to_proto(d: &crate::delta::Delta) -> kiseki_proto::v1::Delta {
    kiseki_proto::v1::Delta {
        header: Some(kiseki_proto::v1::DeltaHeader {
            sequence: d.header.sequence.0,
            shard_id: Some(kiseki_proto::v1::ShardId {
                value: d.header.shard_id.0.to_string(),
            }),
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: d.header.tenant_id.0.to_string(),
            }),
            operation: domain_op_to_proto(d.header.operation),
            timestamp: Some(to_proto_timestamp(&d.header.timestamp)),
            hashed_key: d.header.hashed_key.to_vec(),
            tombstone: d.header.tombstone,
            chunk_refs: d
                .header
                .chunk_refs
                .iter()
                .map(|c| kiseki_proto::v1::ChunkId {
                    value: c.0.to_vec(),
                })
                .collect(),
            payload_size: d.header.payload_size,
            has_inline_data: d.header.has_inline_data,
        }),
        payload: Some(kiseki_proto::v1::DeltaPayload {
            ciphertext: d.payload.ciphertext.clone(),
            auth_tag: d.payload.auth_tag.clone(),
            nonce: d.payload.nonce.clone(),
            system_epoch: d
                .payload
                .system_epoch
                .map(|e| kiseki_proto::v1::KeyEpoch { value: e }),
            tenant_epoch: d
                .payload
                .tenant_epoch
                .map(|e| kiseki_proto::v1::KeyEpoch { value: e }),
            tenant_wrapped_material: d.payload.tenant_wrapped_material.clone(),
        }),
    }
}

/// Tonic interceptor for tenant identity validation (I-Auth1, I-T1).
///
/// When mTLS is configured, extracts the tenant `OrgId` from the client
/// certificate's OU or SPIFFE SAN and attaches it to the request
/// extensions. RPCs then verify the request's `tenant_id` matches.
///
/// Currently a no-op pass-through — returns `Ok(req)` unconditionally.
/// Wire via `tonic::service::interceptor(LogServiceServer::new(...), auth_interceptor)`.
#[allow(clippy::result_large_err)]
pub fn auth_interceptor(req: Request<()>) -> Result<Request<()>, Status> {
    // TODO: Extract OrgId from tonic::Request::peer_certs() when mTLS active.
    // For now, pass through all requests (development mode).
    Ok(req)
}
#[tonic::async_trait]
impl LogService for LogGrpc {
    async fn append_delta(
        &self,
        request: Request<ProtoAppendReq>,
    ) -> Result<Response<AppendDeltaResponse>, Status> {
        let _s = kiseki_tracing::span("LogService.AppendDelta");
        let req = request.into_inner();
        let shard_id = extract_shard_id(req.shard_id)?;
        let tenant_id = req
            .tenant_id
            .ok_or_else(|| Status::invalid_argument("tenant_id required"))?;
        let org_id = uuid::Uuid::parse_str(&tenant_id.value)
            .map_err(|e| Status::invalid_argument(format!("invalid tenant_id: {e}")))?;
        let operation = proto_op_to_domain(req.operation)?;
        let timestamp = from_proto_timestamp(req.timestamp)?;

        let hashed_key: [u8; 32] = req
            .hashed_key
            .try_into()
            .map_err(|_| Status::invalid_argument("hashed_key must be 32 bytes"))?;

        let chunk_refs: Vec<ChunkId> = req
            .chunk_refs
            .into_iter()
            .map(|c| {
                let bytes: [u8; 32] = c
                    .value
                    .try_into()
                    .map_err(|_| "chunk_id must be 32 bytes")?;
                Ok(ChunkId(bytes))
            })
            .collect::<Result<Vec<_>, &str>>()
            .map_err(Status::invalid_argument)?;

        let domain_req = crate::traits::AppendDeltaRequest {
            shard_id,
            tenant_id: OrgId(org_id),
            operation,
            timestamp,
            hashed_key,
            chunk_refs,
            payload: req.payload,
            has_inline_data: req.has_inline_data,
        };

        let seq = self
            .ops
            .append_delta(domain_req)
            .await
            .map_err(|e| to_status(&e))?;
        Ok(Response::new(AppendDeltaResponse { sequence: seq.0 }))
    }

    // #111: leader-side commit of a forwarded `ChunkAndDelta`. The
    // follower gateway re-issues its already-built append here; this
    // node (the shard leader) runs `append_chunk_and_delta` locally,
    // which `client_write`s and replicates to all members.
    async fn append_chunk_and_delta(
        &self,
        request: Request<ProtoChunkAppendReq>,
    ) -> Result<Response<AppendDeltaResponse>, Status> {
        let _s = kiseki_tracing::span("LogService.AppendChunkAndDelta");
        let req = request.into_inner();
        let domain_req = proto_to_append_chunk_and_delta(req).map_err(Status::invalid_argument)?;
        let seq = self
            .ops
            .append_chunk_and_delta(domain_req)
            .await
            .map_err(|e| to_status(&e))?;
        Ok(Response::new(AppendDeltaResponse { sequence: seq.0 }))
    }

    async fn read_deltas(
        &self,
        request: Request<ProtoReadReq>,
    ) -> Result<Response<ReadDeltasResponse>, Status> {
        let _s = kiseki_tracing::span("LogService.ReadDeltas");
        let req = request.into_inner();
        let shard_id = extract_shard_id(req.shard_id)?;

        let domain_req = crate::traits::ReadDeltasRequest {
            shard_id,
            from: SequenceNumber(req.from),
            to: SequenceNumber(req.to),
        };

        let deltas = self
            .ops
            .read_deltas(domain_req)
            .await
            .map_err(|e| to_status(&e))?;
        // Server-side cap: max 1000 deltas per response to prevent OOM.
        let capped = if deltas.len() > 1000 {
            &deltas[..1000]
        } else {
            &deltas
        };
        let proto_deltas: Vec<_> = capped.iter().map(domain_delta_to_proto).collect();

        Ok(Response::new(ReadDeltasResponse {
            deltas: proto_deltas,
        }))
    }

    async fn shard_health(
        &self,
        request: Request<ShardHealthRequest>,
    ) -> Result<Response<ShardHealthResponse>, Status> {
        let _s = kiseki_tracing::span("LogService.ShardHealth");
        let req = request.into_inner();
        let shard_id = extract_shard_id(req.shard_id)?;

        let info = self
            .ops
            .shard_health(shard_id)
            .await
            .map_err(|e| to_status(&e))?;

        let proto_info = kiseki_proto::v1::ShardInfo {
            shard_id: Some(kiseki_proto::v1::ShardId {
                value: info.shard_id.0.to_string(),
            }),
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: info.tenant_id.0.to_string(),
            }),
            namespace_id: None,
            raft_members: info
                .raft_members
                .iter()
                .map(|n| kiseki_proto::v1::NodeId { value: n.0 })
                .collect(),
            leader: info.leader.map(|n| kiseki_proto::v1::NodeId { value: n.0 }),
            tip: info.tip.0,
            state: proto_shard_state(info.state),
            split_config: None,
            split_boundary: Vec::new(),
            split_new_shard: None,
        };

        Ok(Response::new(ShardHealthResponse {
            info: Some(proto_info),
        }))
    }

    async fn set_maintenance(
        &self,
        request: Request<SetMaintenanceRequest>,
    ) -> Result<Response<SetMaintenanceResponse>, Status> {
        let _s = kiseki_tracing::span("LogService.SetMaintenance");
        let req = request.into_inner();
        let shard_id = extract_shard_id(req.shard_id)?;

        self.ops
            .set_maintenance(shard_id, req.enabled)
            .await
            .map_err(|e| to_status(&e))?;

        Ok(Response::new(SetMaintenanceResponse {}))
    }
}
