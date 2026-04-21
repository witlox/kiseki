//! In-memory gateway — wires Composition + Chunk + Crypto for protocol gateways.
//!
//! Handles the full data path: plaintext from protocol client → encrypt →
//! chunk store → composition metadata, and reverse for reads.

use std::sync::Mutex;

use std::sync::Arc;

use kiseki_chunk::store::{ChunkOps, ChunkStore};
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
/// Uses `Mutex` for interior mutability so `GatewayOps` methods can
/// take `&self`, enabling concurrent access.
pub struct InMemoryGateway {
    compositions: Mutex<CompositionStore>,
    chunks: Mutex<ChunkStore>,
    aead: Aead,
    master_key: SystemMasterKey,
    dedup_policy: DedupPolicy,
    tenant_hmac_key: Option<Vec<u8>>,
    view_store: Option<Arc<Mutex<ViewStore>>>,
}

impl InMemoryGateway {
    /// Create a new in-memory gateway with the given crypto material.
    ///
    /// Uses `CrossTenant` dedup policy by default. Call
    /// `with_dedup_policy` to configure per-tenant isolation (I-X2).
    #[must_use]
    pub fn new(
        compositions: CompositionStore,
        chunks: ChunkStore,
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
        }
    }

    /// Register a namespace in the gateway's composition store.
    ///
    /// Namespaces are created by the Control Plane and must be registered
    /// with the gateway before any write/read operations can target them.
    pub fn add_namespace(&self, ns: kiseki_composition::namespace::Namespace) {
        self.compositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add_namespace(ns);
    }

    /// List compositions in a namespace (for S3 `ListObjectsV2`).
    pub fn list_compositions(
        &self,
        ns_id: kiseki_common::ids::NamespaceId,
    ) -> Vec<(kiseki_common::ids::CompositionId, u64)> {
        let compositions = self
            .compositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compositions
            .list_by_namespace(ns_id)
            .into_iter()
            .map(|c| (c.id, c.size))
            .collect()
    }

    /// Attach a shared view store for staleness enforcement (I-K9).
    #[must_use]
    pub fn with_view_store(mut self, vs: Arc<Mutex<ViewStore>>) -> Self {
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

impl GatewayOps for InMemoryGateway {
    fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let compositions = self
            .compositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Look up the composition.
        let comp = compositions
            .get(req.composition_id)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Verify tenant ownership (I-T1).
        if comp.tenant_id != req.tenant_id {
            return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
        }

        // Check view staleness (I-K9) if view store is attached.
        if let Some(ref vs) = self.view_store {
            let view_store = vs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Find a view covering this composition's shard.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            // Check all views for staleness — any stale view blocks the read.
            // In production, only the view serving this shard matters.
            for view_id in view_store.view_ids() {
                if let Ok(view) = view_store.get_view(view_id) {
                    if view.check_staleness(now_ms).is_err() {
                        return Err(GatewayError::StaleView {
                            lag_ms: now_ms.saturating_sub(view.last_advanced_ms),
                        });
                    }
                }
            }
        }

        // Read and decrypt all chunks, concatenate.
        let mut plaintext = Vec::new();
        for chunk_id in &comp.chunks {
            let env = chunks
                .read_chunk(chunk_id)
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            let decrypted = envelope::open_envelope(&self.aead, &self.master_key, env)
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

        Ok(ReadResponse {
            data: plaintext[start..end].to_vec(),
            eof,
        })
    }

    fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        // Compute content-addressed chunk ID.
        // TODO(I-X2): Production must look up the tenant's DedupPolicy.
        // TenantIsolated tenants need HMAC-SHA256 with their tenant HMAC
        // key to prevent cross-tenant co-occurrence analysis.
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

        // Store the encrypted chunk.
        self.chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_chunk(env, "default")
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        // Create a composition referencing this chunk.
        let comp_id = self
            .compositions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .create(req.namespace_id, vec![chunk_id], bytes_written)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        Ok(WriteResponse {
            composition_id: comp_id,
            bytes_written,
        })
    }

    fn list(
        &self,
        _tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<Vec<(kiseki_common::ids::CompositionId, u64)>, GatewayError> {
        Ok(self.list_compositions(namespace_id))
    }
}
