//! gRPC `ClusterChunkService` server implementation.
//!
//! Phase 16a step 5. Receives `PutFragment` / `GetFragment` /
//! `DeleteFragment` / `HasFragment` from peer leaders and delegates
//! to a *local* [`AsyncChunkOps`] (typically the `SyncBridge`-wrapped
//! `ChunkStore` of this node — never the [`ClusteredChunkStore`],
//! which would recurse fan-out into infinity).
//!
//! The SAN-role check ([`crate::auth::verify_fabric_san`]) lives at
//! the gRPC interceptor seam ([`fabric_san_interceptor`]) so it
//! runs before *any* method here is invoked. A leaked tenant cert
//! cannot reach this code path.
//!
//! Replication-N only for 16a — every peer holds the whole envelope
//! at `fragment_index = 0`. EC fragment distribution lands in 16b.

use std::sync::Arc;

use kiseki_chunk::{AsyncChunkOps, ChunkError};
use kiseki_common::ids::{ChunkId as RustChunkId, OrgId as RustOrgId};
use kiseki_crypto::envelope::Envelope as RustEnvelope;
use kiseki_proto::v1 as pb;
use kiseki_proto::v1::cluster_chunk_service_server::{
    ClusterChunkService, ClusterChunkServiceServer,
};
use tonic::{Request, Response, Status};

use crate::auth::{verify_fabric_san, FabricAuthError};
use kiseki_common::locks::LockOrDie;

/// Test-only knobs surfaced via admin endpoints (see
/// `kiseki-server::admin`). Process-global because the chunk-cluster
/// server is per-process. Keep them OFF in production — admin
/// endpoints that toggle them are gated behind a debug build / env
/// flag at the runtime layer.
pub(crate) static FABRIC_SLOW_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static FABRIC_DENY_INCOMING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set the per-RPC slow-down for incoming `PutFragment` calls.
/// `ms = 0` disables. Test-only — the admin endpoint that toggles
/// this is gated by `KISEKI_ENABLE_TEST_KNOBS`.
pub fn set_fabric_slow_ms(ms: u64) {
    FABRIC_SLOW_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
}

/// Toggle whether incoming `PutFragment` is refused with
/// `Unavailable`. Test-only.
pub fn set_fabric_deny_incoming(deny: bool) {
    FABRIC_DENY_INCOMING.store(deny, std::sync::atomic::Ordering::Relaxed);
}

/// gRPC handler wrapping a *local* async chunk store.
pub struct ClusterChunkServer {
    local: Arc<dyn AsyncChunkOps>,
    /// Default pool used when a request omits the optional `pool_id`.
    /// 16a ships a single pool per node; 16b's defaults table makes
    /// this per-tenant.
    default_pool: String,
    /// ADR-025 W4 — per-shard maintenance flag store. When set,
    /// `PutFragment` returns `FailedPrecondition` so operators can
    /// drain in-flight work before reconfiguration. `None` (default)
    /// means no maintenance gating (back-compat for callers that
    /// haven't wired the W4 flag).
    maintenance: Option<(
        Arc<crate::maintenance::MaintenanceMode>,
        kiseki_common::ids::ShardId,
    )>,
    /// Per-chunk envelope crypto fields (`auth_tag`, `nonce`, epochs,
    /// optional tenant-wrapped material). EC fragments persist only
    /// the ciphertext slice — the chunk-level crypto fields would
    /// otherwise be lost across the fabric, and the read path
    /// reconstructs ciphertext from EC shards but can't validate AEAD
    /// without the original tag/nonce. Every `PutFragment` for the
    /// same chunk carries identical crypto fields so writing them N
    /// times is idempotent. Discovered 2026-05-02 — without this, a
    /// 6-node EC 4+2 read returns "AEAD authentication failed" even
    /// though every fragment fetch succeeds.
    ///
    /// The map is wrapped in an `Arc` so the leader's client-side
    /// (`ClusteredChunkStore`) can deposit crypto for chunks it
    /// wrote locally without going through the `PutFragment` RPC.
    /// See `record_local_envelope_crypto` and the GCP 2026-05-02
    /// "1 of 6 readers fails AEAD" finding.
    chunk_envelope_meta: ChunkEnvelopeRegistry,
}

/// Shared handle to the per-chunk envelope crypto side table. Cloning
/// this is cheap (single `Arc` bump). Hand the same registry to the
/// `ClusterChunkServer` (server side, populated by `PutFragment`) and
/// to the `ClusteredChunkStore` (client side, populated by the leader's
/// own local-write path) so reads always see consistent crypto fields
/// regardless of which node wrote the fragment.
#[derive(Clone, Default)]
pub struct ChunkEnvelopeRegistry {
    inner: Arc<std::sync::Mutex<std::collections::HashMap<RustChunkId, EnvelopeMeta>>>,
}

impl ChunkEnvelopeRegistry {
    /// Insert envelope crypto for `chunk_id` if not already present.
    /// First write wins — every fragment of the same chunk carries
    /// identical crypto, so re-recording is a no-op.
    pub fn record(
        &self,
        chunk_id: RustChunkId,
        auth_tag: [u8; 16],
        nonce: [u8; 12],
        system_epoch: kiseki_common::tenancy::KeyEpoch,
        tenant_epoch: Option<kiseki_common::tenancy::KeyEpoch>,
        tenant_wrapped_material: Option<Vec<u8>>,
    ) {
        let mut map = self
            .inner
            .lock()
            .lock_or_die("server.inner");
        map.entry(chunk_id).or_insert(EnvelopeMeta {
            auth_tag,
            nonce,
            system_epoch,
            tenant_epoch,
            tenant_wrapped_material,
        });
    }

    fn lookup(&self, chunk_id: &RustChunkId) -> Option<EnvelopeMeta> {
        self.inner
            .lock()
            .lock_or_die("server.inner")
            .get(chunk_id)
            .cloned()
    }
}

#[derive(Clone)]
pub(crate) struct EnvelopeMeta {
    auth_tag: [u8; 16],
    nonce: [u8; 12],
    system_epoch: kiseki_common::tenancy::KeyEpoch,
    tenant_epoch: Option<kiseki_common::tenancy::KeyEpoch>,
    tenant_wrapped_material: Option<Vec<u8>>,
}

impl ClusterChunkServer {
    /// Build a server delegating to `local`. Allocates a private
    /// envelope registry; for the production wiring where the leader's
    /// client-side needs to deposit crypto for its own local writes,
    /// use `with_envelope_registry` and pass the same handle to
    /// `ClusteredChunkStore::with_envelope_registry`.
    #[must_use]
    pub fn new(local: Arc<dyn AsyncChunkOps>, default_pool: impl Into<String>) -> Self {
        Self::with_envelope_registry(local, default_pool, ChunkEnvelopeRegistry::default())
    }

    /// Build a server with an externally-supplied envelope registry.
    /// Cloning the same `ChunkEnvelopeRegistry` and handing it to the
    /// leader's `ClusteredChunkStore` is what makes the leader's
    /// own local-fragment writes visible to peers fetching via this
    /// server (closing the GCP 2026-05-02 zero-crypto gap).
    #[must_use]
    pub fn with_envelope_registry(
        local: Arc<dyn AsyncChunkOps>,
        default_pool: impl Into<String>,
        registry: ChunkEnvelopeRegistry,
    ) -> Self {
        Self {
            local,
            default_pool: default_pool.into(),
            chunk_envelope_meta: registry,
            maintenance: None,
        }
    }

    /// Builder: attach the per-shard maintenance flag store
    /// (ADR-025 W4). `shard` is the shard id served by this
    /// node — `PutFragment` is gated on
    /// `maintenance.is_in_maintenance(shard)`. Without this
    /// builder, write gating is a no-op.
    #[must_use]
    pub fn with_maintenance(
        mut self,
        maintenance: Arc<crate::maintenance::MaintenanceMode>,
        shard: kiseki_common::ids::ShardId,
    ) -> Self {
        self.maintenance = Some((maintenance, shard));
        self
    }

    /// Borrow the envelope registry — typically to clone it and hand
    /// the clone to a `ClusteredChunkStore` so the leader records
    /// crypto for its own local fragment writes here.
    #[must_use]
    pub fn envelope_registry(&self) -> ChunkEnvelopeRegistry {
        self.chunk_envelope_meta.clone()
    }

    /// Wrap into a tonic server ready to be added to a `Router`.
    /// The returned server has **no** SAN-role interceptor — useful
    /// for plaintext / single-node test setups.
    #[must_use]
    pub fn into_tonic_server(self) -> ClusterChunkServiceServer<Self> {
        ClusterChunkServiceServer::new(self)
            .max_decoding_message_size(crate::peer::FABRIC_MAX_MESSAGE_BYTES)
    }

    /// Deposit chunk-level crypto fields for a chunk this node is
    /// going to (or just did) write a local fragment for. The leader
    /// of an EC write fans out `PutFragment` RPCs to peers — those
    /// RPCs naturally populate the registry on each peer's server.
    /// The leader's OWN fragment goes directly to the local store via
    /// `local.write_fragment`, bypassing the RPC. Without this method,
    /// the registry stays empty for chunks the leader wrote, and any
    /// peer that later fetches the leader's fragment via `get_fragment`
    /// receives an envelope with ZERO `auth_tag` / `nonce` / epochs.
    /// Readers that capture crypto from that response then fail
    /// AES-GCM verify with "AEAD authentication failed" — the GCP
    /// 2026-05-02 finding.
    pub fn record_local_envelope_crypto(
        &self,
        chunk_id: RustChunkId,
        auth_tag: [u8; 16],
        nonce: [u8; 12],
        system_epoch: kiseki_common::tenancy::KeyEpoch,
        tenant_epoch: Option<kiseki_common::tenancy::KeyEpoch>,
        tenant_wrapped_material: Option<Vec<u8>>,
    ) {
        self.chunk_envelope_meta.record(
            chunk_id,
            auth_tag,
            nonce,
            system_epoch,
            tenant_epoch,
            tenant_wrapped_material,
        );
    }

    /// Build an `Envelope` for a fragment-shard read by combining the
    /// raw shard bytes with the chunk-level crypto fields the server
    /// captured on the corresponding `PutFragment`. Falls back to a
    /// zeroed envelope only when the chunk's metadata isn't in the
    /// side table (older Replication-N writes that hit the legacy
    /// path before the side-table existed) — in that case AEAD
    /// validation on the caller will fail loudly, which is the right
    /// signal.
    fn envelope_from_bytes(&self, chunk_id: RustChunkId, bytes: Vec<u8>) -> RustEnvelope {
        let meta = self.chunk_envelope_meta.lookup(&chunk_id);
        if let Some(m) = meta {
            RustEnvelope {
                chunk_id,
                ciphertext: bytes,
                auth_tag: m.auth_tag,
                nonce: m.nonce,
                system_epoch: m.system_epoch,
                tenant_epoch: m.tenant_epoch,
                tenant_wrapped_material: m.tenant_wrapped_material,
            }
        } else {
            RustEnvelope {
                chunk_id,
                ciphertext: bytes,
                auth_tag: [0u8; 16],
                nonce: [0u8; 12],
                system_epoch: kiseki_common::tenancy::KeyEpoch(1),
                tenant_epoch: None,
                tenant_wrapped_material: None,
            }
        }
    }

    /// Wrap into a tonic server with the [`fabric_san_interceptor`]
    /// pre-applied. This is the production path: `ClusterChunkService`
    /// shares the data-path gRPC port with services like `LogService`
    /// that *do* accept tenant certs, so per-method gating on the
    /// fabric service is mandatory or a leaked tenant cert gains
    /// fragment access (Phase 16a I-Auth4 / I-T1).
    #[must_use]
    pub fn into_tonic_server_with_san_check(self) -> InterceptedClusterChunkService {
        // Configure size limits BEFORE wrapping with the interceptor —
        // tonic's `InterceptedService` doesn't expose
        // `max_decoding_message_size` on its outer wrapper.
        let server = ClusterChunkServiceServer::new(self)
            .max_decoding_message_size(crate::peer::FABRIC_MAX_MESSAGE_BYTES);
        tonic::service::interceptor::InterceptedService::new(server, fabric_san_interceptor)
    }
}

/// Concrete return type of [`ClusterChunkServer::into_tonic_server_with_san_check`].
/// The function-pointer-typed interceptor lets the runtime branch
/// between intercepted (mTLS, production) and non-intercepted
/// (plaintext development) builds at the same site without leaking a
/// generic interceptor type into the runtime crate.
pub type InterceptedClusterChunkService = tonic::service::interceptor::InterceptedService<
    ClusterChunkServiceServer<ClusterChunkServer>,
    fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
>;

#[tonic::async_trait]
impl ClusterChunkService for ClusterChunkServer {
    async fn put_fragment(
        &self,
        request: Request<pb::PutFragmentRequest>,
    ) -> Result<Response<pb::PutFragmentResponse>, Status> {
        // Test-only slow-down knob. When the runtime atomic is set
        // (via admin POST `/admin/fabric/slow-ms/{ms}`), sleep that
        // many ms before accepting the fragment. Lets BDD scenarios
        // deterministically induce "Raft applies faster than fabric
        // acks" (the D-10 cross-stream ordering test in
        // `multi-node-raft.feature`) without fragile timing or
        // platform-specific iptables. Atomic so toggling is per-step.
        let slow_ms = FABRIC_SLOW_MS.load(std::sync::atomic::Ordering::Relaxed);
        if slow_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(slow_ms)).await;
        }
        // Test-only deny knob — when set, every `PutFragment` is
        // refused with `Unavailable` (mirroring "fabric port is
        // blocked but Raft is alive"). Lets BDD scenarios isolate
        // fabric-quorum failure from Raft-quorum failure (the D-5
        // 3-node Replication-3 scenario in `multi-node-raft.feature`).
        if FABRIC_DENY_INCOMING.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Status::unavailable(
                "fabric incoming disabled (test-only knob)",
            ));
        }
        // ADR-025 W4 — per-shard maintenance mode. When set,
        // reject writes with `FailedPrecondition` so operators can
        // drain the data path before reconfiguration. Reads are
        // unaffected (they walk the same `local` ops directly).
        if let Some((m, shard)) = self.maintenance.as_ref() {
            if m.is_in_maintenance(*shard) {
                return Err(Status::failed_precondition(
                    "shard is in maintenance mode (StorageAdminService.SetShardMaintenance)",
                ));
            }
        }
        let req = request.into_inner();

        let envelope = req
            .envelope
            .ok_or_else(|| Status::invalid_argument("envelope missing"))?;
        let envelope = proto_envelope_to_rust(envelope)
            .map_err(|e| Status::invalid_argument(format!("bad envelope: {e}")))?;

        let pool = req
            .pool_id
            .as_ref()
            .map_or_else(|| self.default_pool.clone(), proto_pool_to_string);

        // Capture chunk-level crypto fields before envelope is moved
        // into the storage path. Idempotent across fragments — every
        // fragment of the same chunk carries identical metadata.
        let chunk_id = envelope.chunk_id;
        self.chunk_envelope_meta.record(
            chunk_id,
            envelope.auth_tag,
            envelope.nonce,
            envelope.system_epoch,
            envelope.tenant_epoch,
            envelope.tenant_wrapped_material.clone(),
        );

        // Phase 16d step 2: route by fragment_index. index=0 keeps
        // the legacy whole-envelope path (Replication-N + dedup).
        // index>0 is an EC shard; store via write_fragment so the
        // bytes are addressed by (chunk_id, fragment_index).
        if req.fragment_index == 0 {
            let stored = self
                .local
                .write_chunk(envelope, &pool)
                .await
                .map_err(|e| chunk_err_to_status(&e))?;
            Ok(Response::new(pb::PutFragmentResponse { stored }))
        } else {
            self.local
                .write_fragment(&chunk_id, req.fragment_index, envelope.ciphertext)
                .await
                .map_err(|e| chunk_err_to_status(&e))?;
            // EC fragment writes don't carry refcount semantics; report
            // stored=true so callers can count this as a successful ack.
            Ok(Response::new(pb::PutFragmentResponse { stored: true }))
        }
    }

    async fn get_fragment(
        &self,
        request: Request<pb::GetFragmentRequest>,
    ) -> Result<Response<pb::GetFragmentResponse>, Status> {
        let req = request.into_inner();
        let chunk_id = proto_chunk_id_to_rust(req.chunk_id.as_ref())?;

        if req.fragment_index == 0 {
            // index=0 has two storage shapes depending on the mode the
            // leader used: Replication-N stores the WHOLE envelope at
            // chunk_id (via write_chunk), EC stores ONE shard at
            // (chunk_id, 0) (via write_fragment). The server doesn't
            // know which mode the leader used, so try the fragment
            // path first and fall back to the whole-envelope path. If
            // the fragment exists, return it as a synthetic envelope
            // (the EC decoder reads only `ciphertext`; the auth_tag /
            // nonce / epoch fields are leader-side state and irrelevant
            // for fragment reconstruction).
            //
            // Discovered 2026-05-02 — the prior unconditional
            // read_chunk path returned NotFound for EC chunks (leader
            // never wrote a whole envelope under EC) which surfaced
            // as `chunk lost: insufficient fragments for reconstruction`
            // on cross-node reads.
            if let Ok(bytes) = self.local.read_fragment(&chunk_id, 0).await {
                let env = self.envelope_from_bytes(chunk_id, bytes);
                return Ok(Response::new(pb::GetFragmentResponse {
                    envelope: Some(rust_envelope_to_proto(&env)),
                }));
            }
            let env = self
                .local
                .read_chunk(&chunk_id)
                .await
                .map_err(|e| chunk_err_to_status(&e))?;
            Ok(Response::new(pb::GetFragmentResponse {
                envelope: Some(rust_envelope_to_proto(&env)),
            }))
        } else {
            let bytes = self
                .local
                .read_fragment(&chunk_id, req.fragment_index)
                .await
                .map_err(|e| chunk_err_to_status(&e))?;
            let env = self.envelope_from_bytes(chunk_id, bytes);
            Ok(Response::new(pb::GetFragmentResponse {
                envelope: Some(rust_envelope_to_proto(&env)),
            }))
        }
    }

    async fn delete_fragment(
        &self,
        request: Request<pb::DeleteFragmentRequest>,
    ) -> Result<Response<pb::DeleteFragmentResponse>, Status> {
        let req = request.into_inner();
        let chunk_id = proto_chunk_id_to_rust(req.chunk_id.as_ref())?;

        if req.fragment_index == 0 {
            // Whole-envelope path: same as 16a — drop refcount,
            // report deleted=true on a 0-transition.
            match self.local.decrement_refcount(&chunk_id).await {
                Ok(0) => Ok(Response::new(pb::DeleteFragmentResponse { deleted: true })),
                Ok(_) => Ok(Response::new(pb::DeleteFragmentResponse { deleted: false })),
                Err(ChunkError::NotFound(_)) => {
                    Ok(Response::new(pb::DeleteFragmentResponse { deleted: false }))
                }
                Err(e) => Err(chunk_err_to_status(&e)),
            }
        } else {
            // EC fragment: idempotent delete via the per-fragment
            // store. No refcount semantics for individual fragments.
            let was_present = self
                .local
                .delete_fragment(&chunk_id, req.fragment_index)
                .await
                .map_err(|e| chunk_err_to_status(&e))?;
            Ok(Response::new(pb::DeleteFragmentResponse {
                deleted: was_present,
            }))
        }
    }

    async fn has_fragment(
        &self,
        request: Request<pb::HasFragmentRequest>,
    ) -> Result<Response<pb::HasFragmentResponse>, Status> {
        let req = request.into_inner();
        let chunk_id = proto_chunk_id_to_rust(req.chunk_id.as_ref())?;

        let present = if req.fragment_index == 0 {
            match self.local.refcount(&chunk_id).await {
                Ok(rc) => rc > 0,
                Err(ChunkError::NotFound(_)) => false,
                Err(e) => return Err(chunk_err_to_status(&e)),
            }
        } else {
            self.local
                .list_fragments(&chunk_id)
                .await
                .contains(&req.fragment_index)
        };
        Ok(Response::new(pb::HasFragmentResponse {
            present,
            stored_age_ms: 0,
        }))
    }
}

/// Tonic interceptor that enforces the `kiseki-fabric/<node-id>` SAN
/// role on every incoming request (D-1).
///
/// This relies on the TLS layer storing the peer cert chain in the
/// request extensions as `tonic::transport::server::TlsConnectInfo`.
/// The interceptor extracts the leaf cert DER and runs
/// [`verify_fabric_san`].
///
/// In step 7 the runtime wires this with `Server::tls_config(...)`
/// + `Router::add_service(server.with_interceptor(...))`.
pub fn fabric_san_interceptor(req: Request<()>) -> Result<Request<()>, Status> {
    let extensions = req.extensions();
    let info = extensions
        .get::<tonic::transport::server::TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>()
        .ok_or_else(|| Status::permission_denied("TLS client info missing — not on fabric port"))?;

    let certs = info
        .peer_certs()
        .ok_or_else(|| Status::permission_denied("client certificate required"))?;
    let leaf = certs
        .first()
        .ok_or_else(|| Status::permission_denied("client cert chain empty"))?;

    match verify_fabric_san(leaf.as_ref()) {
        Ok(_node_id) => Ok(req),
        Err(FabricAuthError::NotFabricRole | FabricAuthError::MissingSan) => {
            Err(Status::permission_denied("not a fabric-role certificate"))
        }
        Err(e) => Err(Status::permission_denied(format!(
            "fabric SAN check failed: {e}"
        ))),
    }
}

// -- proto ↔ rust converters --------------------------------------------------

fn proto_chunk_id_to_rust(p: Option<&pb::ChunkId>) -> Result<RustChunkId, Status> {
    let bytes = p
        .map(|c| c.value.as_slice())
        .ok_or_else(|| Status::invalid_argument("chunk_id missing"))?;
    if bytes.len() != 32 {
        return Err(Status::invalid_argument("chunk_id must be 32 bytes"));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes);
    Ok(RustChunkId(arr))
}

fn proto_pool_to_string(p: &pb::AffinityPoolId) -> String {
    String::from_utf8_lossy(&p.value).into_owned()
}

#[derive(Debug, thiserror::Error)]
enum EnvelopeConvError {
    #[error("auth_tag must be 16 bytes")]
    BadAuthTag,
    #[error("nonce must be 12 bytes")]
    BadNonce,
    #[error("chunk_id must be 32 bytes")]
    BadChunkId,
    #[error("system_epoch missing")]
    MissingSystemEpoch,
    #[error("chunk_id missing")]
    MissingChunkId,
}

fn proto_envelope_to_rust(p: pb::Envelope) -> Result<RustEnvelope, EnvelopeConvError> {
    let mut auth_tag = [0u8; 16];
    if p.auth_tag.len() != 16 {
        return Err(EnvelopeConvError::BadAuthTag);
    }
    auth_tag.copy_from_slice(&p.auth_tag);

    let mut nonce = [0u8; 12];
    if p.nonce.len() != 12 {
        return Err(EnvelopeConvError::BadNonce);
    }
    nonce.copy_from_slice(&p.nonce);

    let chunk_id_proto = p.chunk_id.ok_or(EnvelopeConvError::MissingChunkId)?;
    if chunk_id_proto.value.len() != 32 {
        return Err(EnvelopeConvError::BadChunkId);
    }
    let mut cid = [0u8; 32];
    cid.copy_from_slice(&chunk_id_proto.value);

    let system_epoch = p
        .system_epoch
        .ok_or(EnvelopeConvError::MissingSystemEpoch)?;
    let tenant_epoch = p
        .tenant_epoch
        .map(|e| kiseki_common::tenancy::KeyEpoch(e.value));

    let tenant_wrapped_material = if p.tenant_wrapped_material.is_empty() {
        None
    } else {
        Some(p.tenant_wrapped_material)
    };

    Ok(RustEnvelope {
        ciphertext: p.ciphertext,
        auth_tag,
        nonce,
        system_epoch: kiseki_common::tenancy::KeyEpoch(system_epoch.value),
        tenant_epoch,
        tenant_wrapped_material,
        chunk_id: RustChunkId(cid),
    })
}

fn rust_envelope_to_proto(e: &RustEnvelope) -> pb::Envelope {
    pb::Envelope {
        ciphertext: e.ciphertext.clone(),
        auth_tag: e.auth_tag.to_vec(),
        nonce: e.nonce.to_vec(),
        // 16a only ships AES-256-GCM (matching kiseki-crypto). Use
        // the proto's AES_256_GCM enum value.
        algorithm: pb::EncryptionAlgorithm::Aes256Gcm as i32,
        system_epoch: Some(pb::KeyEpoch {
            value: e.system_epoch.0,
        }),
        tenant_epoch: e.tenant_epoch.map(|k| pb::KeyEpoch { value: k.0 }),
        tenant_wrapped_material: e.tenant_wrapped_material.clone().unwrap_or_default(),
        chunk_id: Some(pb::ChunkId {
            value: e.chunk_id.0.to_vec(),
        }),
    }
}

#[allow(dead_code)]
fn rust_org_to_proto(o: RustOrgId) -> pb::OrgId {
    pb::OrgId {
        value: o.0.to_string(),
    }
}

fn chunk_err_to_status(e: &ChunkError) -> Status {
    let msg = e.to_string();
    match e {
        ChunkError::NotFound(_) => Status::not_found(msg),
        ChunkError::Corrupted(_) | ChunkError::DeviceUnavailable(_) | ChunkError::ChunkLost => {
            Status::data_loss(msg)
        }
        ChunkError::RetentionHoldActive(_) => Status::failed_precondition(msg),
        ChunkError::RefcountUnderflow(_)
        | ChunkError::EcInvalidConfig
        | ChunkError::EcEncodeFailed => Status::internal(msg),
        ChunkError::PoolFull(_) => Status::resource_exhausted(msg),
        ChunkError::Io(_) | ChunkError::QuorumLost { .. } => Status::unavailable(msg),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
    use kiseki_chunk::store::ChunkStore;
    use kiseki_chunk::SyncBridge;
    use kiseki_common::ids::ChunkId;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_crypto::envelope::Envelope;

    use super::*;

    fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool {
            name: pool.to_owned(),
            device_class: DeviceClass::NvmeSsd,
            durability: DurabilityStrategy::Replication { copies: 1 },
            devices: vec![],
            capacity_bytes: 1 << 30,
            used_bytes: 0,
        });
        Arc::new(SyncBridge::new(store))
    }

    fn make_envelope(seed: u8) -> Envelope {
        Envelope {
            chunk_id: ChunkId([seed; 32]),
            ciphertext: vec![seed; 64],
            auth_tag: [0u8; 16],
            nonce: [0u8; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
        }
    }

    fn put_req(env: &Envelope, pool: &str) -> pb::PutFragmentRequest {
        pb::PutFragmentRequest {
            chunk_id: Some(pb::ChunkId {
                value: env.chunk_id.0.to_vec(),
            }),
            fragment_index: 0,
            tenant_id: None,
            pool_id: Some(pb::AffinityPoolId {
                value: pool.as_bytes().to_vec(),
            }),
            envelope: Some(rust_envelope_to_proto(env)),
            leader_ts: None,
        }
    }

    #[tokio::test]
    async fn put_fragment_stores_envelope_in_local_store() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(Arc::clone(&local), "p");
        let env = make_envelope(0xA1);
        let resp = server
            .put_fragment(Request::new(put_req(&env, "p")))
            .await
            .expect("put ok")
            .into_inner();
        assert!(resp.stored, "first put returns stored=true");
        // Second put on same chunk_id is a dedup hit → stored=false.
        let resp2 = server
            .put_fragment(Request::new(put_req(&env, "p")))
            .await
            .expect("put ok")
            .into_inner();
        assert!(!resp2.stored, "dedup put returns stored=false");
    }

    #[tokio::test]
    async fn get_fragment_returns_stored_envelope() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(Arc::clone(&local), "p");
        let env = make_envelope(0xB2);
        let chunk_id = env.chunk_id;

        server
            .put_fragment(Request::new(put_req(&env, "p")))
            .await
            .expect("put");

        let resp = server
            .get_fragment(Request::new(pb::GetFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index: 0,
            }))
            .await
            .expect("get ok")
            .into_inner();
        let proto_env = resp.envelope.expect("envelope present");
        assert_eq!(proto_env.ciphertext, env.ciphertext);
        assert_eq!(proto_env.chunk_id.unwrap().value, chunk_id.0.to_vec());
    }

    #[tokio::test]
    async fn get_fragment_returns_not_found_for_absent_chunk() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(local, "p");
        let status = server
            .get_fragment(Request::new(pb::GetFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: vec![0u8; 32],
                }),
                fragment_index: 0,
            }))
            .await
            .expect_err("must fail");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn delete_fragment_idempotent_on_absent_chunk() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(local, "p");
        let resp = server
            .delete_fragment(Request::new(pb::DeleteFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: vec![0u8; 32],
                }),
                fragment_index: 0,
                tenant_id: None,
            }))
            .await
            .expect("delete must succeed even when absent")
            .into_inner();
        assert!(!resp.deleted, "absent chunk returns deleted=false");
    }

    #[tokio::test]
    async fn has_fragment_reports_presence_correctly() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(Arc::clone(&local), "p");
        let env = make_envelope(0xC3);
        let chunk_id = env.chunk_id;

        // Before put: not present.
        let resp = server
            .has_fragment(Request::new(pb::HasFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index: 0,
            }))
            .await
            .expect("has ok")
            .into_inner();
        assert!(!resp.present, "unstored fragment must report present=false");

        // After put: present.
        server
            .put_fragment(Request::new(put_req(&env, "p")))
            .await
            .expect("put");
        let resp2 = server
            .has_fragment(Request::new(pb::HasFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index: 0,
            }))
            .await
            .expect("has ok")
            .into_inner();
        assert!(resp2.present, "stored fragment must report present=true");
    }

    /// Phase 16d step 2: the server now accepts `fragment_index > 0`
    /// for EC-mode writes — the 16a `InvalidArgument` reject is
    /// gone. Index=0 still routes to the legacy `write_chunk` path
    /// (whole-envelope, refcounted); index>0 stores via
    /// `write_fragment` keyed by `(chunk_id, fragment_index)`.
    #[tokio::test]
    async fn put_fragment_at_nonzero_index_stores_via_write_fragment() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(Arc::clone(&local), "p");

        // We can't put_req(env, p) directly here because the
        // make_envelope ciphertext is meant for the whole-envelope
        // path. For EC fragments the body is just shard bytes.
        let env = make_envelope(0xD4);
        let chunk_id = env.chunk_id;
        let mut req = put_req(&env, "p");
        req.fragment_index = 3;

        let resp = server
            .put_fragment(Request::new(req))
            .await
            .expect("16d accepts index>0")
            .into_inner();
        assert!(resp.stored, "fragment newly stored");

        // The fragment is queryable via list_fragments on the local
        // store.
        let frags = local.list_fragments(&chunk_id).await;
        assert_eq!(frags, vec![3], "fragment_index=3 stored locally");

        // The legacy `chunks` map is untouched.
        let local_count = kiseki_chunk::AsyncChunkOps::refcount(local.as_ref(), &chunk_id).await;
        assert!(
            matches!(local_count, Err(kiseki_chunk::ChunkError::NotFound(_))),
            "EC fragment must NOT bump the whole-envelope refcount",
        );
    }

    /// Phase 16d step 2: `get_fragment` on index>0 reads from the
    /// per-fragment store; index=0 keeps reading from the legacy
    /// `chunks` map.
    #[tokio::test]
    async fn get_fragment_at_nonzero_index_reads_via_read_fragment() {
        let local = local_bridge("p");
        let server = ClusterChunkServer::new(Arc::clone(&local), "p");

        let env = make_envelope(0xE5);
        let chunk_id = env.chunk_id;
        let mut req = put_req(&env, "p");
        req.fragment_index = 2;
        server.put_fragment(Request::new(req)).await.expect("put");

        let resp = server
            .get_fragment(Request::new(pb::GetFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index: 2,
            }))
            .await
            .expect("get ok")
            .into_inner();
        let proto_env = resp.envelope.expect("envelope present");
        // The bytes returned ARE the shard body — for the test
        // envelope the shard body equals the input ciphertext.
        assert_eq!(proto_env.ciphertext, env.ciphertext);
    }

    // In-process `FabricPeer` that records every put and serves
    // gets from its own map. Distinct from the lib.rs `MockPeer`
    // to keep the bug repro self-contained inside `server.rs`.
    struct TestPeer {
        name: &'static str,
        store: std::sync::Mutex<std::collections::HashMap<(ChunkId, u32), Envelope>>,
    }

    impl TestPeer {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                store: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        }
    }

    #[tonic::async_trait]
    impl crate::FabricPeer for TestPeer {
        fn name(&self) -> &str {
            self.name
        }
        async fn put_fragment(
            &self,
            chunk_id: ChunkId,
            fragment_index: u32,
            _tenant_id: kiseki_common::ids::OrgId,
            _pool_id: String,
            envelope: Envelope,
        ) -> Result<bool, crate::peer::FabricPeerError> {
            self.store
                .lock()
                .unwrap()
                .insert((chunk_id, fragment_index), envelope);
            Ok(true)
        }
        async fn get_fragment(
            &self,
            chunk_id: ChunkId,
            fragment_index: u32,
        ) -> Result<Envelope, crate::peer::FabricPeerError> {
            self.store
                .lock()
                .unwrap()
                .get(&(chunk_id, fragment_index))
                .cloned()
                .ok_or(crate::peer::FabricPeerError::NotFound)
        }
        async fn delete_fragment(
            &self,
            _chunk_id: ChunkId,
            _fragment_index: u32,
            _tenant_id: kiseki_common::ids::OrgId,
        ) -> Result<bool, crate::peer::FabricPeerError> {
            Ok(false)
        }
        async fn has_fragment(
            &self,
            chunk_id: ChunkId,
            fragment_index: u32,
        ) -> Result<bool, crate::peer::FabricPeerError> {
            Ok(self
                .store
                .lock()
                .unwrap()
                .contains_key(&(chunk_id, fragment_index)))
        }
    }

    /// GCP 2026-05-02 regression: in a multi-node EC cluster the
    /// leader writes its own fragment locally via the chunk store,
    /// bypassing the server-side `put_fragment` RPC that would
    /// otherwise capture chunk-level crypto fields (`auth_tag`,
    /// `nonce`, epochs) into the server's envelope registry. When a
    /// peer later asks the leader's server for that fragment via
    /// `get_fragment`, the response carries ZERO crypto. The reader's
    /// `read_chunk_ec` captures crypto from the first peer response;
    /// if that response is the leader's, AES-GCM verify on the
    /// reassembled ciphertext fails with "AEAD authentication failed"
    /// even though every fragment fetch succeeded. Surfaced on the
    /// GCP 6-node EC 4+2 perf cluster as exactly 1 of 6 readers
    /// failing per chunk.
    ///
    /// This test drives the existing production-path APIs only: the
    /// leader's `ClusteredChunkStore::write_chunk` performs the real
    /// EC encode + local-fragment write, then a peer-side
    /// `ClusterChunkServer::get_fragment` (the real RPC handler) runs
    /// against the same local store. The bug manifests because the
    /// two halves don't share the envelope registry — the fix wires
    /// a shared `ChunkEnvelopeRegistry` handle into both.
    #[tokio::test]
    async fn leader_local_fragment_carries_crypto_through_get_fragment() {
        use kiseki_chunk::AsyncChunkOps;

        // 1. Shared local store — leader's client and leader's server
        //    both wrap it. In production they're separate Arcs onto
        //    the same `local_chunk_store` (see runtime.rs).
        let local = local_bridge("p");

        // 2. The shared crypto registry. The fix must wire this so a
        //    leader-local `write_fragment` deposits crypto here too;
        //    today, only the server-side `put_fragment` RPC writes
        //    to it.
        let registry = ChunkEnvelopeRegistry::default();
        let server =
            ClusterChunkServer::with_envelope_registry(Arc::clone(&local), "p", registry.clone());

        // 3. The leader's client (`ClusteredChunkStore`). Configured
        //    as `node-1` with one MockPeer (`node-2`) so EC 1+1
        //    placement spans exactly two slots. The MockPeer is the
        //    *receiver* of the leader's fan-out write — it has no
        //    role in this test's read assertion.
        let p2 = TestPeer::new("node-2");
        let cfg = crate::ClusterCfg::new(kiseki_common::ids::OrgId(uuid::Uuid::nil()), "p")
            .with_min_acks(1)
            .with_self_node_id(1)
            .with_cluster_nodes(vec![1, 2])
            .with_ec_strategy(crate::ec::EcStrategy::Ec { data: 1, parity: 1 });
        let client = crate::ClusteredChunkStore::new(
            Arc::clone(&local),
            vec![Arc::clone(&p2) as Arc<dyn crate::FabricPeer>],
            cfg,
        )
        .with_envelope_registry(registry);

        // 4. A real envelope with non-zero crypto. AES-GCM auth_tag is
        //    16 bytes, nonce is 12 bytes; both must survive the
        //    leader-local-write → peer-get_fragment round-trip.
        let chunk_id = ChunkId([0xF1; 32]);
        let envelope = Envelope {
            chunk_id,
            ciphertext: (0u8..64).collect(),
            auth_tag: [0xAAu8; 16],
            nonce: [0xBBu8; 12],
            system_epoch: KeyEpoch(7),
            tenant_epoch: Some(KeyEpoch(3)),
            tenant_wrapped_material: Some(b"wrapped-key-bytes".to_vec()),
        };

        // 5. Production write path. write_chunk → write_chunk_ec →
        //    for placement[i] == self_node_id, calls
        //    `local.write_fragment` (no crypto captured); for the
        //    other slot, fans out to `p2`.
        client
            .write_chunk(envelope.clone(), "p")
            .await
            .expect("leader write_chunk must succeed");

        // 6. Discover which fragment_index the leader holds locally.
        //    Use the same placement function the client used.
        let placement = crate::placement::pick_placement(&chunk_id, &[1u64, 2u64], 2);
        let leader_idx = placement
            .iter()
            .position(|&n| n == 1)
            .map(|i| u32::try_from(i).unwrap())
            .expect("leader (node 1) is in placement");
        // Sanity: local store actually has that fragment.
        let stored_indices = local.list_fragments(&chunk_id).await;
        assert!(
            stored_indices.contains(&leader_idx),
            "leader wrote fragment {leader_idx} locally; have {stored_indices:?}",
        );

        // 7. Production read path: simulate a peer's fabric client
        //    asking the leader's server for the leader's local
        //    fragment. The bug surfaces because the server's
        //    envelope registry was never populated for this chunk.
        let resp = server
            .get_fragment(Request::new(pb::GetFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index: leader_idx,
            }))
            .await
            .expect("get_fragment must succeed")
            .into_inner();
        let proto_env = resp.envelope.expect("envelope present");

        // 8. The crypto must round-trip. Today, without the fix
        //    wiring the registry, these assertions fail because the
        //    leader's client never deposits crypto on the leader's
        //    server registry — get_fragment falls through to the
        //    zero-fill envelope.
        assert_eq!(
            proto_env.auth_tag.as_slice(),
            envelope.auth_tag.as_slice(),
            "auth_tag must round-trip via leader's local-write path",
        );
        assert_eq!(
            proto_env.nonce.as_slice(),
            envelope.nonce.as_slice(),
            "nonce must round-trip — a zero nonce on the reader makes \
             AES-GCM verify fail with `AEAD authentication failed`",
        );
        assert_eq!(
            proto_env.system_epoch.map(|e| e.value),
            Some(envelope.system_epoch.0),
            "system_epoch must round-trip",
        );
        assert_eq!(
            proto_env.tenant_epoch.map(|e| e.value),
            envelope.tenant_epoch.map(|e| e.0),
            "tenant_epoch must round-trip",
        );
        assert_eq!(
            proto_env.tenant_wrapped_material,
            envelope.tenant_wrapped_material.clone().unwrap_or_default(),
            "tenant_wrapped_material must round-trip",
        );
    }
}
