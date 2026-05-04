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

/// Per-chunk landing record used by `MemGateway::write` to track each
/// piece of a multi-chunk composition for the post-write Raft delta.
struct ChunkLanded {
    id: kiseki_common::ids::ChunkId,
    ciphertext_len: u64,
    was_new: bool,
}

/// Maximum plaintext bytes per chunk on the write path.
///
/// Bug 4 (GCP 2026-05-04): without this cap the gateway emitted one
/// envelope per S3 PUT, so any payload larger than the fabric's
/// per-envelope cap (`FABRIC_CIPHERTEXT_MAX_BYTES = 256 MiB`) failed
/// cross-node replication with an h2 / "quorum lost" error. Splitting
/// at this boundary keeps every fabric envelope well under the cap
/// (AES-256-GCM ciphertext == plaintext length + headroom for the
/// envelope wrapper) while keeping per-PUT chunk count bounded for
/// metadata / refcount overhead.
pub const MAX_PLAINTEXT_PER_CHUNK: usize = 64 * 1024 * 1024;
use kiseki_crypto::aead::Aead;
use kiseki_crypto::chunk_id::derive_chunk_id;
use kiseki_crypto::envelope;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_view::view::{ViewOps, ViewStore};

use crate::error::GatewayError;
use crate::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};
use kiseki_common::locks::LockOrDie;

/// In-memory gateway backed by composition store, chunk store, and crypto.
///
/// Uses `tokio::sync::Mutex` for interior mutability so `GatewayOps` methods can
/// take `&self`, enabling concurrent access.
pub struct InMemoryGateway {
    /// Shared with the Phase 16f composition hydrator (`compositions_handle`)
    /// so leader-emitted Create deltas land in the same store the gateway
    /// reads from on followers.
    compositions: Arc<Mutex<CompositionStore>>,
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
    /// Optional shared workflow table (ADR-021 §3.b). When attached,
    /// the data-path validates `x-kiseki-workflow-ref` headers against
    /// active workflows declared via the advisory gRPC service. Hits
    /// become observable via `kiseki_gateway_workflow_ref_writes_total
    /// {result}` (`valid` / `invalid` / `absent`). Misses fall through
    /// to a normal write (I-WA1: header is advisory, never blocks the
    /// data path). The gateway never mutates the table — only reads.
    workflow_table:
        std::sync::RwLock<Option<Arc<std::sync::Mutex<kiseki_advisory::WorkflowTable>>>>,
    /// Per-result counter for `workflow_ref` header validation.
    /// Indexed `[absent, valid, invalid]` — three buckets keep us out
    /// of a `HashMap` on the hot path. Reset to 0 on construction;
    /// scraped via `workflow_ref_writes_total(result)`.
    workflow_ref_writes: [AtomicU64; 3],
    /// Optional Prometheus counter mirror (`kiseki_gateway_workflow_
    /// ref_writes_total{result=...}`). When set, every `workflow_ref`
    /// validation also `.inc()`s the labeled counter so the
    /// `/metrics` endpoint scrapes the live count without polling
    /// the atomic. Tests + single-node deployments without metrics
    /// wiring leave this `None` and fall back to the atomic.
    workflow_ref_writes_metric: std::sync::RwLock<Option<Arc<prometheus::IntCounterVec>>>,
    /// Optional Prometheus counter mirror for `kiseki_chunk_write_
    /// bytes` (cumulative bytes the gateway has written through the
    /// chunk store, including the inline + chunk + EC paths). The
    /// runtime wires this from `metrics::Metrics::chunk_write_bytes`
    /// so `/metrics` always reflects the gateway's atomic
    /// `bytes_written`. Without the wiring, the metric stays at 0
    /// even under load — the GCP 2026-05-02 perf cluster surfaced
    /// this. Optional so tests + library users without metrics
    /// configured aren't forced to set it.
    chunk_write_bytes_metric: std::sync::RwLock<Option<Arc<prometheus::IntCounter>>>,
    /// Optional Prometheus counter mirror for `kiseki_chunk_read_
    /// bytes` — same shape as `chunk_write_bytes_metric`.
    chunk_read_bytes_metric: std::sync::RwLock<Option<Arc<prometheus::IntCounter>>>,
    /// Candidate cluster nodes used by the placement function
    /// (Phase 16b step 2). The full set of node ids; the actual
    /// per-chunk placement is the rendezvous-hashing-selected
    /// subset of size `target_copies`.
    cluster_placement: Vec<u64>,
    /// Number of fragments stamped into `NewChunkMeta.placement`
    /// per fresh write (Phase 16c step 2). 0 means "carry the whole
    /// `cluster_placement` list" — kept as a backwards-compatible
    /// fallback for clusters that haven't been updated to set a
    /// target. Production runtimes set this from the per-cluster-
    /// size durability defaults table.
    target_copies: usize,
    /// Optional read-path retry metrics (ADR-040 §D7 + §D10 — F-4
    /// closure). When `Some`, the read path increments
    /// `read_retry_total` on every retry-loop hit and
    /// `read_retry_exhausted_total` on every budget-exhausted miss.
    /// Tests and single-node deployments that don't wire metrics
    /// get no-op behavior.
    retry_metrics: Option<Arc<crate::metrics::GatewayRetryMetrics>>,
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
            compositions: Arc::new(Mutex::new(compositions)),
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
            workflow_table: std::sync::RwLock::new(None),
            workflow_ref_writes: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            workflow_ref_writes_metric: std::sync::RwLock::new(None),
            chunk_write_bytes_metric: std::sync::RwLock::new(None),
            chunk_read_bytes_metric: std::sync::RwLock::new(None),
            cluster_placement: Vec::new(),
            target_copies: 0,
            retry_metrics: None,
            decrypt_cache: std::sync::Mutex::new(DecryptCache::default()),
        }
    }

    /// Attach a shared workflow table (ADR-021 §3.b). The data-path
    /// uses it to validate `x-kiseki-workflow-ref` headers against
    /// active workflows declared via the advisory gRPC service. The
    /// runtime wires the same `Arc` into both `AdvisoryGrpc` and the
    /// gateway so a `DeclareWorkflow` RPC is observable to the next
    /// S3 PUT immediately, without an out-of-band sync window.
    pub fn set_workflow_table(&self, table: Arc<std::sync::Mutex<kiseki_advisory::WorkflowTable>>) {
        *self.workflow_table.write().lock_or_die("mem_gateway.unknown") = Some(table);
    }

    /// Snapshot the `workflow_ref` counters as
    /// `[absent, valid, invalid]`. Used by metrics/observability —
    /// the BDD harness scrapes these to assert end-to-end behavior of
    /// the header validation path.
    #[must_use]
    pub fn workflow_ref_writes_total(&self) -> [u64; 3] {
        [
            self.workflow_ref_writes[0].load(Ordering::Relaxed),
            self.workflow_ref_writes[1].load(Ordering::Relaxed),
            self.workflow_ref_writes[2].load(Ordering::Relaxed),
        ]
    }

    /// Attach a Prometheus counter for `workflow_ref` outcomes. The
    /// runtime wires the registered `kiseki_gateway_workflow_ref_
    /// writes_total` counter so metrics scrapes return live values.
    pub fn set_workflow_ref_writes_metric(&self, counter: Arc<prometheus::IntCounterVec>) {
        *self.workflow_ref_writes_metric.write().lock_or_die("mem_gateway.unknown") = Some(counter);
    }

    /// Attach Prometheus counters for chunk byte traffic. The
    /// gateway's existing `bytes_written`/`bytes_read` `AtomicU64s`
    /// are mirrored to these counters per-request so `/metrics`
    /// scrapes always reflect the live values.
    pub fn set_chunk_byte_metrics(
        &self,
        write_bytes: Arc<prometheus::IntCounter>,
        read_bytes: Arc<prometheus::IntCounter>,
    ) {
        *self.chunk_write_bytes_metric.write().lock_or_die("mem_gateway.unknown") = Some(write_bytes);
        *self.chunk_read_bytes_metric.write().lock_or_die("mem_gateway.unknown") = Some(read_bytes);
    }

    /// Attach a telemetry bus (ADR-021). Once attached, the gateway emits
    /// per-workload backpressure events when a write hits a saturation
    /// signal (e.g. `LogError::QuorumLost`, `KeyManagerError::Unavailable`)
    /// so subscribed workloads can react. No-op when the bus is absent
    /// (data path never blocks on advisory delivery — I-WA1/I-WA2).
    pub fn set_telemetry_bus(&self, bus: Arc<kiseki_advisory::TelemetryBus>) {
        *self.telemetry_bus.write().lock_or_die("mem_gateway.unknown") = Some(bus);
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
        let Some(bus) = self.telemetry_bus.read().lock_or_die("mem_gateway.unknown").clone() else {
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
        *self.shard_map.write().lock_or_die("mem_gateway.unknown") = Some(store);
        self
    }

    /// Clear the shard map (simulates stale cache — falls back to namespace `shard_id`).
    pub fn clear_shard_map(&self) {
        *self.shard_map.write().lock_or_die("mem_gateway.unknown") = None;
    }

    /// Re-attach a shard map store after clearing (simulates cache refresh).
    pub fn set_shard_map(
        &self,
        store: Arc<kiseki_control::shard_topology::NamespaceShardMapStore>,
    ) {
        *self.shard_map.write().lock_or_die("mem_gateway.unknown") = Some(store);
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
        self.last_written_seq.lock().lock_or_die("mem_gateway.unknown").clear();
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
            .unwrap_or_default()
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

        // Capture `was_new` from the chunk store so the eventual
        // `complete_multipart_internal` can include this part's chunk
        // in the Create-delta's `new_chunks` list (Raft state machine
        // seeds `cluster_chunk_state` from that). On a dedup hit
        // (`was_new = false`), the chunk is already tracked from a
        // previous write — don't double-seed.
        let was_new = self
            .chunks
            .write_chunk(env, "default")
            .await
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;
        if !was_new {
            // Mirror the regular write path: dedup hits bump the
            // chunk's local refcount so cleanup-on-delete is correct.
            let _ = self.chunks.increment_refcount(&chunk_id).await;
        }

        self.compositions
            .lock()
            .await
            .upload_part(upload_id, part_number, chunk_id, size, was_new)
            .map_err(|e| GatewayError::Upstream(e.to_string()))?;

        Ok(chunk_id)
    }

    /// Complete a multipart upload.
    ///
    /// `name` is the optional S3 URL key. When `Some`, the resulting
    /// composition is bound to it in the per-bucket name index AND
    /// the binding is replicated via the Raft Create-delta's v2
    /// payload so followers' hydrators install the same name binding.
    /// Without this, multipart-uploaded objects would be addressable
    /// by key only on the leader — followers would see the
    /// composition (via the delta) but not the name binding (via the
    /// local-only `bind_name` call), and a GET-by-key on a follower
    /// would silently 404.
    ///
    /// The Create-delta also carries `new_chunks` (built from each
    /// part's tracked `was_new` bit) so `cluster_chunk_state` is
    /// seeded for the multipart's chunks. Cross-node fabric reads
    /// rely on that state — without it, a GET routed to a follower
    /// would `ChunkLost` because no peer knows where the fragment
    /// went.
    #[allow(clippy::too_many_lines)]
    pub async fn complete_multipart_internal(
        &self,
        upload_id: &str,
        name: Option<&str>,
    ) -> Result<kiseki_common::ids::CompositionId, GatewayError> {
        // Phase 1: lock store, finalize, gather everything we need
        // for the Raft emit + name bind, drop the lock before any
        // await on Raft consensus (ADR-032).
        let (comp_id, log, emit_params, new_chunk_ids) = {
            let mut comps = self.compositions.lock().await;
            let comp_id = comps.finalize_multipart(upload_id).map_err(|e| {
                tracing::warn!(error = %e, "complete_multipart: finalize_multipart failed");
                GatewayError::Upstream(e.to_string())
            })?;
            // Pull part-level metadata while we still hold the lock.
            // Need `was_new` to build the new_chunks list below.
            let new_chunk_ids: Vec<kiseki_common::ids::ChunkId> = comps
                .multipart_parts(upload_id)
                .into_iter()
                .filter(|p| p.was_new)
                .map(|p| p.chunk_id)
                .collect();
            let comp = comps.get(comp_id).map_err(|e| {
                tracing::warn!(error = %e, "complete_multipart: get post-finalize failed");
                GatewayError::Upstream(e.to_string())
            })?;
            let params = (
                comp.shard_id,
                comp.tenant_id,
                comp.namespace_id,
                comp.chunks.clone(),
                comp.size,
            );
            // Bind the URL key locally so the leader resolves
            // GET-by-key immediately; followers get the same binding
            // via the Raft delta (v2 payload's `name` field).
            if let Some(n) = name {
                comps
                    .bind_name(comp.namespace_id, n.to_owned(), comp_id)
                    .map_err(|e| {
                        tracing::warn!(error = %e, "complete_multipart: local bind_name failed");
                        GatewayError::Upstream(e.to_string())
                    })?;
            }
            let log = comps.log().cloned();
            (comp_id, log, params, new_chunk_ids)
        };
        tracing::debug!(
            comp_id = %comp_id.0,
            shard_id = %emit_params.0.0,
            new_chunks = new_chunk_ids.len(),
            "complete_multipart: pre-Raft",
        );

        // Phase 2: emit Create delta — v2 payload carries the optional
        // name; new_chunks list seeds cluster_chunk_state for cross-
        // node reads. This is structurally identical to the regular
        // `write` path above; sharing the boilerplate would muddy
        // both code paths.
        if let Some(ref log) = log {
            let hashed_key = kiseki_composition::composition_hash_key(emit_params.2, comp_id);
            let shard_id = if let Some(ref shard_map) = *self.shard_map.read().lock_or_die("mem_gateway.unknown") {
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
            let new_chunks: Vec<kiseki_log::raft_store::NewChunkMeta> = new_chunk_ids
                .iter()
                .map(|chunk_id| {
                    let placement = if self.target_copies > 0 {
                        kiseki_chunk_cluster::pick_placement(
                            chunk_id,
                            &self.cluster_placement,
                            self.target_copies,
                        )
                    } else {
                        self.cluster_placement.clone()
                    };
                    // Multipart parts went through `chunks.write_chunk`
                    // (which wrapped each part in an envelope) so the
                    // ciphertext length per chunk is the per-part
                    // ciphertext, not the composition total. We don't
                    // have it cleanly accessible here without re-reading
                    // the chunk; use 0 to indicate "use the trim-zeros
                    // fallback on read" — the EC layer's trim heuristic
                    // is correct for AES-GCM ciphertext.
                    kiseki_log::raft_store::NewChunkMeta {
                        chunk_id: chunk_id.0,
                        placement,
                        original_len: 0,
                    }
                })
                .collect();

            let comp_payload = kiseki_composition::encode_composition_create_payload_named(
                comp_id,
                emit_params.2,
                emit_params.4,
                name,
            );
            tracing::debug!(
                comp_id = %comp_id.0,
                shard_id = %shard_id.0,
                "complete_multipart: emit_chunk_and_delta start",
            );
            kiseki_composition::log_bridge::emit_chunk_and_delta(
                log.as_ref(),
                shard_id,
                emit_params.1,
                kiseki_log::delta::OperationType::Create,
                hashed_key,
                emit_params.3,
                comp_payload,
                new_chunks,
            )
            .await
            .map_err(|e| {
                tracing::warn!(
                    comp_id = %comp_id.0,
                    error = %e,
                    "complete_multipart: emit_chunk_and_delta failed — composition is local-only",
                );
                GatewayError::Upstream(format!("multipart create-delta emit: {e}"))
            })?;
            tracing::debug!(comp_id = %comp_id.0, "complete_multipart: Raft Create-delta committed");
        }
        Ok(comp_id)
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

    /// Shared handle to the composition store.
    ///
    /// The Phase 16f composition hydrator (a sibling of the view stream
    /// processor) holds a clone of this `Arc` so it can install
    /// leader-emitted compositions into the same store the gateway reads
    /// from. The lock is `tokio::sync::Mutex` because the gateway holds
    /// it across awaits in the read path.
    #[must_use]
    pub fn compositions_handle(&self) -> Arc<Mutex<CompositionStore>> {
        Arc::clone(&self.compositions)
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
    /// `placement` lists the candidate node ids; the actual per-chunk
    /// placement is selected via rendezvous hashing in the write path.
    /// Empty list = single-node mode.
    #[must_use]
    pub fn with_cluster_placement(mut self, placement: Vec<u64>) -> Self {
        self.cluster_placement = placement;
        self
    }

    /// Phase 16c step 2: cap the per-chunk placement at `target_copies`
    /// nodes, picked deterministically via rendezvous hashing. When 0
    /// (default) the gateway carries the whole `cluster_placement`
    /// list — preserves the 16b behaviour for clusters that haven't
    /// been updated to set a target.
    #[must_use]
    pub fn with_target_copies(mut self, target_copies: usize) -> Self {
        self.target_copies = target_copies;
        self
    }

    /// Attach Prometheus retry metrics (ADR-040 §D7 / F-4 closure).
    /// Once wired, every read that exits the retry loop with a hit
    /// increments `kiseki_gateway_read_retry_total`; every read
    /// that exhausts the budget increments
    /// `kiseki_gateway_read_retry_exhausted_total`.
    #[must_use]
    pub fn with_retry_metrics(mut self, metrics: Arc<crate::metrics::GatewayRetryMetrics>) -> Self {
        self.retry_metrics = Some(metrics);
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
    // The read path is a single sequence (composition lookup with bounded
    // retry → per-chunk decrypt with cache → offset/length slice + view
    // staleness check) that doesn't decompose cleanly. Splitting would
    // obscure the read-path data flow more than it would help.
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        skip(self, req),
        fields(
            tenant_id = %req.tenant_id.0,
            namespace_id = %req.namespace_id.0,
            composition_id = %req.composition_id.0,
            offset = req.offset,
            length = req.length,
        ),
    )]
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        tracing::debug!("gateway read: entry");
        // Phase 16f / ADR-040 §D7: on a follower, the hydrator may not
        // have applied the create-delta yet for a composition the
        // client just PUT on the leader. Retry briefly so a tight
        // PUT-then-GET pattern doesn't 404 spuriously.
        //
        // Budget is configurable via `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS`
        // (default 1000). Operators on slow disks or under load can
        // tune up; the default fits well-provisioned NVMe.
        //
        // ADR-040 §D6.3 + I-2: if the persistent hydrator is in halt
        // mode (compaction outran it), missing-composition lookups
        // map to `ServiceUnavailable` so the S3 gateway returns 503
        // and load balancers route around. Existing-composition
        // lookups (cache or redb hit) still serve normally.
        #[allow(clippy::items_after_statements)]
        const COMP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
        let budget = std::env::var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1000);
        let comp_retry_budget = std::time::Duration::from_millis(budget);
        let deadline = std::time::Instant::now() + comp_retry_budget;
        let compositions = loop {
            let guard = self.compositions.lock().await;
            if guard.get(req.composition_id).is_ok() {
                if let Some(ref m) = self.retry_metrics {
                    m.read_retry_total.inc();
                }
                break guard;
            }
            // Halt-mode short-circuit: if the hydrator can't catch up,
            // there's no point waiting out the budget. Surface a
            // retry-elsewhere signal immediately.
            if guard.storage().halted().unwrap_or(false) {
                tracing::warn!(
                    "gateway read: composition hydrator halted — returning ServiceUnavailable"
                );
                return Err(GatewayError::ServiceUnavailable(format!(
                    "composition hydrator halted; retry against another node (composition_id={})",
                    req.composition_id.0
                )));
            }
            drop(guard);
            if std::time::Instant::now() >= deadline {
                if let Some(ref m) = self.retry_metrics {
                    m.read_retry_exhausted_total.inc();
                }
                tracing::warn!(
                    budget_ms = budget,
                    "gateway read: composition not found within retry budget — surfacing inner error",
                );
                // Surface the original error path.
                let g = self.compositions.lock().await;
                let _ = g.get(req.composition_id).map_err(|e| {
                    tracing::warn!(error = %e, "gateway read: compositions.get failed (post-budget)");
                    GatewayError::Upstream(e.to_string())
                })?;
                // Unreachable: get() failed by contract; the `?` returned.
                break g;
            }
            tokio::time::sleep(COMP_RETRY_INTERVAL).await;
        };

        // Look up the composition.
        let comp = compositions.get(req.composition_id).map_err(|e| {
            tracing::warn!(error = %e, "gateway read: compositions.get failed");
            GatewayError::Upstream(e.to_string())
        })?;

        // Verify tenant ownership (I-T1).
        if comp.tenant_id != req.tenant_id {
            tracing::warn!(
                expected_tenant = %req.tenant_id.0,
                actual_tenant = %comp.tenant_id.0,
                "gateway read: tenant mismatch — returning AuthenticationFailed",
            );
            return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
        }

        // Check view staleness and ReadYourWrites consistency (I-K9, I-V3).
        if let Some(ref vs) = self.view_store {
            let view_store = vs.lock().lock_or_die("mem_gateway.view_store");
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

            for view_id in view_store.view_ids() {
                if let Ok(view) = view_store.get_view(view_id) {
                    // BoundedStaleness: check lag.
                    if view.check_staleness(now_ms).is_err() {
                        let lag_ms = now_ms.saturating_sub(view.last_advanced_ms);
                        tracing::warn!(
                            view_id = %view_id.0,
                            lag_ms,
                            "gateway read: view BoundedStaleness exceeded",
                        );
                        return Err(GatewayError::StaleView { lag_ms });
                    }
                    // ReadYourWrites: ensure view has caught up to our last write.
                    if matches!(
                        view.descriptor.consistency,
                        kiseki_view::ConsistencyModel::ReadYourWrites
                    ) {
                        let last_seq = self
                            .last_written_seq
                            .lock()
                            .lock_or_die("mem_gateway.last_written_seq")
                            .get(&(req.tenant_id, req.namespace_id))
                            .copied();
                        if let Some(seq) = last_seq {
                            if view.watermark < seq {
                                tracing::warn!(
                                    view_id = %view_id.0,
                                    view_watermark = ?view.watermark,
                                    last_written_seq = ?seq,
                                    "gateway read: view watermark behind last_written_seq (RYW)",
                                );
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
                .lock_or_die("mem_gateway.decrypt_cache")
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
                tracing::debug!(?chunk_id, "gateway read: inline hit");
                env
            } else {
                // Fall back to chunk store (block device).
                tracing::debug!(?chunk_id, "gateway read: chunk store fetch");
                self.chunks.read_chunk(chunk_id).await.map_err(|e| {
                    tracing::warn!(?chunk_id, error = %e, "gateway read: chunks.read_chunk failed");
                    GatewayError::Upstream(e.to_string())
                })?
            };

            let decrypted =
                envelope::open_envelope(&self.aead, &self.master_key, &env).map_err(|e| {
                    tracing::warn!(?chunk_id, error = %e, "gateway read: open_envelope failed");
                    GatewayError::Upstream(e.to_string())
                })?;
            plaintext.extend_from_slice(&decrypted);

            // Insert into cache for the next call. Bounded eviction
            // ensures memory stays under MAX_CACHE_BYTES.
            self.decrypt_cache
                .lock()
                .lock_or_die("mem_gateway.decrypt_cache")
                .insert(*chunk_id, decrypted);
        }

        // Pull Content-Type from the composition for RFC 6838 round-trip
        // (ADV-PA-4: store-side metadata, not per-instance HashMap).
        let content_type = comp.content_type.clone();

        // Apply offset/length.
        let start = usize::try_from(req.offset).unwrap_or(usize::MAX);
        if start >= plaintext.len() {
            tracing::debug!(
                plaintext_len = plaintext.len(),
                "gateway read: offset beyond EOF — returning empty",
            );
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
        let returned: u64 = (end - start) as u64;
        self.bytes_read.fetch_add(returned, Ordering::Relaxed);
        if let Some(c) = self
            .chunk_read_bytes_metric
            .read()
            .lock_or_die("mem_gateway.chunk_read_bytes_metric")
            .as_ref()
        {
            c.inc_by(returned);
        }

        tracing::debug!(returned_bytes = end - start, eof, "gateway read: success",);
        Ok(ReadResponse {
            data: plaintext[start..end].to_vec(),
            eof,
            content_type,
        })
    }

    #[tracing::instrument(skip(self, content_type), fields(composition_id = %composition_id.0))]
    async fn set_object_content_type(
        &self,
        composition_id: kiseki_common::ids::CompositionId,
        content_type: Option<String>,
    ) -> Result<(), GatewayError> {
        let mut comps = self.compositions.lock().await;
        comps
            .set_content_type(composition_id, content_type)
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway set_object_content_type: failed");
                GatewayError::Upstream(e.to_string())
            })
    }

    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        skip(self, req),
        fields(
            tenant_id = %req.tenant_id.0,
            namespace_id = %req.namespace_id.0,
            bytes = req.data.len(),
        ),
    )]
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        tracing::debug!("gateway write: entry");
        // ADR-021 §3.b / I-WA1: validate the optional workflow_ref
        // header before any storage work. The header is advisory: an
        // unknown ref or one with no shared workflow table simply
        // records `invalid` / `absent` and the write proceeds. The
        // counter is observable via `workflow_ref_writes_total` and
        // the BDD harness asserts on it to prove the header path is
        // wired end-to-end.
        let bucket: usize = match req.workflow_ref {
            None => 0, // absent
            Some(handle) => {
                let table_arc = self
                    .workflow_table
                    .read()
                    .lock_or_die("mem_gateway.workflow_table")
                    .clone();
                match table_arc {
                    None => {
                        tracing::debug!(
                            "gateway write: workflow_ref present but no table → absent"
                        );
                        0
                    }
                    Some(table) => {
                        let wf_ref = kiseki_common::advisory::WorkflowRef(handle);
                        let table = table
                            .lock()
                            .lock_or_die("mem_gateway.table");
                        if table.get(&wf_ref).is_some() {
                            tracing::debug!(
                                workflow_ref = %uuid::Uuid::from_bytes(handle),
                                "gateway write: workflow_ref valid",
                            );
                            1
                        } else {
                            tracing::debug!(
                                workflow_ref = %uuid::Uuid::from_bytes(handle),
                                "gateway write: workflow_ref invalid (advisory ignore — I-WA1)",
                            );
                            2
                        }
                    }
                }
            }
        };
        self.workflow_ref_writes[bucket].fetch_add(1, Ordering::Relaxed);
        if let Some(counter) = self
            .workflow_ref_writes_metric
            .read()
            .lock_or_die("mem_gateway.workflow_ref_writes_metric")
            .as_ref()
        {
            let label = ["absent", "valid", "invalid"][bucket];
            counter.with_label_values(&[label]).inc();
        }

        // POSIX/NFS read-only namespace gate (POSIX.1-2024 EROFS,
        // NFS3ERR_ROFS, NFSv4 NFS4ERR_ROFS). Performed before any
        // crypto/storage work so the rejection is cheap and uniform.
        // Also evaluate the optional HTTP-derived conditional in the
        // same critical section so the existence-check + decision +
        // (later) bind are race-free against concurrent writers to
        // the same name. The S3 layer relies on this — if two PUTs
        // with `If-None-Match: *` race against an empty key, exactly
        // one must succeed.
        {
            let comps = self.compositions.lock().await;
            if let Some(ns) = comps.namespace(req.namespace_id) {
                if ns.read_only {
                    tracing::warn!("gateway write: rejected — namespace is read-only");
                    return Err(GatewayError::ReadOnlyNamespace);
                }
            }
            if let (Some(name), Some(cond)) = (req.name.as_deref(), req.conditional.as_ref()) {
                let existing = comps.lookup_by_name(req.namespace_id, name).map_err(|e| {
                    tracing::warn!(error = %e, "gateway write: lookup_by_name failed");
                    GatewayError::Upstream(e.to_string())
                })?;
                match (cond, existing) {
                    (crate::ops::WriteConditional::IfNoneMatch, Some(_)) => {
                        tracing::debug!(
                            name = %name,
                            "gateway write: If-None-Match * but key exists → PreconditionFailed",
                        );
                        return Err(GatewayError::PreconditionFailed(format!(
                            "object \"{name}\" already exists; If-None-Match: * requires it not to"
                        )));
                    }
                    (crate::ops::WriteConditional::IfMatch(_), None) => {
                        tracing::debug!(
                            name = %name,
                            "gateway write: If-Match against missing key → PreconditionFailed",
                        );
                        return Err(GatewayError::PreconditionFailed(format!(
                            "object \"{name}\" does not exist; If-Match requires existing match"
                        )));
                    }
                    (crate::ops::WriteConditional::IfMatch(want), Some(got)) if *want != got => {
                        tracing::debug!(
                            name = %name,
                            want = %want.0,
                            got = %got.0,
                            "gateway write: If-Match etag mismatch → PreconditionFailed",
                        );
                        return Err(GatewayError::PreconditionFailed(format!(
                            "object \"{name}\" etag mismatch (have {}, want {})",
                            got.0, want.0,
                        )));
                    }
                    _ => {}
                }
            }
        }
        let bytes_written = req.data.len() as u64;
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(bytes_written, Ordering::Relaxed);
        if let Some(c) = self
            .chunk_write_bytes_metric
            .read()
            .lock_or_die("mem_gateway.chunk_write_bytes_metric")
            .as_ref()
        {
            c.inc_by(bytes_written);
        }

        // Bug 4 fix: split payloads larger than MAX_PLAINTEXT_PER_CHUNK
        // across multiple chunks. Smaller payloads (the common case)
        // get a single-chunk composition, identical to the pre-fix
        // shape. The 0-byte case yields a 1-chunk composition with an
        // empty-payload chunk (POSIX `touch` / NFSv4 OPEN-CREATE).
        let raw_pieces: Vec<&[u8]> = if req.data.is_empty() {
            vec![&req.data[..]]
        } else {
            req.data.chunks(MAX_PLAINTEXT_PER_CHUNK).collect()
        };
        let pieces_len = raw_pieces.len();

        let mut landed: Vec<ChunkLanded> = Vec::with_capacity(pieces_len);

        for piece in raw_pieces {
            let chunk_id = derive_chunk_id(
                piece,
                self.dedup_policy,
                self.tenant_hmac_key.as_deref(),
            )
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway write: derive_chunk_id failed");
                GatewayError::Upstream(e.to_string())
            })?;

            let env = envelope::seal_envelope(&self.aead, &self.master_key, &chunk_id, piece)
                .map_err(|e| {
                    tracing::warn!(?chunk_id, error = %e, "gateway write: seal_envelope failed");
                    GatewayError::Upstream(e.to_string())
                })?;
            let ciphertext_len = env.ciphertext.len() as u64;

            let piece_len = piece.len() as u64;
            // Inline path eligibility is per-chunk: a multi-chunk PUT
            // never goes inline (each chunk is large by definition).
            // Single-chunk PUTs ≤ inline_threshold still take the
            // fast path so small-object storage is unchanged.
            let mut chunk_was_new = false;
            if pieces_len == 1
                && piece_len <= self.inline_threshold
                && self.small_store.is_some()
            {
                tracing::debug!(
                    ?chunk_id,
                    inline_threshold = self.inline_threshold,
                    "gateway write: inline path",
                );
                let env_bytes = serde_json::to_vec(&env).map_err(|e| {
                    tracing::warn!(?chunk_id, error = %e, "gateway write: inline encode failed");
                    GatewayError::Upstream(e.to_string())
                })?;
                if let Some(ref store) = self.small_store {
                    store.put(&chunk_id.0, &env_bytes).map_err(|e| {
                        tracing::warn!(?chunk_id, error = %e, "gateway write: small_store.put failed");
                        GatewayError::Upstream(e.to_string())
                    })?;
                }
            } else {
                tracing::debug!(
                    ?chunk_id,
                    ciphertext_len,
                    "gateway write: chunk path → chunks.write_chunk",
                );
                let is_new = self.chunks.write_chunk(env, "default").await.map_err(|e| {
                    tracing::warn!(?chunk_id, error = %e, "gateway write: chunks.write_chunk failed");
                    GatewayError::Upstream(e.to_string())
                })?;
                if is_new {
                    tracing::debug!(?chunk_id, "gateway write: new chunk landed");
                    chunk_was_new = true;
                } else {
                    tracing::debug!(
                        ?chunk_id,
                        "gateway write: dedup hit — incrementing refcount"
                    );
                    let _ = self.chunks.increment_refcount(&chunk_id).await;
                }
            }

            landed.push(ChunkLanded {
                id: chunk_id,
                ciphertext_len,
                was_new: chunk_was_new,
            });
        }
        let chunk_ids: Vec<kiseki_common::ids::ChunkId> =
            landed.iter().map(|l| l.id).collect();

        // Create composition (sync, fast) — lock released before Raft.
        // Log emission happens after lock release to avoid holding the
        // Mutex across Raft consensus (ADR-032).
        let (comp_id, log, emit_params) = {
            let mut comps = self.compositions.lock().await;
            let comp_id = comps
                .create(req.namespace_id, chunk_ids.clone(), bytes_written)
                .map_err(|e| {
                    tracing::warn!(error = %e, "gateway write: compositions.create failed");
                    // Map the typed NamespaceNotFound through to the
                    // gateway's typed variant so the HTTP layer can
                    // return 404 NoSuchBucket instead of an opaque 500.
                    if matches!(
                        e,
                        kiseki_composition::error::CompositionError::NamespaceNotFound(_)
                    ) {
                        GatewayError::NamespaceNotFound(e.to_string())
                    } else {
                        GatewayError::Upstream(e.to_string())
                    }
                })?;
            // Bind the name to the new composition_id (S3 PUT URL key).
            // The conditional check above (or its absence) means the
            // bind is unconditional here — overwrite-replace semantics.
            // The Raft delta below carries the name to followers via
            // the v2 create payload so their hydrators stay consistent.
            if let Some(name) = req.name.as_deref() {
                comps
                    .bind_name(req.namespace_id, name.to_owned(), comp_id)
                    .map_err(|e| {
                        tracing::warn!(error = %e, "gateway write: bind_name failed");
                        GatewayError::Upstream(e.to_string())
                    })?;
            }
            let comp = comps
                .get(comp_id)
                .expect("composition was just created above; must be present");
            let params = (
                comp.shard_id,
                comp.tenant_id,
                comp.namespace_id,
                comp.chunks.clone(),
            );
            let log = comps.log().cloned();
            (comp_id, log, params)
        }; // Lock dropped here — before Raft consensus.
        tracing::debug!(
            comp_id = %comp_id.0,
            shard_id = %emit_params.0.0,
            "gateway write: composition created (pre-Raft)",
        );

        // Emit delta to log (async, slow — Raft consensus).
        if let Some(ref log) = log {
            let hashed_key = kiseki_composition::composition_hash_key(emit_params.2, comp_id);

            // ADR-033: route to correct shard via shard map if available.
            let shard_id = if let Some(ref shard_map) = *self.shard_map.read().lock_or_die("mem_gateway.unknown") {
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
            let new_chunks: Vec<kiseki_log::raft_store::NewChunkMeta> = landed
                .iter()
                .filter(|l| l.was_new)
                .map(|l| {
                    // Phase 16c step 2: when target_copies is set, pick
                    // exactly that many nodes via rendezvous hashing. When
                    // 0 (the 16b posture) carry the whole cluster set.
                    let placement = if self.target_copies > 0 {
                        kiseki_chunk_cluster::pick_placement(
                            &l.id,
                            &self.cluster_placement,
                            self.target_copies,
                        )
                    } else {
                        self.cluster_placement.clone()
                    };
                    kiseki_log::raft_store::NewChunkMeta {
                        chunk_id: l.id.0,
                        placement,
                        original_len: l.ciphertext_len,
                    }
                })
                .collect();

            // Phase 16f: payload encodes (comp_id, namespace_id, size) so
            // followers can hydrate their CompositionStore from the log.
            // S3 per-key naming extends the payload (v2 length-dispatched
            // form) with the optional name so followers can replay name
            // bindings; legacy v1 callers (NFS, internal) keep the 40-
            // byte payload and the hydrator's name index stays untouched.
            let comp_payload = kiseki_composition::encode_composition_create_payload_named(
                comp_id,
                emit_params.2,
                bytes_written,
                req.name.as_deref(),
            );
            tracing::debug!(
                comp_id = %comp_id.0,
                shard_id = %shard_id.0,
                new_chunks = new_chunks.len(),
                "gateway write: emit_chunk_and_delta start",
            );
            match kiseki_composition::log_bridge::emit_chunk_and_delta(
                log.as_ref(),
                shard_id,
                emit_params.1,
                kiseki_log::delta::OperationType::Create,
                hashed_key,
                emit_params.3,
                comp_payload,
                new_chunks,
            )
            .await
            {
                Ok(seq) => {
                    tracing::debug!(
                        comp_id = %comp_id.0,
                        shard_id = %shard_id.0,
                        seq = ?seq,
                        "gateway write: emit_chunk_and_delta committed",
                    );
                }
                Err(kiseki_log::error::LogError::KeyOutOfRange(sid)) => {
                    tracing::warn!(
                        comp_id = %comp_id.0,
                        shard_id = %sid.0,
                        "gateway write: KeyOutOfRange — rolling back composition",
                    );
                    let _ = self.compositions.lock().await.delete(comp_id).ok();
                    return Err(GatewayError::KeyOutOfRange { shard_id: sid });
                }
                Err(e) => {
                    // Rollback: re-acquire lock and remove (PIPE-ADV-1).
                    tracing::warn!(
                        comp_id = %comp_id.0,
                        shard_id = %shard_id.0,
                        error = %e,
                        "gateway write: emit_chunk_and_delta failed — rolling back composition",
                    );
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

        tracing::debug!(comp_id = %comp_id.0, bytes_written, "gateway write: success");
        Ok(WriteResponse {
            composition_id: comp_id,
            bytes_written,
        })
    }

    #[tracing::instrument(
        skip(self),
        fields(tenant_id = %tenant_id.0, namespace_id = %namespace_id.0),
    )]
    async fn list(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<Vec<(kiseki_common::ids::CompositionId, u64)>, GatewayError> {
        // Filter by tenant_id to prevent cross-tenant composition ID leak.
        let compositions = self.compositions.lock().await;
        let entries: Vec<(kiseki_common::ids::CompositionId, u64)> = compositions
            .list_by_namespace(namespace_id)
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway list: list_by_namespace failed");
                GatewayError::Upstream(e.to_string())
            })?
            .into_iter()
            .filter(|c| c.tenant_id == tenant_id)
            .map(|c| (c.id, c.size))
            .collect();
        tracing::debug!(returned = entries.len(), "gateway list: success");
        Ok(entries)
    }

    #[tracing::instrument(skip(self), fields(namespace_id = %namespace_id.0))]
    async fn start_multipart(
        &self,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<String, GatewayError> {
        let upload_id = self.start_multipart_internal(namespace_id).await?;
        tracing::debug!(%upload_id, "gateway start_multipart: success");
        Ok(upload_id)
    }

    #[tracing::instrument(skip(self, data), fields(upload_id = upload_id, part_number, bytes = data.len()))]
    async fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<String, GatewayError> {
        let chunk_id = self
            .upload_part_internal(upload_id, part_number, data)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway upload_part: upload_part_internal failed");
                e
            })?;
        let mut hex = String::with_capacity(64);
        for b in &chunk_id.0 {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        tracing::debug!(?chunk_id, "gateway upload_part: success");
        Ok(hex)
    }

    #[tracing::instrument(skip(self), fields(upload_id = upload_id, has_name = name.is_some()))]
    async fn complete_multipart(
        &self,
        upload_id: &str,
        name: Option<&str>,
    ) -> Result<kiseki_common::ids::CompositionId, GatewayError> {
        let comp_id = self
            .complete_multipart_internal(upload_id, name)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway complete_multipart: failed");
                e
            })?;
        tracing::debug!(comp_id = %comp_id.0, "gateway complete_multipart: success");
        Ok(comp_id)
    }

    #[tracing::instrument(skip(self), fields(upload_id = upload_id))]
    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        self.abort_multipart_internal(upload_id)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway abort_multipart: failed");
                e
            })?;
        tracing::debug!("gateway abort_multipart: success");
        Ok(())
    }

    #[tracing::instrument(
        skip(self),
        fields(tenant_id = %tenant_id.0, namespace_id = %namespace_id.0),
    )]
    async fn ensure_namespace(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<(), GatewayError> {
        self.ensure_namespace_exists(tenant_id, namespace_id).await
    }

    #[tracing::instrument(
        skip(self),
        fields(
            tenant_id = %tenant_id.0,
            namespace_id = %_namespace_id.0,
            composition_id = %composition_id.0,
        ),
    )]
    async fn delete(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        _namespace_id: kiseki_common::ids::NamespaceId,
        composition_id: kiseki_common::ids::CompositionId,
    ) -> Result<(), GatewayError> {
        // Phase 17 item 1: emit a Delete delta to the Raft log so
        // followers' composition hydrators can apply `delete_at` and
        // remove the composition from their local stores. Without
        // this, an S3 DELETE on the leader was invisible to followers
        // (the gateway's `compositions` HashMap is per-node).
        //
        // Lock discipline: hold the compositions lock across (1) the
        // tenant check, (2) the delta emit, and (3) the local delete.
        // The hydrator polls the same Arc<Mutex<...>> so without
        // lock-spanning we could race: hydrator applies `delete_at`
        // (composition gone), gateway's `compositions.delete(...)`
        // then errors with `CompositionNotFound`, S3 sees a 5xx for
        // a delete that actually succeeded. tokio::sync::Mutex lets
        // us hold across the emit await; release before chunk-
        // refcount Raft work since that's a separate transaction.
        tracing::debug!("gateway delete: entry");
        let mut compositions = self.compositions.lock().await;
        let (shard_id, namespace_id, log) = {
            let comp = compositions.get(composition_id).map_err(|e| {
                tracing::warn!(error = %e, "gateway delete: compositions.get failed");
                GatewayError::Upstream(e.to_string())
            })?;
            if comp.tenant_id != tenant_id {
                tracing::warn!(
                    expected_tenant = %tenant_id.0,
                    actual_tenant = %comp.tenant_id.0,
                    "gateway delete: tenant mismatch",
                );
                return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
            }
            (
                comp.shard_id,
                comp.namespace_id,
                compositions.log().cloned(),
            )
        };

        // Emit the Delete tombstone if a log is attached. If the emit
        // fails, the local store is left intact so the operation is
        // re-tryable from the same client. Multi-node clusters depend
        // on this — without the delta, followers retain stale
        // compositions until the next operator-driven full re-sync.
        if let Some(ref log) = log {
            let hashed_key = kiseki_composition::composition_hash_key(namespace_id, composition_id);
            // Resolve the actual write-shard the same way as the
            // Create path (ADR-033 multi-shard routing).
            let routed_shard = if let Some(ref shard_map) = *self.shard_map.read().lock_or_die("mem_gateway.unknown") {
                let ns_str = namespace_id.0.to_string();
                if let Ok(map) = shard_map.get(&ns_str, tenant_id) {
                    kiseki_control::shard_topology::route_to_shard(&map, &hashed_key)
                        .unwrap_or(shard_id)
                } else {
                    shard_id
                }
            } else {
                shard_id
            };
            let payload = kiseki_composition::encode_composition_delete_payload(composition_id);
            tracing::debug!(
                shard_id = %routed_shard.0,
                "gateway delete: emit_chunk_and_delta(Delete) start",
            );
            kiseki_composition::log_bridge::emit_chunk_and_delta(
                log.as_ref(),
                routed_shard,
                tenant_id,
                kiseki_log::delta::OperationType::Delete,
                hashed_key,
                Vec::new(),
                payload,
                Vec::new(),
            )
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway delete: delete delta emit failed");
                GatewayError::Upstream(format!("delete delta emit: {e}"))
            })?;
            tracing::debug!("gateway delete: delete delta committed");
        }

        // Local delete only after the cluster has the tombstone.
        let delete_result = compositions.delete(composition_id).map_err(|e| {
            tracing::warn!(error = %e, "gateway delete: local compositions.delete failed");
            GatewayError::Upstream(e.to_string())
        })?;
        drop(compositions); // release lock before chunk-refcount Raft work below

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
                    // Phase 16c: read the tombstone signal from the
                    // Raft state machine. `true` means
                    // `cluster_chunk_state[(tenant, chunk_id)]`
                    // transitioned refcount → 0; the leader fans
                    // `DeleteFragment` out to the placement list so
                    // every peer's local store can reclaim. `false`
                    // means another composition still references the
                    // chunk — leave it alone.
                    let tombstoned = log
                        .decrement_chunk_refcount(shard_id, tenant_id, *chunk_id)
                        .await
                        .unwrap_or(false);
                    if tombstoned {
                        let _ = self.chunks.delete_distributed(chunk_id, tenant_id).await;
                    }
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(
        skip(self),
        fields(
            tenant_id = %tenant_id.0,
            namespace_id = %namespace_id.0,
            name,
            composition_id = %composition_id.0,
        ),
    )]
    async fn bind_object_name(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
        name: &str,
        composition_id: kiseki_common::ids::CompositionId,
    ) -> Result<(), GatewayError> {
        // Tenant guard mirrors lookup: refuse to bind a name when
        // the composition belongs to a different tenant. Without
        // this a misrouted CompleteMultipartUpload could let one
        // tenant rebind another tenant's composition under its own
        // bucket — defense in depth even if S3 routing is correct.
        let mut comps = self.compositions.lock().await;
        if let Ok(comp) = comps.get(composition_id) {
            if comp.tenant_id != tenant_id {
                tracing::warn!(
                    expected_tenant = %tenant_id.0,
                    actual_tenant = %comp.tenant_id.0,
                    "gateway bind_object_name: tenant mismatch — refusing",
                );
                return Err(GatewayError::AuthenticationFailed("tenant mismatch".into()));
            }
        }
        comps
            .bind_name(namespace_id, name.to_owned(), composition_id)
            .map_err(|e| {
                tracing::warn!(error = %e, "gateway bind_object_name: bind_name failed");
                GatewayError::Upstream(e.to_string())
            })?;
        tracing::debug!("gateway bind_object_name: bound");
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(tenant_id = %tenant_id.0, namespace_id = %namespace_id.0, name))]
    async fn lookup_object_by_name(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
        name: &str,
    ) -> Result<Option<kiseki_common::ids::CompositionId>, GatewayError> {
        let comps = self.compositions.lock().await;
        let id = comps.lookup_by_name(namespace_id, name).map_err(|e| {
            tracing::warn!(error = %e, "gateway lookup_object_by_name: lookup_by_name failed");
            GatewayError::Upstream(e.to_string())
        })?;
        // Tenant ownership check: a composition's tenant must match
        // the requesting tenant. Without this, two tenants who happen
        // to use the same namespace_id (administrative misconfiguration)
        // would see each other's keys.
        if let Some(comp_id) = id {
            if let Ok(comp) = comps.get(comp_id) {
                if comp.tenant_id != tenant_id {
                    tracing::warn!(
                        expected_tenant = %tenant_id.0,
                        actual_tenant = %comp.tenant_id.0,
                        "gateway lookup_object_by_name: tenant mismatch — returning None",
                    );
                    return Ok(None);
                }
            }
        }
        tracing::debug!(found = id.is_some(), "gateway lookup_object_by_name: done");
        Ok(id)
    }

    async fn delete_by_name(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
        name: &str,
    ) -> Result<bool, GatewayError> {
        // Resolve the name to a composition_id, then route through the
        // standard delete path so chunk-refcount + Raft Delete delta
        // are emitted. The hydrator will drop the name binding via
        // the reverse-lookup in stage_delete on every follower; we
        // also drop it locally here for the leader.
        let Some(comp_id) = self
            .lookup_object_by_name(tenant_id, namespace_id, name)
            .await?
        else {
            tracing::debug!(name = %name, "gateway delete_by_name: name not bound");
            return Ok(false);
        };
        self.delete(tenant_id, namespace_id, comp_id).await?;
        // The standard delete path's storage.remove drops the name
        // binding via the same reverse-index logic, but be explicit
        // here in case a future code path takes a different exit.
        Ok(true)
    }

    #[tracing::instrument(skip(self, prefix), fields(tenant_id = %tenant_id.0, namespace_id = %namespace_id.0))]
    async fn list_named(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, kiseki_common::ids::CompositionId, u64)>, GatewayError> {
        let comps = self.compositions.lock().await;
        let pairs = comps.list_names(namespace_id, prefix).map_err(|e| {
            tracing::warn!(error = %e, "gateway list_named: list_names failed");
            GatewayError::Upstream(e.to_string())
        })?;
        let mut out = Vec::with_capacity(pairs.len());
        for (name, comp_id) in pairs {
            // Pull the composition for size + tenant filter.
            if let Ok(comp) = comps.get(comp_id) {
                if comp.tenant_id == tenant_id {
                    out.push((name, comp_id, comp.size));
                }
            }
        }
        tracing::debug!(returned = out.len(), "gateway list_named: done");
        Ok(out)
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

/// Phase 17 ADR-040 §D7 + §D6.3 / I-2 closure tests (auditor finding A1).
///
/// Verifies that a composition lookup which misses while the local
/// persistent hydrator is in halt mode returns
/// `GatewayError::ServiceUnavailable` immediately, without waiting
/// out the retry budget. The S3-side mapping to HTTP 503 +
/// `Retry-After` is tested in `s3_server.rs`.
#[cfg(test)]
mod halt_mode_tests {
    use super::*;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::persistent::{CompositionStorage, HydrationBatch, MemoryStorage};
    use kiseki_crypto::keys::SystemMasterKey;

    /// Serializes the four tests that mutate `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS`.
    /// Tests in a single binary run in parallel by default, and env vars are
    /// process-shared, so without this lock one test's `set_var` can leak into
    /// another's read. Held for the full body (across `gw.read(...).await`) so
    /// no other env-test observes the in-flight value — async-aware so clippy's
    /// `await_holding_lock` lint stays happy.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Build a `CompositionStore` whose storage has `halted = true`.
    fn make_halted_store() -> CompositionStore {
        let mut storage = MemoryStorage::new();
        storage
            .apply_hydration_batch(HydrationBatch {
                puts: Vec::new(),
                removes: Vec::new(),
                name_inserts: Vec::new(),
                name_removes: Vec::new(),
                new_last_applied_seq: kiseki_common::ids::SequenceNumber(0),
                stuck_state: Some(None),
                halted: Some(true),
            })
            .expect("apply halt batch");
        CompositionStore::with_storage(Box::new(storage))
    }

    #[tokio::test]
    async fn read_returns_service_unavailable_when_storage_halted() {
        let gw = InMemoryGateway::new(
            make_halted_store(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );

        let req = ReadRequest {
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            composition_id: CompositionId(uuid::Uuid::new_v4()),
            offset: 0,
            length: u64::MAX,
        };

        let started = std::time::Instant::now();
        let result = gw.read(req).await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(GatewayError::ServiceUnavailable(_))),
            "expected ServiceUnavailable, got {result:?}"
        );
        // Halt-mode short-circuit must NOT wait out the 1s budget.
        // Allow generous slack for CI; the retry interval is 25 ms.
        assert!(
            elapsed.as_millis() < 100,
            "halt-mode read took {elapsed:?} — should short-circuit",
        );
    }

    #[tokio::test]
    async fn read_does_not_short_circuit_when_storage_not_halted() {
        // Sanity: the same path on a healthy (non-halted) store
        // surfaces `Upstream(...)` after the budget expires, NOT
        // `ServiceUnavailable`. The halt-mode branch must be
        // gated on the flag, not on "composition missing."
        let _env_guard = ENV_LOCK.lock().await;
        let gw = InMemoryGateway::new(
            CompositionStore::new(), // fresh, halted=false by default
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );
        // Tighten the retry budget so the test runs fast.
        std::env::set_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS", "50");

        let req = ReadRequest {
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            composition_id: CompositionId(uuid::Uuid::new_v4()),
            offset: 0,
            length: u64::MAX,
        };
        let result = gw.read(req).await;
        std::env::remove_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS");
        assert!(
            matches!(result, Err(GatewayError::Upstream(_))),
            "expected Upstream(NotFound), got {result:?}"
        );
    }

    /// Auditor finding A6: `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS`
    /// env-parsing — verify default + override + malformed-input
    /// fallback behavior.
    #[tokio::test]
    async fn retry_budget_env_override_is_honored() {
        let _env_guard = ENV_LOCK.lock().await;
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );

        // Override to a tight budget. The miss should surface as
        // Upstream(NotFound) within ≈ budget, not the 1 s default.
        std::env::set_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS", "75");
        let req = ReadRequest {
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            composition_id: CompositionId(uuid::Uuid::new_v4()),
            offset: 0,
            length: u64::MAX,
        };
        let started = std::time::Instant::now();
        let result = gw.read(req).await;
        let elapsed = started.elapsed();
        std::env::remove_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS");

        assert!(matches!(result, Err(GatewayError::Upstream(_))));
        // Allow generous slack (CI variance, mutex acquire), but
        // strictly less than the 1 s default — otherwise the env
        // override didn't take effect.
        assert!(
            elapsed.as_millis() < 500,
            "override-budget read took {elapsed:?} — env var not honored",
        );
    }

    #[tokio::test]
    async fn retry_budget_env_malformed_falls_back_to_default() {
        // Garbage input must not panic; it falls back to the 1 s
        // default. We verify by setting it to non-numeric and
        // observing the read takes ≈ 1 s.
        let _env_guard = ENV_LOCK.lock().await;
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        );

        std::env::set_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS", "not-a-number");
        let req = ReadRequest {
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            composition_id: CompositionId(uuid::Uuid::new_v4()),
            offset: 0,
            length: u64::MAX,
        };
        let started = std::time::Instant::now();
        let _ = gw.read(req).await;
        let elapsed = started.elapsed();
        std::env::remove_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS");

        // ≥ ~900 ms (default 1000 ms minus scheduling slop).
        assert!(
            elapsed.as_millis() >= 900,
            "malformed env should have fallen back to 1 s default, got {elapsed:?}",
        );
    }

    #[tokio::test]
    async fn read_retry_metrics_increment_on_exhausted_budget() {
        // Auditor finding A5 — verify the `_exhausted_total` counter
        // bumps when the budget runs out. Closes F-4 (configurable +
        // observable retry budget).
        use prometheus::Registry;
        let _env_guard = ENV_LOCK.lock().await;
        let registry = Registry::new();
        let metrics = std::sync::Arc::new(
            crate::metrics::GatewayRetryMetrics::register(&registry)
                .expect("register retry metrics"),
        );

        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0; 32], KeyEpoch(1)),
        )
        .with_retry_metrics(std::sync::Arc::clone(&metrics));

        std::env::set_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS", "30");
        let req = ReadRequest {
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            namespace_id: NamespaceId(uuid::Uuid::from_u128(2)),
            composition_id: CompositionId(uuid::Uuid::new_v4()),
            offset: 0,
            length: u64::MAX,
        };
        let _ = gw.read(req).await;
        std::env::remove_var("KISEKI_GATEWAY_READ_RETRY_BUDGET_MS");

        assert_eq!(
            metrics.read_retry_exhausted_total.get(),
            1,
            "exhausted counter must bump when budget runs out",
        );
        assert_eq!(metrics.read_retry_total.get(), 0, "no successful retry hit");
    }
}

/// Bug 4 (GCP 2026-05-04): the gateway emitted exactly one envelope
/// per S3 PUT, regardless of size. Combined with Bug 1's fabric
/// envelope cap (256 MiB ciphertext), any PUT > ~256 MB returned 500
/// with the h2 protocol error / "quorum lost" symptom.
///
/// The fix is a chunking policy at the gateway: PUTs larger than
/// `MAX_PLAINTEXT_PER_CHUNK` are split into N chunks, each sealed
/// with its own envelope, and the resulting composition references
/// all of them in order.
#[cfg(test)]
mod chunking_tests {
    use super::*;
    use crate::ops::WriteRequest;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    async fn build_gateway() -> (InMemoryGateway, OrgId, NamespaceId) {
        let tenant = OrgId(uuid::Uuid::from_u128(900));
        let namespace = NamespaceId(uuid::Uuid::from_u128(901));
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
        );
        gw.ensure_namespace_exists(tenant, namespace).await.unwrap();
        (gw, tenant, namespace)
    }

    /// Composition for a 384 MiB write must reference at least 2 chunks.
    /// Today this fails: the gateway derives a single `chunk_id` over
    /// the entire payload and creates a 1-chunk composition. With the
    /// Bug 1 fabric cap in place, any subsequent fabric round-trip
    /// (replication, cross-node read) of that single chunk fails.
    #[tokio::test]
    async fn write_above_chunk_cap_yields_multi_chunk_composition() {
        let (gw, tenant, namespace) = build_gateway().await;

        // Use a deterministic payload pattern so dedup never collapses
        // separate chunks into one (different bytes per chunk position).
        let mut data = vec![0u8; 384 * 1024 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap();
        }
        let resp = gw
            .write(WriteRequest {
                tenant_id: tenant,
                namespace_id: namespace,
                data,
                name: Some("big-object".to_owned()),
                conditional: None,
                workflow_ref: None,
            })
            .await
            .expect("write must succeed");

        let comps = gw.compositions.lock().await;
        let comp = comps.get(resp.composition_id).expect("composition exists");
        assert!(
            comp.chunks.len() >= 2,
            "384 MiB PUT produced a single-chunk composition; gateway is \
             not chunking large payloads. chunks.len()={}",
            comp.chunks.len(),
        );
    }

    /// Bug 6 (GCP 2026-05-04): NFS READ measured exactly 5.12 s
    /// per call regardless of object size, capping NFS read
    /// throughput at <1 MB/s. The fixed timing strongly implies a
    /// hidden ~5 s waiter on the read path.
    ///
    /// This test bounds the time of `MemGateway::read` against an
    /// in-memory backend where every dependency is fast: the chunk
    /// store is in-process, no fabric, no real I/O. If the call
    /// exceeds 200 ms there's a hidden timer/sleep on the gateway
    /// read path. If it returns fast, the bug is upstream of the
    /// gateway (fabric retry budget, NFS dispatch overhead, kernel-
    /// side retransmit) and must be hunted separately.
    #[tokio::test]
    async fn read_completes_under_200ms_on_fast_in_memory_backend() {
        let (gw, tenant, namespace) = build_gateway().await;

        // 4 MiB object — within a single chunk; same shape as the GCP
        // measurement (`4 MB read: 5.12 s`).
        let mut data = vec![0u8; 4 * 1024 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap();
        }
        let resp = gw
            .write(WriteRequest {
                tenant_id: tenant,
                namespace_id: namespace,
                data: data.clone(),
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .unwrap();

        let start = std::time::Instant::now();
        let read = gw
            .read(crate::ops::ReadRequest {
                tenant_id: tenant,
                namespace_id: namespace,
                composition_id: resp.composition_id,
                offset: 0,
                length: data.len() as u64,
            })
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(read.data.len(), data.len());
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "gateway read of 4 MiB took {elapsed:?}; expected <200 ms on \
             a fast in-memory backend. If this fails the 5 s NFS floor \
             is on the gateway read path; if it passes the floor must \
             be upstream (chunk-cluster fabric or NFS dispatch).",
        );
    }

    /// A multi-chunk composition must round-trip through `read` with
    /// the same plaintext as the original PUT.
    #[tokio::test]
    async fn write_then_read_round_trips_multi_chunk_composition() {
        let (gw, tenant, namespace) = build_gateway().await;

        let mut data = vec![0u8; 96 * 1024 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = u8::try_from((i * 17 + 3) % 251).unwrap();
        }
        let original = data.clone();
        let resp = gw
            .write(WriteRequest {
                tenant_id: tenant,
                namespace_id: namespace,
                data,
                name: None,
                conditional: None,
                workflow_ref: None,
            })
            .await
            .unwrap();

        let read = gw
            .read(crate::ops::ReadRequest {
                tenant_id: tenant,
                namespace_id: namespace,
                composition_id: resp.composition_id,
                offset: 0,
                length: original.len() as u64,
            })
            .await
            .unwrap();

        assert_eq!(
            read.data.len(),
            original.len(),
            "round-trip length mismatch",
        );
        assert_eq!(read.data, original, "round-trip bytes diverged");
    }
}
