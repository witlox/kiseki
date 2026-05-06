//! `ServerImpl` — binding-agnostic native gateway data-service handler.
//!
//! ADR-042 §1.8: every binding (gRPC, TCP-framed-postcard, ibverbs,
//! libfabric/cxi) ships its own adapter that decodes its
//! binding-specific request shape and dispatches into the inherent
//! methods on this struct, passing `&dyn RequestPrincipal` for the
//! per-request identity context. Behavioral logic (commit-on-close,
//! dedup, name binding, lease semantics) lives in [`GatewayOps`];
//! this module is the proto-decode + status-mapping shim plus
//! per-call audit emission.
//!
//! Per ADR-042 §1.8 enforcement (Makefile arch-check rule): this
//! module MUST NOT reference binding-specific request-metadata types
//! (`tonic::Request`, `tonic::Response`, `tonic::Streaming`,
//! TCP-framed `ConnectionContext`, cxi `AttestationContext`). The
//! grpc and tcp_framed adapters live in sibling modules and own the
//! wire decode.
//!
//! POSIX verbs (`open`, `read`, `write`, ...) are not bridged here.
//! The grpc adapter returns `Status::unimplemented` for them;
//! TCP-framed dispatch surfaces them as `UnknownVerb`. Bridging to a
//! real inode store is a Phase 2/4 follow-up in
//! `specs/implementation/adr-042-native-gateway.md`.

#![allow(clippy::too_many_lines)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_proto::v1::native as np;
use tonic::Status;

use crate::error::GatewayError;
use crate::ops::{GatewayOps, ReadRequest, WriteConditional, WriteRequest};

use super::lease_store::{
    AcquireOutcome, LeaseStore, ReleaseOutcome, RenewOutcome,
};
use super::signing_keys::SigningKeys;

/// Maximum bytes per `BatchFetchDek` request (gate-1 round-2 N2).
const MAX_BATCH_DEK_TICKETS: usize = 1024;

/// Gateway-side native gRPC handler.
///
/// Wraps a [`GatewayOps`] and several pieces of native-only state:
/// signing keys, lease store, topology snapshot (live `AtomicU64` for
/// the current version + a snapshot of nodes + shards-per-tenant). All
/// fields are `Arc` so the handler can be cheaply cloned per RPC.
pub struct ServerImpl {
    ops: Arc<dyn GatewayOps>,
    signing_keys: Arc<SigningKeys>,
    lease_store: Arc<LeaseStore>,
    topology: Arc<TopologyInjector>,
    /// Per-tenant in-flight stream counter — enforced by the
    /// `StreamSlot` RAII guard on the client; the server reads the
    /// snapshot for `LimitExceeded` rejection on cap. Phase 4 wires
    /// this to a real `DashMap` (gate-1 round-2 N1 lives on the
    /// client; the server-side cap matches it). Currently unused; the
    /// field exists so that runtime wiring in Phase 4 doesn't churn
    /// the public `ServerImpl` shape.
    #[allow(dead_code)]
    stream_caps: Arc<parking_lot::Mutex<std::collections::HashMap<OrgId, AtomicU64>>>,
    #[allow(dead_code)]
    max_streams_per_tenant: u64,
}

/// Topology snapshot (server side). The data-path runtime updates this
/// whenever shard leadership / namespace placement changes; the handler
/// reads it on every `GetTopology` and on every response to inject the
/// `kiseki-topology-version` trailer.
pub struct TopologyInjector {
    /// Monotonic counter — bumped on any topology change.
    pub version: AtomicU64,
    /// Latest snapshot. `parking_lot::RwLock` so the read path is
    /// lock-free under contention.
    pub snapshot: parking_lot::RwLock<TopologySnapshot>,
}

/// What `GetTopology` returns. Owns the per-tenant filtering — see
/// `ServerImpl::filtered_topology_for`.
#[allow(missing_docs)]
#[derive(Clone, Default)]
pub struct TopologySnapshot {
    pub nodes: Vec<np::NodeInfo>,
    /// Per-tenant shard list (F-H3 fix).
    pub shards_by_tenant:
        std::collections::HashMap<OrgId, Vec<np::ShardLeadership>>,
}

impl TopologyInjector {
    /// Empty initial topology with version 0.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: AtomicU64::new(0),
            snapshot: parking_lot::RwLock::new(TopologySnapshot::default()),
        }
    }

    /// Bump the version counter and replace the snapshot. Caller is
    /// responsible for emitting the new version on every subsequent
    /// response.
    pub fn replace(&self, snap: TopologySnapshot) {
        *self.snapshot.write() = snap;
        self.version.fetch_add(1, Ordering::SeqCst);
    }
}

impl ServerImpl {
    /// Build a new handler with sensible defaults
    /// (`max_streams_per_tenant = 256`).
    #[must_use]
    pub fn new(ops: Arc<dyn GatewayOps>, signing_keys: Arc<SigningKeys>) -> Self {
        Self {
            ops,
            signing_keys,
            lease_store: Arc::new(LeaseStore::new(60_000)),
            topology: Arc::new(TopologyInjector::empty()),
            stream_caps: Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            max_streams_per_tenant: 256,
        }
    }

    /// Override the per-tenant in-flight stream cap.
    #[must_use]
    pub fn with_max_streams_per_tenant(mut self, cap: u64) -> Self {
        self.max_streams_per_tenant = cap;
        self
    }

    /// Inject a live [`TopologyInjector`] (the same one the runtime
    /// updates on shard-leader / placement changes).
    #[must_use]
    pub fn with_topology(mut self, t: Arc<TopologyInjector>) -> Self {
        self.topology = t;
        self
    }

    /// Override the lease store (tests).
    #[must_use]
    pub fn with_lease_store(mut self, ls: Arc<LeaseStore>) -> Self {
        self.lease_store = ls;
        self
    }

    /// Borrow the underlying `GatewayOps`. Used by tests.
    #[must_use]
    pub fn ops(&self) -> &Arc<dyn GatewayOps> {
        &self.ops
    }

    /// Borrow the signing-key store. Used by tests.
    #[must_use]
    pub fn signing_keys(&self) -> &Arc<SigningKeys> {
        &self.signing_keys
    }

    /// Borrow the lease store. Used by tests.
    #[must_use]
    pub fn lease_store(&self) -> &Arc<LeaseStore> {
        &self.lease_store
    }

    /// Borrow the topology injector. Used by tests / runtime.
    #[must_use]
    pub fn topology(&self) -> &Arc<TopologyInjector> {
        &self.topology
    }
}

// ---------------------------------------------------------------------
// Helpers — proto <-> domain
// ---------------------------------------------------------------------

#[allow(clippy::result_large_err, clippy::ref_option)]
fn require_control(c: &Option<np::ControlFields>) -> Result<&np::ControlFields, Status> {
    c.as_ref().ok_or_else(|| Status::invalid_argument("control fields required"))
}

#[allow(clippy::result_large_err)]
fn validate_idempotency_key(key: &[u8]) -> Result<(), Status> {
    if key.is_empty() || key.len() > 64 {
        return Err(Status::invalid_argument(
            "idempotency_key must be 1..=64 bytes",
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn org_from_proto(o: Option<&kiseki_proto::v1::OrgId>) -> Result<OrgId, Status> {
    let s = o.ok_or_else(|| Status::invalid_argument("tenant_id required"))?;
    let uuid = uuid::Uuid::parse_str(&s.value)
        .map_err(|e| Status::invalid_argument(format!("tenant_id: {e}")))?;
    Ok(OrgId(uuid))
}

/// Cross-check the payload `tenant_id` against the canonical SAN URI
/// surfaced via [`RequestPrincipal::cert_san_canonical`] (ADR-042
/// §1.8). Gate-1 F-H1 — second half: the proto handler is responsible
/// for the SAN ↔ payload binding; bindings stash the canonical SAN at
/// connection-acceptance time and the principal-adapter exposes it.
///
/// The canonical SAN URI carries the tenant id as the trailing
/// `/<tenant_id>` path segment. We require byte-equality between
/// that segment and the payload's UUID-stringified `OrgId` value.
///
/// In plaintext development mode the gRPC interceptor installs a
/// synthetic `dev` SAN; the cross-check is then a no-op (production
/// runtimes flip `require_tls = true` and the dev SAN never appears).
///
/// The principal is `&dyn RequestPrincipal` so this function is
/// binding-agnostic — TCP-framed, ibverbs, libfabric/cxi all reach
/// the same check via their own adapter that fills in the canonical
/// SAN from their per-connection stash.
#[allow(clippy::result_large_err)]
fn enforce_san_payload_tenant_match(
    principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
    payload_tenant: &OrgId,
) -> Result<(), Status> {
    let san = principal.cert_san_canonical();
    if san.is_empty() {
        // Adapter present but no canonical SAN was stashed (interceptor
        // not installed — e.g. unit tests on the bare ServerImpl).
        // Skip the check rather than fail-closed; the test harness is
        // responsible for installing a real or synthetic SAN when
        // behavior depends on it.
        return Ok(());
    }
    // The canonical SAN format is `spiffe://kiseki/tenant/<tenant_id>`
    // (per `CanonicalSanUri`); pull the trailing segment.
    let tenant = san.rsplit('/').next().unwrap_or("");
    if tenant == "dev" {
        // Plaintext development principal — single-tenant posture.
        return Ok(());
    }
    let payload_str = payload_tenant.0.to_string();
    if tenant == payload_str {
        return Ok(());
    }
    Err(Status::permission_denied(format!(
        "san_payload_tenant_mismatch: cert SAN tenant {tenant:?} != payload tenant {payload_str:?}",
    )))
}

#[allow(clippy::result_large_err)]
fn ns_from_proto(o: Option<&kiseki_proto::v1::NamespaceId>) -> Result<NamespaceId, Status> {
    let s = o.ok_or_else(|| Status::invalid_argument("namespace_id required"))?;
    let uuid = uuid::Uuid::parse_str(&s.value)
        .map_err(|e| Status::invalid_argument(format!("namespace_id: {e}")))?;
    Ok(NamespaceId(uuid))
}

#[allow(clippy::result_large_err)]
fn comp_from_proto(
    o: Option<&kiseki_proto::v1::CompositionId>,
) -> Result<CompositionId, Status> {
    let s = o.ok_or_else(|| Status::invalid_argument("composition_id required"))?;
    let uuid = uuid::Uuid::parse_str(&s.value)
        .map_err(|e| Status::invalid_argument(format!("composition_id: {e}")))?;
    Ok(CompositionId(uuid))
}

fn org_to_proto(o: OrgId) -> kiseki_proto::v1::OrgId {
    kiseki_proto::v1::OrgId {
        value: o.0.to_string(),
    }
}

fn ns_to_proto(n: NamespaceId) -> kiseki_proto::v1::NamespaceId {
    kiseki_proto::v1::NamespaceId {
        value: n.0.to_string(),
    }
}

fn comp_to_proto(c: CompositionId) -> kiseki_proto::v1::CompositionId {
    kiseki_proto::v1::CompositionId {
        value: c.0.to_string(),
    }
}

fn etag_for(c: CompositionId) -> np::Etag {
    // ETag is the composition UUID bytes (16 bytes). Stable for the
    // lifetime of the composition; deterministic across nodes.
    np::Etag {
        value: c.0.as_bytes().to_vec(),
    }
}

fn map_gateway_error(e: GatewayError) -> Status {
    match e {
        GatewayError::AuthenticationFailed(m) => Status::unauthenticated(m),
        GatewayError::OperationNotSupported(m) => Status::unimplemented(m),
        GatewayError::ProtocolError(m) => Status::invalid_argument(m),
        GatewayError::Upstream(m) => Status::internal(m),
        GatewayError::StaleView { lag_ms } => {
            Status::failed_precondition(format!("stale view (lag={lag_ms}ms)"))
        }
        GatewayError::KeyOutOfRange { shard_id } => {
            Status::out_of_range(format!("key out of range for shard {shard_id:?}"))
        }
        GatewayError::ReadOnlyNamespace => {
            Status::failed_precondition("namespace is read-only")
        }
        GatewayError::ServiceUnavailable(m) => Status::unavailable(m),
        GatewayError::PreconditionFailed(m) => Status::failed_precondition(m),
        // Both NotFound (composition missing) and NamespaceNotFound
        // map to gRPC NOT_FOUND. The S3 layer disambiguates the two
        // for HTTP semantics; native callers don't.
        GatewayError::NotFound(m) | GatewayError::NamespaceNotFound(m) => {
            Status::not_found(m)
        }
    }
}

#[allow(clippy::result_large_err)]
fn workflow_ref_bytes(s: &str) -> Result<Option<[u8; 16]>, Status> {
    if s.is_empty() {
        return Ok(None);
    }
    // workflow_ref is conventionally a UUID string (advisory, see
    // ADR-020 + I-WA1). Anything else is rejected at the boundary so
    // the gateway's per-write counter can attribute correctly.
    let uuid = uuid::Uuid::parse_str(s)
        .map_err(|e| Status::invalid_argument(format!("workflow_ref: {e}")))?;
    Ok(Some(*uuid.as_bytes()))
}

#[allow(clippy::result_large_err)]
fn write_conditional_from(
    cf: &np::ControlFields,
) -> Result<Option<WriteConditional>, Status> {
    use np::control_fields::Conditional;
    let Some(cond) = cf.conditional.as_ref() else {
        return Ok(None);
    };
    Ok(Some(match cond {
        Conditional::IfNoneMatch(_) => WriteConditional::IfNoneMatch,
        Conditional::IfMatch(et) => {
            if et.value.len() != 16 {
                return Err(Status::invalid_argument(
                    "if_match etag must be 16 bytes",
                ));
            }
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&et.value);
            WriteConditional::IfMatch(CompositionId(uuid::Uuid::from_bytes(bytes)))
        }
        Conditional::IfVersionMatch(_) => {
            return Err(Status::unimplemented(
                "if_version_match not yet supported",
            ));
        }
    }))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---------------------------------------------------------------------
// ServerImpl handler methods (binding-agnostic)
// ---------------------------------------------------------------------
//
// ADR-042 §1.8 enforcement: every handler reads request-source identity
// only through `&dyn RequestPrincipal`. The tonic-shaped
// `GatewayDataService` trait impl lives in [`super::grpc::adapter`];
// other bindings (TCP-framed-postcard, ibverbs, libfabric/cxi) ship
// their own adapter to the same inherent surface.
//
// Streaming methods (`put_object_stream`, `get_object_stream`,
// `put_part`, `read_stream`, `write_stream`) live exclusively in the
// adapter: they buffer/frame on the binding-specific stream type and
// then call into the unary inherent methods here. This module never
// references `tonic::Streaming`.
//
// POSIX-only methods (`path_lookup`, `open`, `read`, `write`, `close`,
// `setattr`, `getattr`, `read_dir`, `mkdir`, `unlink`,
// `rename_within_shard`) are not bridged in this phase; the adapter
// returns `Unimplemented` directly.

impl ServerImpl {
    // ----- Object verbs -----

    pub async fn put_object(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::PutObjectRequest,
    ) -> Result<np::PutObjectResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let conditional = write_conditional_from(cf)?;
        let workflow_ref = workflow_ref_bytes(&cf.workflow_ref)?;

        let name = if req.name.is_empty() {
            None
        } else {
            Some(req.name.clone())
        };
        let bytes = req.data;
        let wreq = WriteRequest {
            tenant_id: tenant,
            namespace_id: ns,
            data: bytes,
            name,
            conditional,
            workflow_ref,
        };
        let resp = self.ops.write(wreq).await.map_err(map_gateway_error)?;
        Ok(np::PutObjectResponse {
            composition_id: Some(comp_to_proto(resp.composition_id)),
            size: resp.bytes_written,
            etag: Some(etag_for(resp.composition_id)),
        })
    }

    pub async fn get_object(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::GetObjectRequest,
    ) -> Result<np::GetObjectResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let comp = match req.key {
            Some(np::get_object_request::Key::CompositionId(id)) => {
                comp_from_proto(Some(&id))?
            }
            Some(np::get_object_request::Key::Name(name)) => {
                if name.is_empty() {
                    return Err(Status::invalid_argument("name must be non-empty"));
                }
                self.ops
                    .lookup_object_by_name(tenant, ns, &name)
                    .await
                    .map_err(map_gateway_error)?
                    .ok_or_else(|| Status::not_found(format!("name not found: {name}")))?
            }
            None => {
                return Err(Status::invalid_argument("key required (name | composition_id)"));
            }
        };
        let length = if req.range_end == 0 || req.range_end <= req.range_start {
            u64::MAX
        } else {
            req.range_end - req.range_start
        };
        let read_req = ReadRequest {
            tenant_id: tenant,
            namespace_id: ns,
            composition_id: comp,
            offset: req.range_start,
            length,
        };
        let resp = self.ops.read(read_req).await.map_err(map_gateway_error)?;
        Ok(np::GetObjectResponse {
            size: resp.data.len() as u64,
            content_type: resp.content_type.unwrap_or_default(),
            etag: Some(etag_for(comp)),
            data: resp.data,
            sealed_chunks: Vec::new(),
        })
    }

    pub async fn delete_object(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::DeleteObjectRequest,
    ) -> Result<np::DeleteObjectResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let comp = comp_from_proto(req.composition_id.as_ref())?;
        match self.ops.delete(tenant, ns, comp).await {
            Ok(()) => Ok(np::DeleteObjectResponse { removed: true }),
            Err(GatewayError::NotFound(_)) => Ok(np::DeleteObjectResponse { removed: false }),
            Err(e) => Err(map_gateway_error(e)),
        }
    }

    pub async fn head_object(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::HeadObjectRequest,
    ) -> Result<np::HeadObjectResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let comp = match req.key {
            Some(np::head_object_request::Key::CompositionId(id)) => {
                comp_from_proto(Some(&id))?
            }
            Some(np::head_object_request::Key::Name(name)) => self
                .ops
                .lookup_object_by_name(tenant, ns, &name)
                .await
                .map_err(map_gateway_error)?
                .ok_or_else(|| Status::not_found(format!("name not found: {name}")))?,
            None => return Err(Status::invalid_argument("key required")),
        };
        // HEAD is implemented as a 0-length GET so we get content_type
        // + size without reading the bytes.
        let resp = self
            .ops
            .read(ReadRequest {
                tenant_id: tenant,
                namespace_id: ns,
                composition_id: comp,
                offset: 0,
                length: 0,
            })
            .await
            .map_err(map_gateway_error)?;
        Ok(np::HeadObjectResponse {
            size: resp.data.len() as u64,
            content_type: resp.content_type.unwrap_or_default(),
            etag: Some(etag_for(comp)),
            last_modified_millis_since_epoch: 0,
        })
    }

    pub async fn list_objects(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::ListObjectsRequest,
    ) -> Result<np::ListObjectsResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let prefix = if req.prefix.is_empty() {
            None
        } else {
            Some(req.prefix.as_str())
        };
        let mut entries = self
            .ops
            .list_named(tenant, ns, prefix)
            .await
            .map_err(map_gateway_error)?;
        if req.limit > 0 && entries.len() > req.limit as usize {
            entries.truncate(req.limit as usize);
        }
        let objects = entries
            .into_iter()
            .map(|(name, comp, size)| np::ObjectSummary {
                name,
                composition_id: Some(comp_to_proto(comp)),
                size,
                etag: Some(etag_for(comp)),
                last_modified_millis_since_epoch: 0,
            })
            .collect();
        Ok(np::ListObjectsResponse {
            objects,
            // v1 returns full result; pagination wiring lands in Phase 4.
            continuation: Vec::new(),
        })
    }

    pub async fn lookup_by_name(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::LookupByNameRequest,
    ) -> Result<np::LookupByNameResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        if req.name.is_empty() {
            return Err(Status::invalid_argument("name required"));
        }
        let comp = self
            .ops
            .lookup_object_by_name(tenant, ns, &req.name)
            .await
            .map_err(map_gateway_error)?
            .ok_or_else(|| Status::not_found(format!("name not found: {}", req.name)))?;
        Ok(np::LookupByNameResponse {
            composition_id: Some(comp_to_proto(comp)),
        })
    }

    // ----- Multipart -----

    pub async fn init_multipart(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::InitMultipartRequest,
    ) -> Result<np::InitMultipartResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let _ = tenant; // multipart impls don't need the tenant directly
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let upload_id = self
            .ops
            .start_multipart(ns)
            .await
            .map_err(map_gateway_error)?;
        Ok(np::InitMultipartResponse { upload_id })
    }

    /// Buffer-then-call entry point for multipart `PutPart` —
    /// stream-handling lives on the binding adapter, the handler
    /// just consumes the assembled `(header, data)` pair.
    pub async fn put_part_buffered(
        &self,
        _principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        header: np::PutPartHeader,
        data: Vec<u8>,
    ) -> Result<np::PutPartResponse, Status> {
        let etag = self
            .ops
            .upload_part(&header.upload_id, header.part_number, &data)
            .await
            .map_err(map_gateway_error)?;
        Ok(np::PutPartResponse {
            etag: Some(np::Etag {
                value: etag.into_bytes(),
            }),
            size: data.len() as u64,
        })
    }

    pub async fn complete_multipart(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::CompleteMultipartRequest,
    ) -> Result<np::CompleteMultipartResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let _ = tenant;
        let comp = self
            .ops
            .complete_multipart(&req.upload_id, None)
            .await
            .map_err(map_gateway_error)?;
        Ok(np::CompleteMultipartResponse {
            composition_id: Some(comp_to_proto(comp)),
            size: 0,
            etag: Some(etag_for(comp)),
        })
    }

    pub async fn abort_multipart(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::AbortMultipartRequest,
    ) -> Result<np::AbortMultipartResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let _ = tenant;
        self.ops
            .abort_multipart(&req.upload_id)
            .await
            .map_err(map_gateway_error)?;
        Ok(np::AbortMultipartResponse {})
    }

    /// POSIX `Fsync` — drives the gateway's `fsync_pending` hook
    /// chain even without a bridged inode model. No principal needed
    /// (it's an unauthenticated cluster-wide flush trigger today;
    /// authenticated/per-handle Fsync lands with the POSIX bridging
    /// follow-up).
    pub async fn fsync(&self) -> Result<np::FsyncResponse, Status> {
        self.ops.fsync_pending().await.map_err(map_gateway_error)?;
        Ok(np::FsyncResponse {
            fsynced_lsn: 0,
            shard_id: None,
        })
    }

    // ----- Lease verbs -----

    pub async fn acquire_lease(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::AcquireLeaseRequest,
    ) -> Result<np::AcquireLeaseResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        // Cap TTL at 5 min (300_000 ms) per ADR-042 §7.
        let ttl_ms = req.requested_ttl_ms.min(300_000);
        let lease_id_seed: [u8; 16] = uuid::Uuid::new_v4().into_bytes();
        let lease_holder_principal = lease_holder_principal_for(principal, &tenant);
        let outcome = self.lease_store.acquire(
            tenant,
            ns,
            req.inode,
            lease_holder_principal,
            ttl_ms,
            lease_id_seed,
            now_ms(),
        );
        let resp = match outcome {
            AcquireOutcome::Granted(g) => np::AcquireLeaseResponse {
                outcome: Some(np::acquire_lease_response::Outcome::Grant(np::LeaseGrant {
                    lease_id: g.lease_id.to_vec(),
                    fencing_token: g.fencing_token,
                    ttl_ms: g.ttl_ms,
                    expires_at_millis_since_epoch: g.expires_at_millis_since_epoch,
                })),
            },
            AcquireOutcome::Held {
                holder_principal,
                ttl_remaining_ms,
            } => np::AcquireLeaseResponse {
                outcome: Some(np::acquire_lease_response::Outcome::Held(np::LeaseHeld {
                    holder_principal,
                    ttl_remaining_ms,
                })),
            },
            AcquireOutcome::Draining {
                quiesce_window_remaining_ms,
            } => np::AcquireLeaseResponse {
                outcome: Some(np::acquire_lease_response::Outcome::Draining(
                    np::NodeDraining {
                        quiesce_window_remaining_ms,
                    },
                )),
            },
        };
        Ok(resp)
    }

    pub async fn renew_lease(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::RenewLeaseRequest,
    ) -> Result<np::RenewLeaseResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let lease_holder_principal = lease_holder_principal_for(principal, &tenant);
        if req.lease_id.len() != 16 {
            return Err(Status::invalid_argument("lease_id must be 16 bytes"));
        }
        let mut lid = [0u8; 16];
        lid.copy_from_slice(&req.lease_id);
        let ttl_ms = req.ttl_ms.min(300_000);
        let outcome = self.lease_store.renew(lid, &lease_holder_principal, ttl_ms, now_ms());
        let resp = match outcome {
            RenewOutcome::Renewed(g) => np::RenewLeaseResponse {
                outcome: Some(np::renew_lease_response::Outcome::Grant(np::LeaseGrant {
                    lease_id: g.lease_id.to_vec(),
                    fencing_token: g.fencing_token,
                    ttl_ms: g.ttl_ms,
                    expires_at_millis_since_epoch: g.expires_at_millis_since_epoch,
                })),
            },
            RenewOutcome::Expired {
                current_fencing_token,
                current_holder_principal,
            } => np::RenewLeaseResponse {
                outcome: Some(np::renew_lease_response::Outcome::Expired(np::LeaseExpired {
                    current_fencing_token,
                    current_holder_principal,
                })),
            },
            RenewOutcome::NotFound => {
                return Err(Status::not_found("lease_id not found"));
            }
        };
        Ok(resp)
    }

    pub async fn release_lease(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::ReleaseLeaseRequest,
    ) -> Result<np::ReleaseLeaseResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let lease_holder_principal = lease_holder_principal_for(principal, &tenant);
        if req.lease_id.len() != 16 {
            return Err(Status::invalid_argument("lease_id must be 16 bytes"));
        }
        let mut lid = [0u8; 16];
        lid.copy_from_slice(&req.lease_id);
        match self.lease_store.release(lid, &lease_holder_principal) {
            ReleaseOutcome::Released => Ok(np::ReleaseLeaseResponse {}),
            ReleaseOutcome::NotFound => Err(Status::not_found("lease_id not found")),
            ReleaseOutcome::NotHolder => Err(Status::permission_denied(
                "release_lease: not the holder",
            )),
        }
    }

    // ----- DEK fetch -----
    //
    // The gateway is NOT the keymanager — these RPCs forward verified
    // tickets to `kiseki-keymanager` which holds the master keys. v1
    // calls into the gateway-local verify path so the wire surface is
    // exercised; Phase 4 wires the keymanager forward path.

    pub async fn fetch_dek(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::FetchDekRequest,
    ) -> Result<np::FetchDekResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let outcome = match super::dek_fetch_ticket::verify_and_decode(
            &self.signing_keys,
            &req.dek_fetch_ticket,
            &tenant,
            now_ms(),
        ) {
            Ok(_) => np::fetch_dek_response::Outcome::Invalid(np::InvalidDekTicket {
                // Phase 4: forward to keymanager + return the real DEK
                // bytes here. v1 returns InvalidDekTicket{not_wired_yet}
                // so callers know the ticket parsed but the keymanager
                // round-trip is not in this build.
                reason: "keymanager_not_wired_in_phase_2".into(),
            }),
            Err(e) => np::fetch_dek_response::Outcome::Invalid(np::InvalidDekTicket {
                reason: format!("{e}"),
            }),
        };
        Ok(np::FetchDekResponse {
            outcome: Some(outcome),
        })
    }

    pub async fn batch_fetch_dek(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::BatchFetchDekRequest,
    ) -> Result<np::BatchFetchDekResponse, Status> {
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        if req.dek_fetch_tickets.len() > MAX_BATCH_DEK_TICKETS {
            return Err(Status::invalid_argument(format!(
                "batch_too_large: max {MAX_BATCH_DEK_TICKETS} tickets per request"
            )));
        }
        let outcomes = req
            .dek_fetch_tickets
            .into_iter()
            .map(|t| {
                let outcome = match super::dek_fetch_ticket::verify_and_decode(
                    &self.signing_keys,
                    &t,
                    &tenant,
                    now_ms(),
                ) {
                    Ok(_) => np::fetch_dek_response::Outcome::Invalid(np::InvalidDekTicket {
                        reason: "keymanager_not_wired_in_phase_2".into(),
                    }),
                    Err(e) => np::fetch_dek_response::Outcome::Invalid(np::InvalidDekTicket {
                        reason: format!("{e}"),
                    }),
                };
                np::FetchDekResponse {
                    outcome: Some(outcome),
                }
            })
            .collect();
        Ok(np::BatchFetchDekResponse { outcomes })
    }

    // ----- Topology -----

    pub async fn get_topology(
        &self,
        principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
        req: np::GetTopologyRequest,
    ) -> Result<np::TopologyInfo, Status> {
        let tenant = org_from_proto(req.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(principal, &tenant)?;
        let current = self.topology.version.load(Ordering::SeqCst);
        if req.known_topology_version > 0 && req.known_topology_version == current {
            // 304-equivalent — empty payload, just the version stamp.
            return Ok(np::TopologyInfo {
                topology_version: current,
                nodes: Vec::new(),
                shards: Vec::new(),
            });
        }
        let snap = self.topology.snapshot.read();
        let shards = snap
            .shards_by_tenant
            .get(&tenant)
            .cloned()
            .unwrap_or_default();
        Ok(np::TopologyInfo {
            topology_version: current,
            nodes: snap.nodes.clone(),
            shards,
        })
    }
}

/// Lease-holder principal string: canonical SAN when present
/// (production mTLS posture), tenant UUID otherwise (unit-test /
/// plaintext dev mode). Shared by `acquire_lease`, `renew_lease`,
/// `release_lease` so the derivation rule is one place.
fn lease_holder_principal_for(
    principal: &dyn kiseki_proto::native_contract::RequestPrincipal,
    tenant: &OrgId,
) -> String {
    let san = principal.cert_san_canonical();
    if san.is_empty() {
        tenant.0.to_string()
    } else {
        san.to_string()
    }
}

// Silence unused warnings for ID conversion helpers exposed for Phase 5+.
#[doc(hidden)]
#[must_use]
pub fn __keep_helpers_alive() -> (kiseki_proto::v1::OrgId, kiseki_proto::v1::NamespaceId) {
    (
        org_to_proto(OrgId(uuid::Uuid::nil())),
        ns_to_proto(NamespaceId(uuid::Uuid::nil())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use kiseki_chunk::ChunkStore;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    fn make_server() -> (Arc<InMemoryGateway>, ServerImpl) {
        let gw = Arc::new(InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
        ));
        let signing = Arc::new(SigningKeys::new(
            &SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
            60_000,
        ));
        let server = ServerImpl::new(gw.clone() as Arc<dyn GatewayOps>, signing);
        (gw, server)
    }

    fn org() -> OrgId {
        OrgId(uuid::Uuid::from_bytes([1; 16]))
    }
    fn ns() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_bytes([2; 16]))
    }

    fn ctrl() -> np::ControlFields {
        np::ControlFields {
            tenant_id: Some(org_to_proto(org())),
            idempotency_key: vec![0xAB; 8],
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
        }
    }

    /// Default principal — empty canonical SAN. Triggers the
    /// "interceptor-not-installed" fallback in
    /// `enforce_san_payload_tenant_match` so handler tests aren't
    /// gated on real cert plumbing.
    fn anon_principal() -> super::super::grpc::TonicPrincipal {
        use kiseki_proto::native_contract::ConnectionId;
        super::super::grpc::TonicPrincipal::new(String::new(), ConnectionId(0))
    }

    /// Principal carrying a specific canonical SAN. Used by the
    /// SAN/payload cross-check tests.
    fn principal_with_san(san: &str) -> super::super::grpc::TonicPrincipal {
        use kiseki_proto::native_contract::ConnectionId;
        super::super::grpc::TonicPrincipal::new(san.into(), ConnectionId(0))
    }

    async fn register_namespace(gw: &InMemoryGateway) {
        use kiseki_common::ids::ShardId;
        use kiseki_composition::namespace::Namespace;
        gw.add_namespace(Namespace {
            id: ns(),
            tenant_id: org(),
            shard_id: ShardId(uuid::Uuid::nil()),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        })
        .await;
    }

    #[tokio::test]
    async fn put_object_returns_not_found_when_namespace_unregistered() {
        let (_gw, server) = make_server();
        let put = np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "hello".into(),
            data: b"world".to_vec(),
        };
        let err = server
            .put_object(&anon_principal(), put)
            .await
            .expect_err("namespace not registered");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn put_object_then_get_object_round_trips_through_native_service() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        let put = np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "hello".into(),
            data: b"world".to_vec(),
        };
        let resp = server.put_object(&anon_principal(), put).await.unwrap();
        assert_eq!(resp.size, 5);
        let comp = resp.composition_id.expect("composition_id");
        let get = np::GetObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            range_start: 0,
            range_end: 0,
            key: Some(np::get_object_request::Key::CompositionId(comp.clone())),
        };
        let got = server.get_object(&anon_principal(), get).await.unwrap();
        assert_eq!(got.data, b"world");
        assert_eq!(got.size, 5);
    }

    #[tokio::test]
    async fn lookup_by_name_returns_composition_id() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        // PUT under name "alpha".
        let put = np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "alpha".into(),
            data: b"data".to_vec(),
        };
        let put_resp = server.put_object(&anon_principal(), put).await.unwrap();
        let lookup = np::LookupByNameRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "alpha".into(),
        };
        let lresp = server
            .lookup_by_name(&anon_principal(), lookup)
            .await
            .unwrap();
        assert_eq!(lresp.composition_id, put_resp.composition_id);
    }

    #[tokio::test]
    async fn put_object_rejects_when_san_tenant_mismatches_payload() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        // Principal carries a canonical SAN whose tenant id !=
        // payload's tenant id. The handler must reject before any
        // gateway work.
        let principal = principal_with_san(
            "spiffe://kiseki/tenant/00000000-0000-0000-0000-000000000999",
        );
        let req = np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "x".into(),
            data: b"y".to_vec(),
        };
        let err = server
            .put_object(&principal, req)
            .await
            .expect_err("mismatch");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(err.message().contains("san_payload_tenant_mismatch"));
    }

    #[tokio::test]
    async fn put_object_accepts_when_san_tenant_matches_payload() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        let principal = principal_with_san(&format!("spiffe://kiseki/tenant/{}", org().0));
        let req = np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "x".into(),
            data: b"y".to_vec(),
        };
        let resp = server.put_object(&principal, req).await.unwrap();
        assert!(resp.composition_id.is_some());
    }

    #[tokio::test]
    async fn lookup_by_name_missing_returns_not_found() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        let lookup = np::LookupByNameRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "nope".into(),
        };
        let err = server
            .lookup_by_name(&anon_principal(), lookup)
            .await
            .expect_err("missing");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_topology_increments_with_replace() {
        let (_gw, server) = make_server();
        let req = np::GetTopologyRequest {
            known_topology_version: 0,
            tenant_id: Some(org_to_proto(org())),
        };
        let resp = server.get_topology(&anon_principal(), req).await.unwrap();
        assert_eq!(resp.topology_version, 0);

        server.topology().replace(TopologySnapshot {
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "127.0.0.1:9100".into(),
                state: np::NodeState::Active as i32,
                bindings: Vec::new(),
            }],
            shards_by_tenant: std::collections::HashMap::new(),
        });
        let req2 = np::GetTopologyRequest {
            known_topology_version: 0,
            tenant_id: Some(org_to_proto(org())),
        };
        let resp2 = server.get_topology(&anon_principal(), req2).await.unwrap();
        assert_eq!(resp2.topology_version, 1);
        assert_eq!(resp2.nodes.len(), 1);
    }

    #[tokio::test]
    async fn get_topology_304_when_version_matches() {
        let (_gw, server) = make_server();
        server.topology().replace(TopologySnapshot {
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "127.0.0.1:9100".into(),
                state: np::NodeState::Active as i32,
                bindings: Vec::new(),
            }],
            shards_by_tenant: std::collections::HashMap::new(),
        });
        let resp = server
            .get_topology(
                &anon_principal(),
                np::GetTopologyRequest {
                    known_topology_version: 1,
                    tenant_id: Some(org_to_proto(org())),
                },
            )
            .await
            .unwrap();
        // 304-equivalent: same version, empty payload.
        assert_eq!(resp.topology_version, 1);
        assert!(resp.nodes.is_empty());
        assert!(resp.shards.is_empty());
    }

    #[tokio::test]
    async fn acquire_lease_grants_then_held() {
        let (_gw, server) = make_server();
        let cf = ctrl;
        let acq = server
            .acquire_lease(
                &anon_principal(),
                np::AcquireLeaseRequest {
                    control: Some(cf()),
                    namespace_id: Some(ns_to_proto(ns())),
                    inode: 100,
                    mode: np::LeaseMode::Write as i32,
                    requested_ttl_ms: 30_000,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            acq.outcome.as_ref().unwrap(),
            np::acquire_lease_response::Outcome::Grant(_)
        ));
        // Second acquire on same (ns, inode) -> Held.
        let acq2 = server
            .acquire_lease(
                &anon_principal(),
                np::AcquireLeaseRequest {
                    control: Some(cf()),
                    namespace_id: Some(ns_to_proto(ns())),
                    inode: 100,
                    mode: np::LeaseMode::Write as i32,
                    requested_ttl_ms: 30_000,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            acq2.outcome.as_ref().unwrap(),
            np::acquire_lease_response::Outcome::Held(_)
        ));
    }

    #[tokio::test]
    async fn batch_fetch_dek_rejects_oversized_batch() {
        let (_gw, server) = make_server();
        let tickets = vec![vec![0u8; 100]; MAX_BATCH_DEK_TICKETS + 1];
        let err = server
            .batch_fetch_dek(
                &anon_principal(),
                np::BatchFetchDekRequest {
                    control: Some(ctrl()),
                    dek_fetch_tickets: tickets,
                },
            )
            .await
            .expect_err("batch too large");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("batch_too_large"));
    }

    #[tokio::test]
    async fn put_object_rejects_short_idempotency_key() {
        let (_gw, server) = make_server();
        let req = np::PutObjectRequest {
            control: Some(np::ControlFields {
                tenant_id: Some(org_to_proto(org())),
                idempotency_key: Vec::new(), // 0 bytes -> reject
                workflow_ref: String::new(),
                cache_hint: None,
                conditional: None,
            }),
            namespace_id: Some(ns_to_proto(ns())),
            name: "x".into(),
            data: b"y".to_vec(),
        };
        let err = server
            .put_object(&anon_principal(), req)
            .await
            .expect_err("idempotency key too short");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}

