//! In-memory gateway — wires Composition + Chunk + Crypto for protocol gateways.
//!
//! Handles the full data path: plaintext from protocol client → encrypt →
//! chunk store → composition metadata, and reverse for reads.

use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::Arc;
use tokio::sync::Mutex;

use kiseki_chunk::AsyncChunkOps;
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
    chunks: Arc<dyn AsyncChunkOps>,
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
    /// Shard map store for multi-shard routing (ADR-033).
    /// When present, the gateway routes writes to the correct shard
    /// based on `hashed_key`. When absent, uses the namespace's single `shard_id`.
    shard_map:
        std::sync::RwLock<Option<Arc<kiseki_control::shard_topology::NamespaceShardMapStore>>>,
    /// Optional telemetry bus (ADR-021). When attached, the gateway emits
    /// per-workload backpressure events on saturation and QoS-headroom
    /// updates as quota is consumed (I-WA5: per-caller scoping).
    telemetry_bus: std::sync::RwLock<Option<Arc<kiseki_advisory::TelemetryBus>>>,
    /// Cluster placement for newly-created chunks (Phase 16b step 2).
    /// Stamped into `NewChunkMeta.placement` on every fresh chunk write
    /// so `cluster_chunk_state[(tenant, chunk_id)]` records who holds
    /// the fragments. Empty in single-node mode (`raft_peers.len() == 1`).
    cluster_placement: Vec<u64>,
    /// Chunk plaintext cache (Phase 15c.5 perf fix). Chunks are
    /// content-addressed (`chunk_id` = HMAC over plaintext + salt)
    /// so caching decrypted bytes by `chunk_id` is correct: the
    /// same id always yields the same plaintext and chunks are
    /// immutable.
    ///
    /// The cache eliminates re-decryption on sequential NFS reads:
    /// without it, every NFS3 READ call (kernel chunks at ~1 MiB)
    /// re-decrypts every chunk in the composition, turning a
    /// 32 MiB / 32 reads sequence into 32x redundant decrypt work.
    /// Measured baseline before this cache: 0.7 MB/s `NFSv3`
    /// `seq-read`.
    ///
    /// Bounded eviction: when the cache exceeds `MAX_CACHE_BYTES`
    /// (256 MiB), oldest entries are dropped FIFO. Memory pressure
    /// is bounded; correctness is unaffected because the chunk store
    /// is the source of truth.
    decrypt_cache: std::sync::Mutex<DecryptCache>,
}

const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Default)]
struct DecryptCache {
    /// Insertion-ordered (`chunk_id` → plaintext) for FIFO eviction.
    /// FIFO is good enough — sequential reads on a single composition
    /// hit the same chunk repeatedly within a short window, so even
    /// FIFO retains the hot chunk for the duration of a streaming
    /// read.
    map: std::collections::HashMap<kiseki_common::ids::ChunkId, Vec<u8>>,
    /// Eviction queue: front = oldest. Pop here when total exceeds cap.
    queue: std::collections::VecDeque<kiseki_common::ids::ChunkId>,
    /// Sum of `plaintext.len()` across the map.
    total_bytes: usize,
}

impl DecryptCache {
    fn get(&self, id: &kiseki_common::ids::ChunkId) -> Option<Vec<u8>> {
        self.map.get(id).cloned()
    }

    fn insert(&mut self, id: kiseki_common::ids::ChunkId, plaintext: Vec<u8>) {
        if self.map.contains_key(&id) {
            return; // duplicate insert — keep the existing entry
        }
        let len = plaintext.len();
        self.total_bytes += len;
        self.map.insert(id, plaintext);
        self.queue.push_back(id);
        while self.total_bytes > MAX_CACHE_BYTES {
            let Some(victim) = self.queue.pop_front() else {
                break;
            };
            if let Some(v) = self.map.remove(&victim) {
                self.total_bytes = self.total_bytes.saturating_sub(v.len());
            }
        }
    }
}

impl InMemoryGateway {
    /// Create a new in-memory gateway with the given crypto material.
    ///
    /// Uses `CrossTenant` dedup policy by default. Call
    /// `with_dedup_policy` to configure per-tenant isolation (I-X2).
    #[must_use]
    pub fn new(
        compositions: CompositionStore,
        chunks: Arc<dyn AsyncChunkOps>,
        master_key: SystemMasterKey,
    ) -> Self {
        Self {
            compositions: Mutex::new(compositions),
            chunks,
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
            shard_map: std::sync::RwLock::new(None),
            telemetry_bus: std::sync::RwLock::new(None),
            cluster_placement: Vec::new(),
            decrypt_cache: std::sync::Mutex::new(DecryptCache::default()),
        }
    }

    /// Attach a telemetry bus (ADR-021). Once attached, the gateway emits
    /// per-workload backpressure events when a write hits a saturation
    /// signal (e.g. `LogError::QuorumLost`, `KeyManagerError::Unavailable`)
    /// so subscribed workloads can react. No-op when the bus is absent
    /// (data path never blocks on advisory delivery — I-WA1/I-WA2).
    pub fn set_telemetry_bus(&self, bus: Arc<kiseki_advisory::TelemetryBus>) {
        *self.telemetry_bus.write().unwrap() = Some(bus);
    }

    /// Emit a per-workload backpressure event through the attached bus.
    /// `workload` is opaque (typically the `WorkloadId` from a workflow
    /// hint header). No-op when no bus is attached. Errors are
    /// swallowed — advisory must never block the data path (I-WA2).
    pub fn report_backpressure(
        &self,
        workload: &str,
        severity: kiseki_advisory::BackpressureSeverity,
        retry_after_ms: u64,
    ) {
        let Some(bus) = self.telemetry_bus.read().unwrap().clone() else {
            return;
        };
        bus.emit_backpressure(
            workload,
            kiseki_advisory::BackpressureEvent {
                severity,
                retry_after_ms: kiseki_advisory::bucket_retry_after_ms(retry_after_ms),
            },
        );
    }

    /// Attach a shard map store for multi-shard routing (ADR-033).
    #[must_use]
    pub fn with_shard_map(
        self,
        store: Arc<kiseki_control::shard_topology::NamespaceShardMapStore>,
    ) -> Self {
        *self.shard_map.write().unwrap() = Some(store);
        self
    }

    /// Clear the shard map (simulates stale cache — falls back to namespace `shard_id`).
    pub fn clear_shard_map(&self) {
        *self.shard_map.write().unwrap() = None;
    }

    /// Re-attach a shard map store after clearing (simulates cache refresh).
    pub fn set_shard_map(
        &self,
        store: Arc<kiseki_control::shard_topology::NamespaceShardMapStore>,
    ) {
        *self.shard_map.write().unwrap() = Some(store);
    }

    /// Simulate a gateway crash: drop all ephemeral state.
    ///
    /// Clears namespace cache, session tracking, and counters.
    /// Durable state (log store, chunk store) is unaffected.
    /// NFS opens/locks and in-flight multipart uploads are lost.
    pub async fn crash(&self) {
        // Clear composition namespace cache (ephemeral).
        self.compositions.lock().await.clear_namespaces();
        // Clear session tracking.
        self.last_written_seq.lock().unwrap().clear();
        // Reset counters.
        self.requests_total.store(0, Ordering::Relaxed);
        self.bytes_written.store(0, Ordering::Relaxed);
        self.bytes_read.store(0, Ordering::Relaxed);
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
            .write_chunk(env, "default")
            .await
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

    /// Configure cluster placement for fresh chunks (Phase 16b step 2).
    /// `placement` lists the node ids that hold each new chunk's
    /// fragments. Stamped into `NewChunkMeta.placement` so the per-shard
    /// Raft state machine can drive cross-cluster GC + repair scrub
    /// without re-discovering topology. Empty list = single-node mode.
    #[must_use]
    pub fn with_cluster_placement(mut self, placement: Vec<u64>) -> Self {
        self.cluster_placement = placement;
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

        // Read and decrypt all chunks, concatenate. Per-chunk plaintext
        // is cached by content-addressed chunk_id; cache hits skip the
        // envelope::open_envelope call (the dominant cost on
        // sequential NFS reads where the kernel issues many small
        // READs against the same composition).
        let mut plaintext = Vec::new();
        for chunk_id in &comp.chunks {
            // Cache lookup first (cheap mutex; clone is unavoidable
            // because the cache holds Vec<u8> shared across calls).
            if let Some(cached) = self
                .decrypt_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(chunk_id)
            {
                plaintext.extend_from_slice(&cached);
                continue;
            }

            // Cache miss — read from inline store first (ADR-030),
            // then block device.
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
                self.chunks
                    .read_chunk(chunk_id)
                    .await
                    .map_err(|e| GatewayError::Upstream(e.to_string()))?
            };

            let decrypted = envelope::open_envelope(&self.aead, &self.master_key, &env)
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            plaintext.extend_from_slice(&decrypted);

            // Insert into cache for the next call. Bounded eviction
            // ensures memory stays under MAX_CACHE_BYTES.
            self.decrypt_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(*chunk_id, decrypted);
        }

        // Pull Content-Type from the composition for RFC 6838 round-trip
        // (ADV-PA-4: store-side metadata, not per-instance HashMap).
        let content_type = comp.content_type.clone();

        // Apply offset/length.
        let start = usize::try_from(req.offset).unwrap_or(usize::MAX);
        if start >= plaintext.len() {
            return Ok(ReadResponse {
                data: Vec::new(),
                eof: true,
                content_type,
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
            content_type,
        })
    }

    async fn set_object_content_type(
        &self,
        composition_id: kiseki_common::ids::CompositionId,
        content_type: Option<String>,
    ) -> Result<(), GatewayError> {
        let mut comps = self.compositions.lock().await;
        comps
            .set_content_type(composition_id, content_type)
            .map_err(|e| GatewayError::Upstream(e.to_string()))
    }

    #[allow(clippy::too_many_lines)]
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        // POSIX/NFS read-only namespace gate (POSIX.1-2024 EROFS,
        // NFS3ERR_ROFS, NFSv4 NFS4ERR_ROFS). Performed before any
        // crypto/storage work so the rejection is cheap and uniform.
        {
            let comps = self.compositions.lock().await;
            if let Some(ns) = comps.namespace(req.namespace_id) {
                if ns.read_only {
                    return Err(GatewayError::ReadOnlyNamespace);
                }
            }
        }
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

        // Route: inline (ADR-030) or chunk store. `chunk_was_new`
        // tracks whether this write actually created a new chunk
        // (vs. a dedup hit) so the log proposal below can carry the
        // right Phase 16b cluster_chunk_state hint.
        let mut chunk_was_new = false;
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
            let is_new = self
                .chunks
                .write_chunk(env, "default")
                .await
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            if !is_new {
                let _ = self.chunks.increment_refcount(&chunk_id).await;
            }
            chunk_was_new = is_new;
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

            // ADR-033: route to correct shard via shard map if available.
            let shard_id = if let Some(ref shard_map) = *self.shard_map.read().unwrap() {
                // Convert NamespaceId to string for shard map lookup.
                let ns_str = emit_params.2 .0.to_string();
                if let Ok(map) = shard_map.get(&ns_str, emit_params.1) {
                    kiseki_control::shard_topology::route_to_shard(&map, &hashed_key)
                        .unwrap_or(emit_params.0)
                } else {
                    emit_params.0
                }
            } else {
                emit_params.0
            };

            // Phase 16b D-4: if this write created a new chunk, carry
            // it in the `new_chunks` list so the per-shard Raft state
            // machine seeds a `cluster_chunk_state[(tenant, chunk_id)]`
            // row atomically with the delta. Dedup-hit writes use the
            // plain `AppendDelta` path.
            let new_chunks: Vec<kiseki_log::raft_store::NewChunkMeta> = if chunk_was_new {
                vec![kiseki_log::raft_store::NewChunkMeta {
                    chunk_id: chunk_id.0,
                    placement: self.cluster_placement.clone(),
                }]
            } else {
                vec![]
            };

            match kiseki_composition::log_bridge::emit_chunk_and_delta(
                log.as_ref(),
                shard_id,
                emit_params.1,
                kiseki_log::delta::OperationType::Create,
                hashed_key,
                emit_params.3,
                comp_id.0.as_bytes().to_vec(),
                new_chunks,
            )
            .await
            {
                Ok(_seq) => {}
                Err(kiseki_log::error::LogError::KeyOutOfRange(sid)) => {
                    let _ = self.compositions.lock().await.delete(comp_id).ok();
                    return Err(GatewayError::KeyOutOfRange { shard_id: sid });
                }
                Err(e) => {
                    // Rollback: re-acquire lock and remove (PIPE-ADV-1).
                    let _ = self.compositions.lock().await.delete(comp_id).ok();
                    // ADR-021 / I-WA5: emit a per-tenant backpressure
                    // signal whenever the data path returns a retriable
                    // error. Subscribers (workloads with active workflow
                    // declarations on this tenant) can react. The emit
                    // is non-blocking — it never delays the data path
                    // (I-WA1, I-WA2). Only retriable errors produce a
                    // signal; permanent errors (KeyOutOfRange, etc.)
                    // are surfaced via their own typed paths above.
                    let detail = e.to_string();
                    let is_retriable = matches!(
                        e,
                        kiseki_log::error::LogError::ShardSplitting(_)
                            | kiseki_log::error::LogError::LeaderUnavailable(_)
                            | kiseki_log::error::LogError::QuorumLost(_)
                            | kiseki_log::error::LogError::Unavailable
                            | kiseki_log::error::LogError::ShardBusy { .. }
                            | kiseki_log::error::LogError::MaintenanceMode(_)
                    );
                    if is_retriable {
                        // Use the tenant id as the workload key — production
                        // deployments will swap this for the workflow_ref
                        // once ADR-021 hint headers reach the data path.
                        let workload = req.tenant_id.0.to_string();
                        self.report_backpressure(
                            &workload,
                            kiseki_advisory::BackpressureSeverity::Soft,
                            100,
                        );
                    }
                    return Err(GatewayError::Upstream(format!(
                        "delta emission failed: {detail}"
                    )));
                }
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
        // Verify tenant ownership and snapshot the routing data
        // (shard_id, log handle) before deleting. Phase 16b step 2:
        // we need the shard to emit DecrementChunkRefcount Raft
        // proposals after the composition is gone.
        let (shard_id, log) = {
            let compositions = self.compositions.lock().await;
            let comp = compositions
                .get(composition_id)
                .map_err(|e| GatewayError::Upstream(e.to_string()))?;
            if comp.tenant_id != tenant_id {
                return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
            }
            (comp.shard_id, compositions.log().cloned())
        };

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
        // Two refcount tracks:
        //   - Local `ChunkStore` refcount drives this node's GC sweep.
        //   - `cluster_chunk_state` refcount drives cluster-wide GC
        //     (DeleteFragment fan-out lands in the next step).
        if let kiseki_composition::DeleteResult::Removed(ref released) = delete_result {
            for chunk_id in released {
                let _ = self.chunks.decrement_refcount(chunk_id).await;
                if let Some(ref log) = log {
                    let _ = log
                        .decrement_chunk_refcount(shard_id, tenant_id, *chunk_id)
                        .await;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod telemetry_wiring_tests {
    use super::*;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    /// `report_backpressure` is a no-op when no telemetry bus is attached
    /// — proves the data path doesn't depend on the bus (I-WA1, I-WA2).
    #[tokio::test]
    async fn report_backpressure_is_noop_without_bus() {
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );
        // Must not panic, must not block, must not allocate a channel.
        gw.report_backpressure(
            "any-tenant",
            kiseki_advisory::BackpressureSeverity::Soft,
            42,
        );
    }

    /// `report_backpressure` delivers a bucketed event to the named
    /// workload's subscriber — proves the wiring from gateway → bus →
    /// subscriber works end-to-end.
    #[tokio::test]
    async fn report_backpressure_delivers_bucketed_event_to_subscriber() {
        let bus = Arc::new(kiseki_advisory::TelemetryBus::new());
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );
        gw.set_telemetry_bus(Arc::clone(&bus));
        let mut rx = bus.subscribe_backpressure("tenant-x");

        gw.report_backpressure("tenant-x", kiseki_advisory::BackpressureSeverity::Soft, 75);

        let evt = rx.try_recv().expect("subscriber must receive event");
        assert_eq!(evt.severity, kiseki_advisory::BackpressureSeverity::Soft);
        // Raw 75ms must have been bucketed to the next fixed bucket (100).
        assert_eq!(evt.retry_after_ms, 100);
    }

    /// A neighbouring tenant's subscriber must NOT see another tenant's
    /// backpressure (I-WA5: per-caller scoping).
    #[tokio::test]
    async fn neighbour_tenant_does_not_see_emit() {
        let bus = Arc::new(kiseki_advisory::TelemetryBus::new());
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );
        gw.set_telemetry_bus(Arc::clone(&bus));
        let mut alice = bus.subscribe_backpressure("alice");
        let mut bob = bus.subscribe_backpressure("bob");

        gw.report_backpressure("alice", kiseki_advisory::BackpressureSeverity::Hard, 500);

        assert!(alice.try_recv().is_ok(), "alice receives her event");
        assert!(bob.try_recv().is_err(), "bob must not see alice's event");
    }
}
