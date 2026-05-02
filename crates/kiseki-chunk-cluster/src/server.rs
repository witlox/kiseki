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

/// gRPC handler wrapping a *local* async chunk store.
pub struct ClusterChunkServer {
    local: Arc<dyn AsyncChunkOps>,
    /// Default pool used when a request omits the optional `pool_id`.
    /// 16a ships a single pool per node; 16b's defaults table makes
    /// this per-tenant.
    default_pool: String,
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
    chunk_envelope_meta: std::sync::Mutex<std::collections::HashMap<RustChunkId, EnvelopeMeta>>,
}

#[derive(Clone)]
struct EnvelopeMeta {
    auth_tag: [u8; 16],
    nonce: [u8; 12],
    system_epoch: kiseki_common::tenancy::KeyEpoch,
    tenant_epoch: Option<kiseki_common::tenancy::KeyEpoch>,
    tenant_wrapped_material: Option<Vec<u8>>,
}

impl ClusterChunkServer {
    /// Build a server delegating to `local`.
    #[must_use]
    pub fn new(local: Arc<dyn AsyncChunkOps>, default_pool: impl Into<String>) -> Self {
        Self {
            local,
            default_pool: default_pool.into(),
            chunk_envelope_meta: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Wrap into a tonic server ready to be added to a `Router`.
    /// The returned server has **no** SAN-role interceptor — useful
    /// for plaintext / single-node test setups.
    #[must_use]
    pub fn into_tonic_server(self) -> ClusterChunkServiceServer<Self> {
        ClusterChunkServiceServer::new(self)
            .max_decoding_message_size(crate::peer::FABRIC_MAX_MESSAGE_BYTES)
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
        let meta = self
            .chunk_envelope_meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&chunk_id)
            .cloned();
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
        {
            let mut meta = self
                .chunk_envelope_meta
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            meta.entry(chunk_id).or_insert_with(|| EnvelopeMeta {
                auth_tag: envelope.auth_tag,
                nonce: envelope.nonce,
                system_epoch: envelope.system_epoch,
                tenant_epoch: envelope.tenant_epoch,
                tenant_wrapped_material: envelope.tenant_wrapped_material.clone(),
            });
        }

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
}
