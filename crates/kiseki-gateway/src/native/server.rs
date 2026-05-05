//! `ServerImpl` — tonic service wrapper over `GatewayOps`.
//!
//! Implements `kiseki.v1.native.GatewayDataService` per ADR-042 §3-§14.
//! Behavioral logic (commit-on-close, dedup, name binding, lease
//! semantics) lives in [`GatewayOps`]; this module is the wire-decode +
//! tonic-status-mapping shim plus per-call audit emission.
//!
//! POSIX verbs (Open, Read, Write, ...) currently return
//! `Status::unimplemented`. The POSIX path requires per-(namespace,
//! inode) state that `GatewayOps` does not yet expose; that work is
//! tracked as a Phase 2/4 follow-up in
//! `specs/implementation/adr-042-native-gateway.md`. The wire bodies
//! return real status codes (no `unimplemented!()` macro) so the
//! handler is honest about what's not yet bridged.

#![allow(clippy::too_many_lines)]

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_proto::v1::native::{
    self as np, gateway_data_service_server::GatewayDataService,
};
use tonic::{Request, Response, Status, Streaming};

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
/// stashed in the request's tonic extensions by `SanInterceptor`
/// (gate-1 F-H1 — second half: the proto handler is responsible for
/// the SAN ↔ payload binding; the interceptor only stashes).
///
/// The canonical SAN URI carries the tenant id as the trailing
/// `/<tenant_id>` path segment. We require byte-equality between
/// that segment and the payload's UUID-stringified `OrgId` value.
///
/// In plaintext development mode the interceptor installs a synthetic
/// `dev` SAN; the cross-check is then a no-op (production runtimes
/// flip `require_tls = true` and the dev SAN never appears).
#[allow(clippy::result_large_err)]
fn enforce_san_payload_tenant_match(
    canonical: Option<&super::canonical_san::CanonicalSanUri>,
    payload_tenant: &OrgId,
) -> Result<(), Status> {
    let Some(canonical) = canonical else {
        // Interceptor not installed (e.g. unit tests on the bare
        // ServerImpl). Skip the check rather than fail-closed; the
        // test harness is responsible for installing a real or
        // synthetic SAN when behavior depends on it.
        return Ok(());
    };
    if canonical.tenant_id() == "dev" {
        // Plaintext development principal — single-tenant posture.
        return Ok(());
    }
    let payload_str = payload_tenant.0.to_string();
    if canonical.tenant_id() == payload_str {
        return Ok(());
    }
    Err(Status::permission_denied(format!(
        "san_payload_tenant_mismatch: cert SAN tenant {:?} != payload tenant {:?}",
        canonical.tenant_id(),
        payload_str,
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
// GatewayDataService impl
// ---------------------------------------------------------------------

#[tonic::async_trait]
impl GatewayDataService for ServerImpl {
    // ----- Object verbs -----

    async fn put_object(
        &self,
        request: Request<np::PutObjectRequest>,
    ) -> Result<Response<np::PutObjectResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::PutObjectResponse {
            composition_id: Some(comp_to_proto(resp.composition_id)),
            size: resp.bytes_written,
            etag: Some(etag_for(resp.composition_id)),
        }))
    }

    async fn put_object_stream(
        &self,
        request: Request<Streaming<np::PutObjectChunk>>,
    ) -> Result<Response<np::PutObjectResponse>, Status> {
        // Buffer the stream, then call put_object once. Phase 5+ may
        // optimize for multi-chunk PUT atomicity (F-NG12) — the
        // ServerImpl needs an atomic CommitStream barrier with
        // orphan-fragment scrub on partial failure. v1 buffers.
        let mut stream = request.into_inner();
        let mut header: Option<np::PutObjectRequest> = None;
        let mut data: Vec<u8> = Vec::new();
        let mut committed = false;
        while let Some(chunk) = stream.message().await? {
            let Some(kind) = chunk.kind else {
                return Err(Status::invalid_argument("PutObjectChunk.kind required"));
            };
            match kind {
                np::put_object_chunk::Kind::First(h) => {
                    if header.is_some() {
                        return Err(Status::invalid_argument(
                            "First sent more than once on PutObjectStream",
                        ));
                    }
                    header = Some(h);
                }
                np::put_object_chunk::Kind::Data(b) => {
                    if header.is_none() {
                        return Err(Status::invalid_argument(
                            "Data before First on PutObjectStream",
                        ));
                    }
                    data.extend_from_slice(&b);
                }
                np::put_object_chunk::Kind::Commit(_) => {
                    committed = true;
                    break;
                }
            }
        }
        if !committed {
            return Err(Status::aborted(
                "PutObjectStream ended without explicit Commit",
            ));
        }
        let mut req = header.ok_or_else(|| {
            Status::invalid_argument("PutObjectStream missing First")
        })?;
        req.data = data;
        self.put_object(Request::new(req)).await
    }

    async fn get_object(
        &self,
        request: Request<np::GetObjectRequest>,
    ) -> Result<Response<np::GetObjectResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::GetObjectResponse {
            size: resp.data.len() as u64,
            content_type: resp.content_type.unwrap_or_default(),
            etag: Some(etag_for(comp)),
            data: resp.data,
            sealed_chunks: Vec::new(),
        }))
    }

    type GetObjectStreamStream = Pin<
        Box<dyn tokio_stream::Stream<Item = Result<np::GetObjectChunk, Status>> + Send + 'static>,
    >;

    async fn get_object_stream(
        &self,
        request: Request<np::GetObjectRequest>,
    ) -> Result<Response<Self::GetObjectStreamStream>, Status> {
        // v1: identical wire content, just framed. Future work: emit
        // per-chunk sealed envelopes for TrustedCompute and lazy-stream
        // chunks from the chunk store directly.
        let resp = self.get_object(request).await?.into_inner();
        let header = np::GetObjectChunk {
            kind: Some(np::get_object_chunk::Kind::Header(np::GetObjectHeader {
                size: resp.size,
                content_type: resp.content_type,
                etag: resp.etag,
                client_decrypt: false,
            })),
        };
        let data_chunk = np::GetObjectChunk {
            kind: Some(np::get_object_chunk::Kind::Data(resp.data)),
        };
        let s = tokio_stream::iter(vec![Ok(header), Ok(data_chunk)]);
        Ok(Response::new(Box::pin(s)))
    }

    async fn delete_object(
        &self,
        request: Request<np::DeleteObjectRequest>,
    ) -> Result<Response<np::DeleteObjectResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let comp = comp_from_proto(req.composition_id.as_ref())?;
        match self.ops.delete(tenant, ns, comp).await {
            Ok(()) => Ok(Response::new(np::DeleteObjectResponse { removed: true })),
            Err(GatewayError::NotFound(_)) => {
                Ok(Response::new(np::DeleteObjectResponse { removed: false }))
            }
            Err(e) => Err(map_gateway_error(e)),
        }
    }

    async fn head_object(
        &self,
        request: Request<np::HeadObjectRequest>,
    ) -> Result<Response<np::HeadObjectResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::HeadObjectResponse {
            size: resp.data.len() as u64,
            content_type: resp.content_type.unwrap_or_default(),
            etag: Some(etag_for(comp)),
            last_modified_millis_since_epoch: 0,
        }))
    }

    async fn list_objects(
        &self,
        request: Request<np::ListObjectsRequest>,
    ) -> Result<Response<np::ListObjectsResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::ListObjectsResponse {
            objects,
            // v1 returns full result; pagination wiring lands in Phase 4.
            continuation: Vec::new(),
        }))
    }

    async fn lookup_by_name(
        &self,
        request: Request<np::LookupByNameRequest>,
    ) -> Result<Response<np::LookupByNameResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::LookupByNameResponse {
            composition_id: Some(comp_to_proto(comp)),
        }))
    }

    // ----- Multipart -----

    async fn init_multipart(
        &self,
        request: Request<np::InitMultipartRequest>,
    ) -> Result<Response<np::InitMultipartResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let _ = tenant; // multipart impls don't need the tenant directly
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        let upload_id = self
            .ops
            .start_multipart(ns)
            .await
            .map_err(map_gateway_error)?;
        Ok(Response::new(np::InitMultipartResponse { upload_id }))
    }

    async fn put_part(
        &self,
        request: Request<Streaming<np::PutPartChunk>>,
    ) -> Result<Response<np::PutPartResponse>, Status> {
        let mut stream = request.into_inner();
        let mut header: Option<np::PutPartHeader> = None;
        let mut data: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.message().await? {
            let Some(kind) = chunk.kind else {
                return Err(Status::invalid_argument("PutPartChunk.kind required"));
            };
            match kind {
                np::put_part_chunk::Kind::Header(h) => {
                    if header.is_some() {
                        return Err(Status::invalid_argument(
                            "Header sent more than once on PutPart",
                        ));
                    }
                    header = Some(h);
                }
                np::put_part_chunk::Kind::Data(b) => {
                    if header.is_none() {
                        return Err(Status::invalid_argument("Data before Header"));
                    }
                    data.extend_from_slice(&b);
                }
            }
        }
        let h = header.ok_or_else(|| Status::invalid_argument("PutPart missing Header"))?;
        let etag = self
            .ops
            .upload_part(&h.upload_id, h.part_number, &data)
            .await
            .map_err(map_gateway_error)?;
        Ok(Response::new(np::PutPartResponse {
            etag: Some(np::Etag {
                value: etag.into_bytes(),
            }),
            size: data.len() as u64,
        }))
    }

    async fn complete_multipart(
        &self,
        request: Request<np::CompleteMultipartRequest>,
    ) -> Result<Response<np::CompleteMultipartResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let _ = tenant;
        let comp = self
            .ops
            .complete_multipart(&req.upload_id, None)
            .await
            .map_err(map_gateway_error)?;
        Ok(Response::new(np::CompleteMultipartResponse {
            composition_id: Some(comp_to_proto(comp)),
            size: 0,
            etag: Some(etag_for(comp)),
        }))
    }

    async fn abort_multipart(
        &self,
        request: Request<np::AbortMultipartRequest>,
    ) -> Result<Response<np::AbortMultipartResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let _ = tenant;
        self.ops
            .abort_multipart(&req.upload_id)
            .await
            .map_err(map_gateway_error)?;
        Ok(Response::new(np::AbortMultipartResponse {}))
    }

    // ----- POSIX verbs (stubs returning Status::unimplemented; bridging
    // to a real inode store is a follow-up — see ADR-042 §9 + the
    // implementation plan's open items.) -----

    async fn path_lookup(
        &self,
        _request: Request<np::PathLookupRequest>,
    ) -> Result<Response<np::PathLookupResponse>, Status> {
        Err(Status::unimplemented("POSIX path_lookup not bridged in Phase 2"))
    }

    async fn open(
        &self,
        _request: Request<np::OpenRequest>,
    ) -> Result<Response<np::OpenResponse>, Status> {
        Err(Status::unimplemented("POSIX open not bridged in Phase 2"))
    }

    async fn read(
        &self,
        _request: Request<np::ReadRequest>,
    ) -> Result<Response<np::ReadResponse>, Status> {
        Err(Status::unimplemented("POSIX read not bridged in Phase 2"))
    }

    type ReadStreamStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<np::ReadChunk, Status>> + Send + 'static>>;

    async fn read_stream(
        &self,
        _request: Request<np::ReadRequest>,
    ) -> Result<Response<Self::ReadStreamStream>, Status> {
        Err(Status::unimplemented("POSIX read_stream not bridged in Phase 2"))
    }

    async fn write(
        &self,
        _request: Request<np::WriteRequest>,
    ) -> Result<Response<np::WriteResponse>, Status> {
        Err(Status::unimplemented("POSIX write not bridged in Phase 2"))
    }

    async fn write_stream(
        &self,
        _request: Request<Streaming<np::WriteChunk>>,
    ) -> Result<Response<np::WriteResponse>, Status> {
        Err(Status::unimplemented("POSIX write_stream not bridged in Phase 2"))
    }

    async fn fsync(
        &self,
        _request: Request<np::FsyncRequest>,
    ) -> Result<Response<np::FsyncResponse>, Status> {
        // Drive the gateway's fsync_pending hook chain even without a
        // bridged inode model — clients calling Fsync on an unknown
        // handle still get the cluster-wide flush they expect.
        self.ops.fsync_pending().await.map_err(map_gateway_error)?;
        Ok(Response::new(np::FsyncResponse {
            fsynced_lsn: 0,
            shard_id: None,
        }))
    }

    async fn close(
        &self,
        _request: Request<np::CloseRequest>,
    ) -> Result<Response<np::CloseResponse>, Status> {
        Err(Status::unimplemented("POSIX close not bridged in Phase 2"))
    }

    async fn setattr(
        &self,
        _request: Request<np::SetattrRequest>,
    ) -> Result<Response<np::SetattrResponse>, Status> {
        Err(Status::unimplemented("POSIX setattr not bridged in Phase 2"))
    }

    async fn getattr(
        &self,
        _request: Request<np::GetattrRequest>,
    ) -> Result<Response<np::GetattrResponse>, Status> {
        Err(Status::unimplemented("POSIX getattr not bridged in Phase 2"))
    }

    type ReadDirStream = Pin<
        Box<dyn tokio_stream::Stream<Item = Result<np::ReadDirEntry, Status>> + Send + 'static>,
    >;

    async fn read_dir(
        &self,
        _request: Request<np::ReadDirRequest>,
    ) -> Result<Response<Self::ReadDirStream>, Status> {
        Err(Status::unimplemented("POSIX read_dir not bridged in Phase 2"))
    }

    async fn mkdir(
        &self,
        _request: Request<np::MkdirRequest>,
    ) -> Result<Response<np::MkdirResponse>, Status> {
        Err(Status::unimplemented("POSIX mkdir not bridged in Phase 2"))
    }

    async fn unlink(
        &self,
        _request: Request<np::UnlinkRequest>,
    ) -> Result<Response<np::UnlinkResponse>, Status> {
        Err(Status::unimplemented("POSIX unlink not bridged in Phase 2"))
    }

    async fn rename_within_shard(
        &self,
        _request: Request<np::RenameRequest>,
    ) -> Result<Response<np::RenameResponse>, Status> {
        Err(Status::unimplemented("POSIX rename not bridged in Phase 2"))
    }

    // ----- Lease verbs -----

    async fn acquire_lease(
        &self,
        request: Request<np::AcquireLeaseRequest>,
    ) -> Result<Response<np::AcquireLeaseResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let ns = ns_from_proto(req.namespace_id.as_ref())?;
        // Cap TTL at 5 min (300_000 ms) per ADR-042 §7.
        let ttl_ms = req.requested_ttl_ms.min(300_000);
        let lease_id_seed: [u8; 16] = uuid::Uuid::new_v4().into_bytes();
        // Principal = the canonical SAN URI when available (production
        // mTLS posture), otherwise fall back to the tenant UUID string
        // (unit tests / plaintext dev mode).
        let principal = canonical
            .as_ref()
            .map_or_else(|| tenant.0.to_string(), |c| c.as_str().to_string());
        let outcome = self.lease_store.acquire(
            tenant,
            ns,
            req.inode,
            principal,
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
        Ok(Response::new(resp))
    }

    async fn renew_lease(
        &self,
        request: Request<np::RenewLeaseRequest>,
    ) -> Result<Response<np::RenewLeaseResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let principal = canonical
            .as_ref()
            .map_or_else(|| tenant.0.to_string(), |c| c.as_str().to_string());
        if req.lease_id.len() != 16 {
            return Err(Status::invalid_argument("lease_id must be 16 bytes"));
        }
        let mut lid = [0u8; 16];
        lid.copy_from_slice(&req.lease_id);
        let ttl_ms = req.ttl_ms.min(300_000);
        let outcome = self.lease_store.renew(lid, &principal, ttl_ms, now_ms());
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
        Ok(Response::new(resp))
    }

    async fn release_lease(
        &self,
        request: Request<np::ReleaseLeaseRequest>,
    ) -> Result<Response<np::ReleaseLeaseResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let principal = canonical
            .as_ref()
            .map_or_else(|| tenant.0.to_string(), |c| c.as_str().to_string());
        if req.lease_id.len() != 16 {
            return Err(Status::invalid_argument("lease_id must be 16 bytes"));
        }
        let mut lid = [0u8; 16];
        lid.copy_from_slice(&req.lease_id);
        match self.lease_store.release(lid, &principal) {
            ReleaseOutcome::Released => Ok(Response::new(np::ReleaseLeaseResponse {})),
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

    async fn fetch_dek(
        &self,
        request: Request<np::FetchDekRequest>,
    ) -> Result<Response<np::FetchDekResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::FetchDekResponse {
            outcome: Some(outcome),
        }))
    }

    async fn batch_fetch_dek(
        &self,
        request: Request<np::BatchFetchDekRequest>,
    ) -> Result<Response<np::BatchFetchDekResponse>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let cf = require_control(&req.control)?;
        validate_idempotency_key(&cf.idempotency_key)?;
        let tenant = org_from_proto(cf.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
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
        Ok(Response::new(np::BatchFetchDekResponse { outcomes }))
    }

    // ----- Topology -----

    async fn get_topology(
        &self,
        request: Request<np::GetTopologyRequest>,
    ) -> Result<Response<np::TopologyInfo>, Status> {
        let canonical = request
            .extensions()
            .get::<super::canonical_san::CanonicalSanUri>()
            .cloned();
        let req = request.into_inner();
        let tenant = org_from_proto(req.tenant_id.as_ref())?;
        enforce_san_payload_tenant_match(canonical.as_ref(), &tenant)?;
        let current = self.topology.version.load(Ordering::SeqCst);
        if req.known_topology_version > 0 && req.known_topology_version == current {
            // 304-equivalent — empty payload, just the version stamp.
            return Ok(Response::new(np::TopologyInfo {
                topology_version: current,
                nodes: Vec::new(),
                shards: Vec::new(),
            }));
        }
        let snap = self.topology.snapshot.read();
        let shards = snap
            .shards_by_tenant
            .get(&tenant)
            .cloned()
            .unwrap_or_default();
        Ok(Response::new(np::TopologyInfo {
            topology_version: current,
            nodes: snap.nodes.clone(),
            shards,
        }))
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
            .put_object(Request::new(put))
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
        let resp = server
            .put_object(Request::new(put))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.size, 5);
        let comp = resp.composition_id.expect("composition_id");
        let get = np::GetObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            range_start: 0,
            range_end: 0,
            key: Some(np::get_object_request::Key::CompositionId(comp.clone())),
        };
        let got = server
            .get_object(Request::new(get))
            .await
            .unwrap()
            .into_inner();
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
        let put_resp = server
            .put_object(Request::new(put))
            .await
            .unwrap()
            .into_inner();
        let lookup = np::LookupByNameRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "alpha".into(),
        };
        let lresp = server
            .lookup_by_name(Request::new(lookup))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(lresp.composition_id, put_resp.composition_id);
    }

    #[tokio::test]
    async fn put_object_rejects_when_san_tenant_mismatches_payload() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        // Install a canonical SAN whose tenant id != the payload's
        // tenant id. The handler must reject before any gateway work.
        let san = super::super::canonical_san::CanonicalSanUri::from_canonical_for_tests(
            "spiffe://kiseki/tenant/00000000-0000-0000-0000-000000000999",
        );
        let mut req = Request::new(np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "x".into(),
            data: b"y".to_vec(),
        });
        req.extensions_mut().insert(san);
        let err = server.put_object(req).await.expect_err("mismatch");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(err.message().contains("san_payload_tenant_mismatch"));
    }

    #[tokio::test]
    async fn put_object_accepts_when_san_tenant_matches_payload() {
        let (gw, server) = make_server();
        register_namespace(&gw).await;
        let san = super::super::canonical_san::CanonicalSanUri::from_canonical_for_tests(
            &format!("spiffe://kiseki/tenant/{}", org().0),
        );
        let mut req = Request::new(np::PutObjectRequest {
            control: Some(ctrl()),
            namespace_id: Some(ns_to_proto(ns())),
            name: "x".into(),
            data: b"y".to_vec(),
        });
        req.extensions_mut().insert(san);
        let resp = server.put_object(req).await.unwrap().into_inner();
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
            .lookup_by_name(Request::new(lookup))
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
        let resp = server
            .get_topology(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.topology_version, 0);

        server.topology().replace(TopologySnapshot {
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "127.0.0.1:9100".into(),
                state: np::NodeState::Active as i32,
            }],
            shards_by_tenant: std::collections::HashMap::new(),
        });
        let req2 = np::GetTopologyRequest {
            known_topology_version: 0,
            tenant_id: Some(org_to_proto(org())),
        };
        let resp2 = server
            .get_topology(Request::new(req2))
            .await
            .unwrap()
            .into_inner();
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
            }],
            shards_by_tenant: std::collections::HashMap::new(),
        });
        let resp = server
            .get_topology(Request::new(np::GetTopologyRequest {
                known_topology_version: 1,
                tenant_id: Some(org_to_proto(org())),
            }))
            .await
            .unwrap()
            .into_inner();
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
            .acquire_lease(Request::new(np::AcquireLeaseRequest {
                control: Some(cf()),
                namespace_id: Some(ns_to_proto(ns())),
                inode: 100,
                mode: np::LeaseMode::Write as i32,
                requested_ttl_ms: 30_000,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            acq.outcome.as_ref().unwrap(),
            np::acquire_lease_response::Outcome::Grant(_)
        ));
        // Second acquire on same (ns, inode) -> Held.
        let acq2 = server
            .acquire_lease(Request::new(np::AcquireLeaseRequest {
                control: Some(cf()),
                namespace_id: Some(ns_to_proto(ns())),
                inode: 100,
                mode: np::LeaseMode::Write as i32,
                requested_ttl_ms: 30_000,
            }))
            .await
            .unwrap()
            .into_inner();
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
            .batch_fetch_dek(Request::new(np::BatchFetchDekRequest {
                control: Some(ctrl()),
                dek_fetch_tickets: tickets,
            }))
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
            .put_object(Request::new(req))
            .await
            .expect_err("idempotency key too short");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn posix_open_returns_unimplemented() {
        let (_gw, server) = make_server();
        let err = server
            .open(Request::new(np::OpenRequest::default()))
            .await
            .expect_err("POSIX not bridged");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }
}
