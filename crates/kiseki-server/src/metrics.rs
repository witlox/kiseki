//! Prometheus metrics registry and HTTP endpoint.
//!
//! Exposes `/metrics` (Prometheus text format) and `/health` (200 OK)
//! on a dedicated HTTP port (default 9090).

use std::net::SocketAddr;

use axum::routing::get;
use axum::Router;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};

/// Application-wide metrics registry.
///
/// Created once at server boot and shared via `Arc` across all
/// subsystems. Each subsystem records into its own metrics.
#[derive(Clone)]
#[allow(dead_code)] // Fields used incrementally as subsystems wire in metrics
pub struct KisekiMetrics {
    registry: Registry,

    // --- Raft ---
    /// Raft commit latency in seconds.
    pub raft_commit_latency: HistogramVec,
    /// Total Raft entries applied.
    pub raft_entries_total: IntCounter,

    // --- Chunk ---
    /// Chunk bytes written.
    pub chunk_write_bytes: IntCounter,
    /// Chunk bytes read.
    pub chunk_read_bytes: IntCounter,
    /// EC encode latency in seconds.
    pub chunk_ec_encode_latency: HistogramVec,
    /// `PersistentChunkStore::write_chunk` per-phase latency. Phases:
    /// `dedup_check` (`HashMap` lookup + refcount path), `extent_io`
    /// (alloc + `device.write` per extent — includes per-extent CRC),
    /// `save_meta` (full-state JSON rewrite + atomic rename),
    /// `device_sync` (`flush_bitmap` + `sync_all`). Wired into
    /// `PersistentChunkStore::set_write_phase_metric` from runtime.
    /// The 2026-05-04 perf sweep removed the bit-loop CRC bottleneck
    /// (~36 ms → ~3 ms on 16 MiB) but left ~14 ms unaccounted for in
    /// the receiver-side `write_chunk`; this histogram splits that.
    pub chunk_persistent_write_phase_duration: HistogramVec,

    // --- Gateway ---
    /// S3/NFS request count by method and status.
    pub gateway_requests_total: IntCounterVec,
    /// Gateway request duration in seconds.
    pub gateway_request_duration: HistogramVec,
    /// GET-path phase latency histogram. Labels:
    /// `phase` ∈ {`composition_lookup`, `chunk_fetch`, `decrypt`}.
    /// Wired into `InMemoryGateway` via `set_phase_duration_metrics`
    /// so each GET surfaces where its time actually goes — separating
    /// metadata-lookup latency, chunk-store fetch (local + cluster
    /// fabric) latency, and per-chunk decrypt cost. Buckets cover
    /// 100 µs — 5 s, the range observed across local/distributed runs.
    pub gateway_get_phase_duration: HistogramVec,
    /// PUT-path phase latency histogram. Labels:
    /// `phase` ∈ {`encrypt`, `chunk_write`, `composition_record`}.
    /// `composition_record` covers the `CompositionStore::create` +
    /// `bind_name` + Raft-replicated delta emission round trip.
    pub gateway_put_phase_duration: HistogramVec,
    /// `x-kiseki-workflow-ref` header validation outcome counter.
    /// Labels: `result` ∈ {`absent`, `valid`, `invalid`}. Wired in
    /// `runtime.rs` to scrape from
    /// `InMemoryGateway::workflow_ref_writes_total()` on each
    /// metrics gather; ADR-021 / I-WA1 makes the header advisory so
    /// `invalid` writes still succeed — operators use this counter
    /// to spot misconfigured clients.
    pub gateway_workflow_ref_writes_total: IntCounterVec,
    /// ADR-008 rev 2 / ADR-042 §4 — stale-leader redirects emitted on
    /// the protocol boundary. Labels: `protocol` ∈ {`s3`, `native`}.
    /// Incremented every time the gateway returns a leader hint
    /// (307 for S3, NotLeader/ForwardToLeader for native) because
    /// the caller's request arrived at a non-leader. Alarm at
    /// sustained > 20 % of total writes for any tenant.
    pub stale_leader_redirects_total: IntCounterVec,

    /// ADR-042 §4 — count of writes the native server forwarded to a
    /// peer leader via the in-process proxy path. Labels:
    /// `source_node` (this node's id) and `leader_node` (target
    /// leader's id). Sustained share > 20% of total writes is the
    /// operator alarm declared in ADR-042 §4 §"Consequences" (stale
    /// client topology cache or unstable leadership).
    pub native_proxy_forwards_total: IntCounterVec,

    // --- Pool ---
    /// Pool capacity bytes (total).
    pub pool_capacity_total: IntGaugeVec,
    /// Pool capacity bytes (used).
    pub pool_capacity_used: IntGaugeVec,
    /// Per-device pool capacity gauge with labels
    /// `{pool, device_id, kind}` where `kind` is one of
    /// `total` / `used` / `free` (2026-05-15 follow-ups doc D2).
    pub pool_device_capacity_bytes: IntGaugeVec,
    /// Per-device IO error counter with labels `{device_id, op}`
    /// where `op` is one of `read` / `write`. Cheap signal that
    /// surfaces media trouble before a full health probe runs.
    pub pool_device_errors_total: IntCounterVec,

    // --- Node storage (GH #115 chunk-store capacity + dedup) ---
    // Node-level, unlabeled so the cluster aggregator can sum them
    // directly. Refreshed periodically from
    // `AsyncChunkOps::storage_stats()`.
    /// Physical bytes used on this node's chunk-store device pool.
    pub storage_device_used_bytes: IntGauge,
    /// Total physical capacity of this node's chunk-store device pool.
    pub storage_device_total_bytes: IntGauge,
    /// Logical bytes addressed by clients (`sum(refcount × payload)`).
    pub storage_logical_bytes: IntGauge,
    /// Unique stored payload bytes (each content-addressed chunk once).
    pub storage_physical_bytes: IntGauge,
    /// Unique chunk count held locally.
    pub storage_chunk_count: IntGauge,
    /// Metadata tier on-disk bytes (`KISEKI_DATA_DIR/metadata` —
    /// compositions/views; ADR-030 "meta" tier on the system disk).
    pub storage_meta_bytes: IntGauge,
    /// Small-object tier on-disk bytes (`KISEKI_DATA_DIR/small` —
    /// ADR-030 inline content tier).
    pub storage_small_bytes: IntGauge,
    /// Chunk-pool capacity split by cost/performance tier (ADR-024):
    /// `{used, total}` for fast (`NVMe`), bulk (`SSD`), cold (`HDD`). The
    /// cluster aggregator sums these so `kiseki-admin capacity` can show
    /// per-class capacity across the heterogeneous fleet.
    pub storage_tier_fast_used: IntGauge,
    /// Fast-tier (`NVMe`) total capacity, this node.
    pub storage_tier_fast_total: IntGauge,
    /// Bulk-tier (`SSD`) used bytes, this node.
    pub storage_tier_bulk_used: IntGauge,
    /// Bulk-tier (`SSD`) total capacity, this node.
    pub storage_tier_bulk_total: IntGauge,
    /// Cold-tier (`HDD`) used bytes, this node.
    pub storage_tier_cold_used: IntGauge,
    /// Cold-tier (`HDD`) total capacity, this node.
    pub storage_tier_cold_total: IntGauge,

    // --- Transport ---
    /// Active transport connections.
    pub transport_connections_active: IntGauge,
    /// Idle transport connections.
    pub transport_connections_idle: IntGauge,

    // --- Shard ---
    /// Delta count per shard.
    pub shard_delta_count: IntGaugeVec,

    // --- Key management ---
    /// Key rotation count.
    pub key_rotation_total: IntCounter,
    /// Crypto-shred count.
    pub crypto_shred_total: IntCounter,

    // --- Cluster fabric (Phase 16a) ---
    /// Cross-node chunk fabric metrics. Wired into
    /// `ClusteredChunkStore` and `GrpcFabricPeer` via
    /// `with_metrics(...)` at runtime construction.
    pub fabric: std::sync::Arc<kiseki_chunk_cluster::FabricMetrics>,

    // --- Multiplexed Raft RPC transport (ADR-041) ---
    /// 8 metrics covering per-shard RPC count + duration + outcomes,
    /// registry size, listener restarts, dispatcher panics, per-peer
    /// connection cap, active connections. Wired into the per-node
    /// `RaftRpcListener` via `with_metrics(...)`.
    pub raft_transport: std::sync::Arc<kiseki_raft::transport_metrics::RaftTransportMetrics>,

    // --- Control-plane Raft (ADR-033 §4) ---
    /// 6 metrics covering control-plane submit count + duration,
    /// apply count, apply-hook duration, namespace gauge, and
    /// leader-forwarded admin RPCs. Wired into
    /// `OpenRaftControlStore` and the storage-admin forwarding
    /// path at runtime construction.
    pub cluster_control: std::sync::Arc<crate::cluster_control::ClusterControlMetrics>,

    // --- Log context (ADR-032) ---
    /// 9 metrics: per-shard append/read/compaction count + duration
    /// histograms, watermark advance count, truncate boundary
    /// gauge, total deltas removed by compaction. Wired into
    /// `RaftShardStore` / `MemShardStore` / `PersistentShardStore`
    /// via `with_metrics(...)` at runtime construction.
    pub log: std::sync::Arc<kiseki_log::LogMetrics>,

    // --- Key manager (security-critical) ---
    /// 5 metrics: rotation count, current epoch gauge, epoch count
    /// gauge, fetch duration histogram, migration-complete count.
    /// Wired into the production key store via
    /// `InstrumentedKeyManager` at runtime construction.
    pub keymanager: std::sync::Arc<kiseki_keymanager::KeyManagerMetrics>,

    // --- Control plane (ADR-027 + ADR-033) ---
    /// 5 metrics: tenant ops + namespace creates + active gauge,
    /// ratio-floor evaluations, alias counter. Owned by the
    /// control crate; consumers (`storage_admin` handlers, ratio
    /// floor evaluator) record directly.
    pub control: std::sync::Arc<kiseki_control::metrics::ControlMetrics>,

    // --- View context (NFS materialization) ---
    /// 5 metrics: versions added/deleted, objects + total versions
    /// gauges, per-shard delta poll counter. Owned by the view
    /// crate; the runtime hands the `Arc` to the `StreamProcessor`
    /// poll loop and the version-store mutators.
    pub view: std::sync::Arc<kiseki_view::metrics::ViewMetrics>,

    // --- Block storage backend ---
    /// 6 metrics: per-device read/write IOP count + byte count +
    /// latency histograms. Wired into `DeviceBackend` impls (when
    /// they record on each `read`/`write` call) — concrete-store
    /// instrumentation lands incrementally.
    pub block: std::sync::Arc<kiseki_block::BlockMetrics>,

    // --- Gateway retry budget (ADR-040 §D7 + §D10 — F-4 closure) ---
    /// Read-path retry counters. Wired into `InMemoryGateway` via
    /// `with_retry_metrics(...)` at runtime construction.
    pub gateway_retry: std::sync::Arc<kiseki_gateway::metrics::GatewayRetryMetrics>,

    // --- Composition persistent store + hydrator (ADR-040 §D10) ---
    /// Metrics covering store on-disk size, hydrator apply duration,
    /// last-applied-seq per shard, skip counter, halt flag, decode
    /// and commit error counters. Cloned into the active
    /// `CompositionStorage` backend (`with_metrics()`) and into
    /// `CompositionHydrator::with_metrics()` at runtime construction.
    pub composition: std::sync::Arc<kiseki_composition::metrics::CompositionMetrics>,

    // --- Storage admin gRPC (ADR-025) ---
    /// `StorageAdminService` per-RPC call counter. Labels:
    /// `rpc` (RPC method name, e.g. `ListPools`) and `outcome`
    /// (`ok`, `client_error`, `server_error`, `unimplemented`).
    /// Cloned into `StorageAdminGrpc::with_metrics()` at runtime
    /// construction.
    pub storage_admin_calls_total: IntCounterVec,
}

impl KisekiMetrics {
    /// Create and register all metrics.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Self {
        let registry = Registry::new();

        let raft_commit_latency = HistogramVec::new(
            HistogramOpts::new("kiseki_raft_commit_latency_seconds", "Raft commit latency")
                .buckets(vec![
                    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0,
                ]),
            &["shard"],
        )
        .expect("metric");
        registry
            .register(Box::new(raft_commit_latency.clone()))
            .expect("register");

        let raft_entries_total =
            IntCounter::new("kiseki_raft_entries_total", "Total Raft entries applied")
                .expect("metric");
        registry
            .register(Box::new(raft_entries_total.clone()))
            .expect("register");

        let chunk_write_bytes =
            IntCounter::new("kiseki_chunk_write_bytes_total", "Chunk bytes written")
                .expect("metric");
        registry
            .register(Box::new(chunk_write_bytes.clone()))
            .expect("register");

        let chunk_read_bytes =
            IntCounter::new("kiseki_chunk_read_bytes_total", "Chunk bytes read").expect("metric");
        registry
            .register(Box::new(chunk_read_bytes.clone()))
            .expect("register");

        let chunk_ec_encode_latency = HistogramVec::new(
            HistogramOpts::new("kiseki_chunk_ec_encode_seconds", "EC encode latency")
                .buckets(vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05]),
            &["strategy"],
        )
        .expect("metric");
        registry
            .register(Box::new(chunk_ec_encode_latency.clone()))
            .expect("register");

        let chunk_persistent_write_phase_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_chunk_persistent_write_phase_duration_seconds",
                "PersistentChunkStore write_chunk phase latency: dedup_check, extent_io, save_meta, device_sync",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["phase"],
        )
        .expect("metric");
        registry
            .register(Box::new(chunk_persistent_write_phase_duration.clone()))
            .expect("register");

        let gateway_requests_total = IntCounterVec::new(
            Opts::new("kiseki_gateway_requests_total", "Gateway request count"),
            &["method", "status"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_requests_total.clone()))
            .expect("register");

        let gateway_request_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_gateway_request_duration_seconds",
                "Gateway request duration",
            )
            .buckets(vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0]),
            &["method"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_request_duration.clone()))
            .expect("register");

        let gateway_get_phase_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_gateway_get_phase_duration_seconds",
                "Gateway GET phase latency: composition_lookup, chunk_fetch, decrypt",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["phase"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_get_phase_duration.clone()))
            .expect("register");

        let gateway_put_phase_duration = HistogramVec::new(
            HistogramOpts::new(
                "kiseki_gateway_put_phase_duration_seconds",
                "Gateway PUT phase latency: encrypt, chunk_write, composition_record",
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["phase"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_put_phase_duration.clone()))
            .expect("register");

        let gateway_workflow_ref_writes_total = IntCounterVec::new(
            Opts::new(
                "kiseki_gateway_workflow_ref_writes_total",
                "S3 PUTs by x-kiseki-workflow-ref validation outcome (ADR-021 / I-WA1)",
            ),
            &["result"],
        )
        .expect("metric");
        registry
            .register(Box::new(gateway_workflow_ref_writes_total.clone()))
            .expect("register");

        let stale_leader_redirects_total = IntCounterVec::new(
            Opts::new(
                "kiseki_native_topology_stale_leader_redirects_total",
                "ADR-008 rev 2 / ADR-014 — stale-leader redirects emitted on the protocol boundary",
            ),
            // Labels match the call site in `kiseki-gateway::s3_server::
            // leader_unavailable_response` (line 299) which passes
            // `&["s3", "<tenant>"]`. Arity-mismatch with the call site
            // would panic at runtime on every 307 emission. ADR-008
            // rev 2 §"Observability" specifies (protocol, tenant).
            &["protocol", "tenant"],
        )
        .expect("metric");
        registry
            .register(Box::new(stale_leader_redirects_total.clone()))
            .expect("register");

        let native_proxy_forwards_total = IntCounterVec::new(
            Opts::new(
                "kiseki_native_proxy_forwards_total",
                "ADR-042 §4 — native gateway requests forwarded to a peer leader via the in-process proxy path",
            ),
            &["source_node", "leader_node"],
        )
        .expect("metric");
        registry
            .register(Box::new(native_proxy_forwards_total.clone()))
            .expect("register");

        let pool_capacity_total = IntGaugeVec::new(
            Opts::new("kiseki_pool_capacity_total_bytes", "Pool total capacity"),
            &["pool"],
        )
        .expect("metric");
        registry
            .register(Box::new(pool_capacity_total.clone()))
            .expect("register");

        let pool_capacity_used = IntGaugeVec::new(
            Opts::new("kiseki_pool_capacity_used_bytes", "Pool used capacity"),
            &["pool"],
        )
        .expect("metric");
        registry
            .register(Box::new(pool_capacity_used.clone()))
            .expect("register");

        // Per-device pool capacity (D2). `kind` is `total` / `used` /
        // `free`. Wired from FileBackedDevice / DeviceBackend stats —
        // the runtime fills the gauges from the StorageAdminService
        // payloads (`ListDevicesResponse`) on its periodic refresh.
        let pool_device_capacity_bytes = IntGaugeVec::new(
            Opts::new(
                "kiseki_pool_device_capacity_bytes",
                "Per-device capacity (kind = total | used | free)",
            ),
            &["pool", "device_id", "kind"],
        )
        .expect("metric");
        registry
            .register(Box::new(pool_device_capacity_bytes.clone()))
            .expect("register");

        let pool_device_errors_total = IntCounterVec::new(
            Opts::new(
                "kiseki_pool_device_errors_total",
                "Per-device IO error counter (op = read | write)",
            ),
            &["device_id", "op"],
        )
        .expect("metric");
        registry
            .register(Box::new(pool_device_errors_total.clone()))
            .expect("register");

        // Node storage capacity + dedup (GH #115).
        let storage_device_used_bytes = IntGauge::new(
            "kiseki_storage_device_used_bytes",
            "Chunk-store device pool bytes used (this node)",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_device_used_bytes.clone()))
            .expect("register");

        let storage_device_total_bytes = IntGauge::new(
            "kiseki_storage_device_total_bytes",
            "Chunk-store device pool total capacity (this node)",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_device_total_bytes.clone()))
            .expect("register");

        let storage_logical_bytes = IntGauge::new(
            "kiseki_storage_logical_bytes",
            "Logical bytes addressed by clients (sum refcount × payload)",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_logical_bytes.clone()))
            .expect("register");

        let storage_physical_bytes = IntGauge::new(
            "kiseki_storage_physical_bytes",
            "Unique stored payload bytes (each chunk once)",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_physical_bytes.clone()))
            .expect("register");

        let storage_chunk_count = IntGauge::new(
            "kiseki_storage_chunk_count",
            "Unique chunk count held locally",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_chunk_count.clone()))
            .expect("register");

        let storage_meta_bytes = IntGauge::new(
            "kiseki_storage_meta_bytes",
            "Metadata tier on-disk bytes (system disk, ADR-030 last-resort tier)",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_meta_bytes.clone()))
            .expect("register");

        let storage_small_bytes = IntGauge::new(
            "kiseki_storage_small_bytes",
            "Small-object inline tier on-disk bytes (system disk, ADR-030)",
        )
        .expect("metric");
        registry
            .register(Box::new(storage_small_bytes.clone()))
            .expect("register");

        let mk_tier = |name: &str, help: &str, reg: &Registry| -> IntGauge {
            let g = IntGauge::new(name, help).expect("metric");
            reg.register(Box::new(g.clone())).expect("register");
            g
        };
        let storage_tier_fast_used = mk_tier(
            "kiseki_storage_tier_fast_used_bytes",
            "Fast-tier (NVMe) used",
            &registry,
        );
        let storage_tier_fast_total = mk_tier(
            "kiseki_storage_tier_fast_total_bytes",
            "Fast-tier (NVMe) total",
            &registry,
        );
        let storage_tier_bulk_used = mk_tier(
            "kiseki_storage_tier_bulk_used_bytes",
            "Bulk-tier (SSD) used",
            &registry,
        );
        let storage_tier_bulk_total = mk_tier(
            "kiseki_storage_tier_bulk_total_bytes",
            "Bulk-tier (SSD) total",
            &registry,
        );
        let storage_tier_cold_used = mk_tier(
            "kiseki_storage_tier_cold_used_bytes",
            "Cold-tier (HDD) used",
            &registry,
        );
        let storage_tier_cold_total = mk_tier(
            "kiseki_storage_tier_cold_total_bytes",
            "Cold-tier (HDD) total",
            &registry,
        );

        let transport_connections_active = IntGauge::new(
            "kiseki_transport_connections_active",
            "Active transport connections",
        )
        .expect("metric");
        registry
            .register(Box::new(transport_connections_active.clone()))
            .expect("register");

        let transport_connections_idle = IntGauge::new(
            "kiseki_transport_connections_idle",
            "Idle transport connections",
        )
        .expect("metric");
        registry
            .register(Box::new(transport_connections_idle.clone()))
            .expect("register");

        let shard_delta_count = IntGaugeVec::new(
            Opts::new("kiseki_shard_delta_count", "Delta count per shard"),
            &["shard"],
        )
        .expect("metric");
        registry
            .register(Box::new(shard_delta_count.clone()))
            .expect("register");

        let key_rotation_total =
            IntCounter::new("kiseki_key_rotation_total", "Key rotations performed")
                .expect("metric");
        registry
            .register(Box::new(key_rotation_total.clone()))
            .expect("register");

        let crypto_shred_total = IntCounter::new(
            "kiseki_crypto_shred_total",
            "Crypto-shred operations performed",
        )
        .expect("metric");
        registry
            .register(Box::new(crypto_shred_total.clone()))
            .expect("register");

        let fabric = std::sync::Arc::new(
            kiseki_chunk_cluster::FabricMetrics::register(&registry)
                .expect("fabric metrics register"),
        );

        let raft_transport = std::sync::Arc::new(
            kiseki_raft::transport_metrics::RaftTransportMetrics::register(&registry)
                .expect("raft transport metrics register"),
        );

        let cluster_control = std::sync::Arc::new(
            crate::cluster_control::ClusterControlMetrics::register(&registry)
                .expect("cluster-control metrics register"),
        );

        let log = std::sync::Arc::new(
            kiseki_log::LogMetrics::register(&registry).expect("log metrics register"),
        );

        let keymanager = std::sync::Arc::new(
            kiseki_keymanager::KeyManagerMetrics::register(&registry)
                .expect("keymanager metrics register"),
        );

        let control = std::sync::Arc::new(
            kiseki_control::metrics::ControlMetrics::register(&registry)
                .expect("control metrics register"),
        );

        let view = std::sync::Arc::new(
            kiseki_view::metrics::ViewMetrics::register(&registry).expect("view metrics register"),
        );

        let block = std::sync::Arc::new(
            kiseki_block::BlockMetrics::register(&registry).expect("block metrics register"),
        );

        let gateway_retry = std::sync::Arc::new(
            kiseki_gateway::metrics::GatewayRetryMetrics::register(&registry)
                .expect("gateway retry metrics register"),
        );

        let composition = std::sync::Arc::new(
            kiseki_composition::metrics::CompositionMetrics::register(&registry)
                .expect("composition metrics register"),
        );

        let storage_admin_calls_total = IntCounterVec::new(
            Opts::new(
                "kiseki_storage_admin_calls_total",
                "StorageAdminService gRPC calls (ADR-025) by RPC and outcome",
            ),
            &["rpc", "outcome"],
        )
        .expect("metric");
        registry
            .register(Box::new(storage_admin_calls_total.clone()))
            .expect("register");

        // ADR-047 hot-path timers: register the `kiseki_hotpath_*`
        // histogram-vec when the `hot-path-trace` feature is on. OFF
        // builds skip this entirely — no metric appears on /metrics,
        // no allocation, no atomic. The function is fully cfg-gated
        // inside kiseki-tracing so a non-feature build sees no symbol.
        #[cfg(feature = "hot-path-trace")]
        kiseki_tracing::hot_path::register(&registry).expect("hotpath metric register");

        Self {
            registry,
            raft_commit_latency,
            raft_entries_total,
            chunk_write_bytes,
            chunk_read_bytes,
            chunk_ec_encode_latency,
            chunk_persistent_write_phase_duration,
            gateway_requests_total,
            gateway_request_duration,
            gateway_get_phase_duration,
            gateway_put_phase_duration,
            gateway_workflow_ref_writes_total,
            stale_leader_redirects_total,
            native_proxy_forwards_total,
            pool_capacity_total,
            pool_capacity_used,
            pool_device_capacity_bytes,
            pool_device_errors_total,
            storage_device_used_bytes,
            storage_device_total_bytes,
            storage_logical_bytes,
            storage_physical_bytes,
            storage_chunk_count,
            storage_meta_bytes,
            storage_small_bytes,
            storage_tier_fast_used,
            storage_tier_fast_total,
            storage_tier_bulk_used,
            storage_tier_bulk_total,
            storage_tier_cold_used,
            storage_tier_cold_total,
            transport_connections_active,
            transport_connections_idle,
            shard_delta_count,
            key_rotation_total,
            crypto_shred_total,
            fabric,
            raft_transport,
            cluster_control,
            log,
            keymanager,
            control,
            view,
            block,
            gateway_retry,
            composition,
            storage_admin_calls_total,
        }
    }

    /// Encode all metrics as Prometheus text format.
    #[must_use]
    pub fn encode(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap_or(());
        String::from_utf8(buffer).unwrap_or_default()
    }
}

impl Default for KisekiMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Start the metrics + admin UI HTTP server on the given address.
///
/// Serves:
/// - `GET /metrics` — Prometheus text exposition format
/// - `GET /health` — `200 OK` (load balancer probe)
/// - `GET /cluster/info` — JSON cluster info with leader discovery
/// - `GET /ui` — Admin dashboard (HTMX + Chart.js)
/// - `GET /ui/api/*` — JSON API endpoints
/// - `GET /ui/fragment/*` — HTMX HTML partial endpoints
/// - `GET /ui/logo` — Logo image
#[allow(clippy::too_many_arguments)] // wire many distinct ops handles into the metrics server boot
pub async fn run_metrics_server(
    addr: SocketAddr,
    metrics: KisekiMetrics,
    peer_addrs: Vec<String>,
    log_store: Option<std::sync::Arc<dyn kiseki_log::LogOps + Send + Sync>>,
    node_info: crate::web::api::NodeInfo,
    compositions: Option<std::sync::Arc<kiseki_composition::composition::CompositionStore>>,
    local_chunk_store: Option<std::sync::Arc<dyn kiseki_chunk::AsyncChunkOps>>,
    cluster_control: Option<std::sync::Arc<crate::cluster_control::ControlStateMachine>>,
    cluster_control_store: Option<std::sync::Arc<crate::cluster_control::OpenRaftControlStore>>,
    audit: Option<crate::web::admin_extra::AuditHandle>,
    key_manager: Option<crate::web::admin_extra::KeyManagerHandle>,
    tenants: Option<crate::web::admin_extra::TenantHandle>,
    namespaces: Option<crate::web::admin_extra::NamespaceHandle>,
    drain: Option<crate::web::admin_extra::DrainHandle>,
) -> std::io::Result<()> {
    use crate::web;

    // Set up the metrics aggregator for cluster-wide view.
    let metrics_addr = addr.to_string();
    let aggregator = std::sync::Arc::new(web::aggregator::MetricsAggregator::new(metrics_addr, 10));

    // Diagnostic store: metric history (3h) + event log (10K events).
    let diagnostics = web::events::new_shared();

    // Clone metrics for the encode closure.
    let metrics_for_ui = metrics.clone();
    let ui_state = web::api::UiState {
        aggregator: std::sync::Arc::clone(&aggregator),
        metrics_encode: std::sync::Arc::new(move || metrics_for_ui.encode()),
        diagnostics: std::sync::Arc::clone(&diagnostics),
        log_store,
        node_info,
        compositions,
        local_chunk_store,
        cluster_control,
        cluster_control_store,
        audit,
        key_manager,
        tenants,
        namespaces,
        drain,
    };

    // Auth config snapshot at boot. `/metrics`, `/health`, `/ui/logo`
    // intentionally stay open (probe surface); `/admin/*`, `/ui/*`
    // and `/cluster/info` go through the auth-tier router. See
    // `web::auth` for the env-var contract.
    let auth = web::auth::AuthConfig::from_env();
    tracing::info!(
        addr = %addr,
        admin_token_set = auth.admin_token.is_some(),
        client_token_set = auth.client_token.is_some(),
        admin_auth_disabled = auth.admin_auth_disabled,
        cluster_info_public = auth.cluster_info_public,
        "metrics + admin UI auth posture",
    );

    // Build combined router: metrics + health + admin UI.
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .route("/ui/logo", get(logo_handler))
        .with_state(metrics)
        .merge(web::api::ui_router(ui_state, auth));

    tracing::info!(addr = %addr, "metrics + admin UI server listening");

    // Spawn background peer scraper + diagnostic recorder.
    let scrape_agg = std::sync::Arc::clone(&aggregator);
    let scrape_diag = std::sync::Arc::clone(&diagnostics);
    let scrape_peers = peer_addrs;
    tokio::spawn(async move {
        let interval = scrape_agg.interval();
        loop {
            for peer in &scrape_peers {
                scrape_agg.scrape_peer(peer).await;
            }
            // Record cluster snapshot into diagnostic history.
            let summary = scrape_agg.cluster_summary().await;
            {
                let mut diag = scrape_diag.write().await;
                diag.record_snapshot(
                    summary.aggregate,
                    summary.healthy_nodes,
                    summary.total_nodes,
                );
            }
            tokio::time::sleep(interval).await;
        }
    });

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(std::io::Error::other)
}

async fn logo_handler() -> impl axum::response::IntoResponse {
    // Serve the embedded logo image.
    let logo_bytes: &[u8] = include_bytes!("static/logo.png");
    (
        axum::http::StatusCode::OK,
        [("content-type", "image/png")],
        logo_bytes,
    )
}

async fn metrics_handler(
    axum::extract::State(metrics): axum::extract::State<KisekiMetrics>,
) -> String {
    metrics.encode()
}

async fn health_handler() -> &'static str {
    "OK"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_encode_after_observation() {
        let m = KisekiMetrics::new();
        // Observe some values so they appear in the output.
        m.raft_commit_latency
            .with_label_values(&["test"])
            .observe(0.001);
        m.chunk_write_bytes.inc_by(100);
        m.gateway_requests_total
            .with_label_values(&["GET", "200"])
            .inc();

        let output = m.encode();
        assert!(
            output.contains("kiseki_raft_commit_latency_seconds"),
            "histogram should appear after observation"
        );
        assert!(
            output.contains("kiseki_chunk_write_bytes_total"),
            "counter should appear after increment"
        );
        assert!(
            output.contains("kiseki_gateway_requests_total"),
            "counter vec should appear after increment"
        );
    }

    #[test]
    fn counter_increments() {
        let m = KisekiMetrics::new();
        m.raft_entries_total.inc();
        m.raft_entries_total.inc();
        assert_eq!(m.raft_entries_total.get(), 2);
    }

    #[test]
    fn histogram_observes() {
        let m = KisekiMetrics::new();
        m.raft_commit_latency
            .with_label_values(&["shard-1"])
            .observe(0.005);
        let output = m.encode();
        assert!(output.contains("shard-1"));
    }

    #[test]
    fn gateway_request_counter() {
        let m = KisekiMetrics::new();
        m.gateway_requests_total
            .with_label_values(&["PUT", "200"])
            .inc();
        m.gateway_requests_total
            .with_label_values(&["GET", "404"])
            .inc();
        let output = m.encode();
        assert!(output.contains("PUT"));
        assert!(output.contains("GET"));
    }

    #[test]
    fn gauge_set_and_read() {
        let m = KisekiMetrics::new();
        m.transport_connections_active.set(42);
        assert_eq!(m.transport_connections_active.get(), 42);
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let resp = health_handler().await;
        assert_eq!(resp, "OK");
    }
}
