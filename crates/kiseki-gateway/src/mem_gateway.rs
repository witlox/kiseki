//! In-memory gateway — wires Composition + Chunk + Crypto for protocol gateways.
//!
//! Handles the full data path: plaintext from protocol client → encrypt →
//! chunk store → composition metadata, and reverse for reads.

use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::Arc;
use tokio::sync::Mutex;

use kiseki_chunk::store::ChunkOps;
use kiseki_common::tenancy::DedupPolicy;
use kiseki_composition::composition::{CompositionOps, CompositionStore};
use kiseki_crypto::aead::Aead;
use kiseki_crypto::chunk_id::derive_chunk_id;
use kiseki_crypto::envelope;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_view::view::{ViewOps, ViewStore};

use crate::error::GatewayError;
use crate::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

/// In-memory gateway backed by composition store, chunk store, and crypto.
///
/// Uses `tokio::sync::Mutex` for interior mutability so `GatewayOps` methods can
/// take `&self`, enabling concurrent access.
pub struct InMemoryGateway {
    compositions: Mutex<CompositionStore>,
    chunks: Mutex<Box<dyn ChunkOps + Send>>,
    aead: Aead,
    master_key: SystemMasterKey,
    dedup_policy: DedupPolicy,
    tenant_hmac_key: Option<Vec<u8>>,
    view_store: Option<Arc<std::sync::Mutex<ViewStore>>>,
    /// Total gateway requests (reads + writes).
    pub requests_total: AtomicU64,
    /// Cumulative bytes written through the gateway.
    pub bytes_written: AtomicU64,
    /// Cumulative bytes read through the gateway.
    pub bytes_read: AtomicU64,
    /// Last-written sequence per session for `ReadYourWrites` enforcement.
    /// Maps (tenant, namespace) → highest committed sequence number.
    last_written_seq: std::sync::Mutex<
        std::collections::HashMap<
            (kiseki_common::ids::OrgId, kiseki_common::ids::NamespaceId),
            kiseki_common::ids::SequenceNumber,
        >,
    >,
    /// Inline data threshold (ADR-030). Files with encrypted payload
    /// at or below this size are stored inline in the delta payload
    /// instead of as chunk extents. 0 = disable inline.
    inline_threshold: u64,
    /// Inline content store for small-file reads (ADR-030).
    small_store: Option<Arc<dyn kiseki_common::inline_store::InlineStore>>,
}

impl InMemoryGateway {
    /// Create a new in-memory gateway with the given crypto material.
    ///
    /// Uses `CrossTenant` dedup policy by default. Call
    /// `with_dedup_policy` to configure per-tenant isolation (I-X2).
    #[must_use]
    pub fn new(
        compositions: CompositionStore,
        chunks: Box<dyn ChunkOps + Send>,
        master_key: SystemMasterKey,
    ) -> Self {
        Self {
            compositions: Mutex::new(compositions),
            chunks: Mutex::new(chunks),
            aead: Aead::new(),
            master_key,
            dedup_policy: DedupPolicy::CrossTenant,
            tenant_hmac_key: None,
            view_store: None,
            requests_total: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            last_written_seq: std::sync::Mutex::new(std::collections::HashMap::new()),
            inline_threshold: 0, // disabled by default; set via with_inline_threshold
            small_store: None,
        }
    }

    /// Set the inline data threshold (ADR-030).
    ///
    /// Files with encrypted payload at or below this size are stored
    /// inline in the delta payload. Set to 0 to disable.
    #[must_use]
    pub fn with_inline_threshold(
        mut self,
        threshold: u64,
        store: Arc<dyn kiseki_common::inline_store::InlineStore>,
    ) -> Self {
        self.inline_threshold = threshold;
        self.small_store = Some(store);
        self
    }

    /// Register a namespace in the gateway's composition store.
    ///
    /// Namespaces are created by the Control Plane and must be registered
    /// with the gateway before any write/read operations can target them.
    pub async fn add_namespace(&self, ns: kiseki_composition::namespace::Namespace) {
        self.compositions.lock().await.add_namespace(ns);
    }

    /// List compositions in a namespace (for S3 `ListObjectsV2`).
    pub async fn list_compositions(
        &self,
        ns_id: kiseki_common::ids::NamespaceId,
    ) -> Vec<(kiseki_common::ids::CompositionId, u64)> {
        let compositions = self.compositions.lock().await;
        compositions
            .list_by_namespace(ns_id)
            .into_iter()
            .map(|c| (c.id, c.size))
            .collect()
    }

    /// Start a multipart upload. Returns the upload ID.
    pub async fn start_multipart_internal(
        &self,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<String, GatewayError> {
        self.compositions
            .lock()
            .await
            .start_multipart(namespace_id)
            .map_err(|e| GatewayError::Upstream(e.to_string()))
    }

    /// Upload a part: encrypt + store chunk, then register it with the upload.
    pub async fn upload_part_internal(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<kiseki_common::ids::ChunkId, GatewayError> {
        let chunk_id = kiseki_crypto::chunk_id::derive_chunk_id(
            data,
            self.dedup_policy,
            self.tenant_hmac_key.as_deref(),
        )
        .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        let env = envelope::seal_envelope(&self.aead, &self.master_key, &chunk_id, data)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        let size = data.len() as u64;

        self.chunks
            .lock()
            .await
            .write_chunk(env, "default")
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        self.compositions
            .lock()
            .await
            .upload_part(upload_id, part_number, chunk_id, size)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        Ok(chunk_id)
    }

    /// Complete a multipart upload.
    pub async fn complete_multipart_internal(
        &self,
        upload_id: &str,
    ) -> Result<kiseki_common::ids::CompositionId, GatewayError> {
        self.compositions
            .lock()
            .await
            .finalize_multipart(upload_id)
            .map_err(|e| GatewayError::Upstream(e.to_string()))
    }

    /// Abort a multipart upload.
    pub async fn abort_multipart_internal(&self, upload_id: &str) -> Result<(), GatewayError> {
        self.compositions
            .lock()
            .await
            .abort_multipart(upload_id)
            .map_err(|e| GatewayError::Upstream(e.to_string()))
    }

    /// Attach a shared view store for staleness enforcement (I-K9).
    #[must_use]
    pub fn with_view_store(mut self, vs: Arc<std::sync::Mutex<ViewStore>>) -> Self {
        self.view_store = Some(vs);
        self
    }

    /// Configure the dedup policy (I-X2).
    ///
    /// `TenantIsolated` requires a tenant HMAC key for chunk ID derivation.
    #[must_use]
    pub fn with_dedup_policy(mut self, policy: DedupPolicy, hmac_key: Option<Vec<u8>>) -> Self {
        self.dedup_policy = policy;
        self.tenant_hmac_key = hmac_key;
        self
    }
}

impl InMemoryGateway {
    /// Ensure a namespace exists — creates it if missing.
    ///
    /// Used by `create_bucket` so the composition store has a registered
    /// namespace before any object write targets it.
    pub async fn ensure_namespace_exists(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<(), GatewayError> {
        let mut comps = self.compositions.lock().await;
        if comps.namespace(namespace_id).is_none() {
            comps.add_namespace(kiseki_composition::namespace::Namespace {
                id: namespace_id,
                tenant_id,
                shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
                read_only: false,
                versioning_enabled: false,
                compliance_tags: Vec::new(),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl GatewayOps for InMemoryGateway {
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let compositions = self.compositions.lock().await;
        let chunks = self.chunks.lock().await;

        // Look up the composition.
        let comp = compositions
            .get(req.composition_id)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Verify tenant ownership (I-T1).
        if comp.tenant_id != req.tenant_id {
            return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
        }

        // Check view staleness and ReadYourWrites consistency (I-K9, I-V3).
        if let Some(ref vs) = self.view_store {
            let view_store = vs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

            for view_id in view_store.view_ids() {
                if let Ok(view) = view_store.get_view(view_id) {
                    // BoundedStaleness: check lag.
                    if view.check_staleness(now_ms).is_err() {
                        return Err(GatewayError::StaleView {
                            lag_ms: now_ms.saturating_sub(view.last_advanced_ms),
                        });
                    }
                    // ReadYourWrites: ensure view has caught up to our last write.
                    if matches!(
                        view.descriptor.consistency,
                        kiseki_view::ConsistencyModel::ReadYourWrites
                    ) {
                        let last_seq = self
                            .last_written_seq
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&(req.tenant_id, req.namespace_id))
                            .copied();
                        if let Some(seq) = last_seq {
                            if view.watermark < seq {
                                return Err(GatewayError::StaleView {
                                    lag_ms: 0, // not time-based, sequence-based
                                });
                            }
                        }
                    }
                }
            }
        }

        // Read and decrypt all chunks, concatenate.
        // Checks inline store first (ADR-030), then block device.
        let mut plaintext = Vec::new();
        for chunk_id in &comp.chunks {
            // Try inline store first (small files).
            let inline_hit = if let Some(ref store) = self.small_store {
                store
                    .get(&chunk_id.0)
                    .ok()
                    .flatten()
                    .and_then(|data| serde_json::from_slice::<envelope::Envelope>(&data).ok())
            } else {
                None
            };

            let env = if let Some(env) = inline_hit {
                env
            } else {
                // Fall back to chunk store (block device).
                chunks
                    .read_chunk(chunk_id)
                    .map_err(|e| GatewayError::Upstream(e.to_string()))?
            };

            let decrypted = envelope::open_envelope(&self.aead, &self.master_key, &env)
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            plaintext.extend_from_slice(&decrypted);
        }

        // Apply offset/length.
        let start = usize::try_from(req.offset).unwrap_or(usize::MAX);
        if start >= plaintext.len() {
            return Ok(ReadResponse {
                data: Vec::new(),
                eof: true,
            });
        }
        let length = usize::try_from(req.length).unwrap_or(usize::MAX);
        let end = std::cmp::min(start.saturating_add(length), plaintext.len());
        let eof = end >= plaintext.len();

        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_read
            .fetch_add((end - start) as u64, Ordering::Relaxed);

        Ok(ReadResponse {
            data: plaintext[start..end].to_vec(),
            eof,
        })
    }

    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        // Compute content-addressed chunk ID.
        let chunk_id = derive_chunk_id(
            &req.data,
            self.dedup_policy,
            self.tenant_hmac_key.as_deref(),
        )
        .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Encrypt the data (I-K1: no plaintext past the gateway boundary).
        let env = envelope::seal_envelope(&self.aead, &self.master_key, &chunk_id, &req.data)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        let bytes_written = req.data.len() as u64;

        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(bytes_written, Ordering::Relaxed);

        // Route: inline (ADR-030) or chunk store.
        if bytes_written <= self.inline_threshold && self.small_store.is_some() {
            let env_bytes =
                serde_json::to_vec(&env).map_err(|e| GatewayError::Upstream(e.to_string()))?;
            if let Some(ref store) = self.small_store {
                store
                    .put(&chunk_id.0, &env_bytes)
                    .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            }
        } else {
            // Chunk path: store encrypted envelope on block device.
            let mut chunks = self.chunks.lock().await;
            let is_new = chunks
                .write_chunk(env, "default")
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            if !is_new {
                let _ = chunks.increment_refcount(&chunk_id);
            }
        }

        // Create composition (sync, fast) — lock released before Raft.
        // Log emission happens after lock release to avoid holding the
        // Mutex across Raft consensus (ADR-032).
        let (comp_id, log, emit_params) = {
            let mut comps = self.compositions.lock().await;
            let comp_id = comps
                .create(req.namespace_id, vec![chunk_id], bytes_written)
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            let comp = comps.get(comp_id).unwrap();
            let params = (
                comp.shard_id,
                comp.tenant_id,
                comp.namespace_id,
                comp.chunks.clone(),
            );
            let log = comps.log().cloned();
            (comp_id, log, params)
        }; // Lock dropped here — before Raft consensus.

        // Emit delta to log (async, slow — Raft consensus).
        if let Some(ref log) = log {
            let hashed_key = kiseki_composition::composition_hash_key(emit_params.2, comp_id);
            if !kiseki_composition::log_bridge::emit_delta(
                log.as_ref(),
                emit_params.0,
                emit_params.1,
                kiseki_log::delta::OperationType::Create,
                hashed_key,
                emit_params.3,
                comp_id.0.as_bytes().to_vec(),
            )
            .await
            {
                // Rollback: re-acquire lock and remove (PIPE-ADV-1).
                let _ = self.compositions.lock().await.delete(comp_id).ok();
                return Err(GatewayError::Upstream("delta emission failed".to_string()));
            }
        }

        Ok(WriteResponse {
            composition_id: comp_id,
            bytes_written,
        })
    }

    async fn list(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<Vec<(kiseki_common::ids::CompositionId, u64)>, GatewayError> {
        // Filter by tenant_id to prevent cross-tenant composition ID leak.
        let compositions = self.compositions.lock().await;
        Ok(compositions
            .list_by_namespace(namespace_id)
            .into_iter()
            .filter(|c| c.tenant_id == tenant_id)
            .map(|c| (c.id, c.size))
            .collect())
    }

    async fn start_multipart(
        &self,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<String, GatewayError> {
        self.start_multipart_internal(namespace_id).await
    }

    async fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<String, GatewayError> {
        let chunk_id = self
            .upload_part_internal(upload_id, part_number, data)
            .await?;
        let mut hex = String::with_capacity(64);
        for b in &chunk_id.0 {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        Ok(hex)
    }

    async fn complete_multipart(
        &self,
        upload_id: &str,
    ) -> Result<kiseki_common::ids::CompositionId, GatewayError> {
        self.complete_multipart_internal(upload_id).await
    }

    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        self.abort_multipart_internal(upload_id).await
    }

    async fn ensure_namespace(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<(), GatewayError> {
        self.ensure_namespace_exists(tenant_id, namespace_id).await
    }

    async fn delete(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        _namespace_id: kiseki_common::ids::NamespaceId,
        composition_id: kiseki_common::ids::CompositionId,
    ) -> Result<(), GatewayError> {
        // Verify tenant ownership before deleting.
        {
            let compositions = self.compositions.lock().await;
            let comp = compositions
                .get(composition_id)
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            if comp.tenant_id != tenant_id {
                return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
            }
        }

        // Delete the composition (sync — no lock held during Raft).
        // Log emission for delete tombstone would go here if needed.
        let delete_result = self
            .compositions
            .lock()
            .await
            .delete(composition_id)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Decrement chunk refcounts only when actually removed (not
        // a versioned delete marker). I-C2: GC when refcount reaches 0.
        if let kiseki_composition::DeleteResult::Removed(ref released) = delete_result {
            let mut chunks = self.chunks.lock().await;
            for chunk_id in released {
                let _ = chunks.decrement_refcount(chunk_id);
            }
        }

        Ok(())
    }
}
