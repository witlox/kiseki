//! gRPC `StorageAdminService` (ADR-025).
//!
//! Operator-facing API for storage subsystem management — devices,
//! pools, shards, tuning parameters, observability streams, and
//! repair / scrub. Disjoint from `AdminService` (snapshots, ADR-016)
//! and `ControlService` (tenant-facing).
//!
//! Workstream tracking: every RPC body either implements its W2-W7
//! behavior or returns `Status::unimplemented` with a message naming
//! the workstream that will land it. The inline `tests` module has
//! one test per RPC — real-impl tests for landed RPCs and
//! `_unimplemented_until_w*` tests for pending ones. See
//! `specs/implementation/adr-025-storage-admin-api.md`.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use kiseki_chunk::evacuation::{EvacuationProgress, EvacuationRegistry};
use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy, PoolDevice};
use kiseki_chunk_cluster::maintenance::MaintenanceMode;
use kiseki_chunk_cluster::repair_tracker::{
    RepairRecord, RepairState, RepairTracker, RepairTrigger,
};
use kiseki_chunk_cluster::scrub_scheduler::ScrubScheduler;
use kiseki_common::ids::{ChunkId, ShardId};
use kiseki_log::traits::LogOps;
use kiseki_proto::v1 as pb;
use kiseki_proto::v1::storage_admin_service_server::StorageAdminService;
use prometheus::IntCounterVec;
use tonic::codegen::tokio_stream::Stream;
use tonic::{Code, Request, Response, Status};

use crate::pool_overrides::{PoolMutationDeps, PoolThresholds};

/// Handler for `StorageAdminService`. Holds `Arc` deps each RPC
/// needs. Optional fields cover the W4-W7 surfaces — left `None`
/// today so the W2 read-only impls work without dragging in the
/// rest of the runtime.
pub struct StorageAdminGrpc {
    /// Local-node chunk ops — drives `snapshot_pools()` and
    /// `find_device()` for `ListPools` / `GetPool` / `ListDevices` /
    /// `GetDevice` / `ClusterStatus`.
    chunk_store: Option<Arc<dyn kiseki_chunk::AsyncChunkOps>>,
    /// Voting member node ids — drives `ClusterStatus.node_count`
    /// and `AdminShardInfo.members`. Empty for a single-node cluster.
    cluster_nodes: Vec<u64>,
    /// This node's id — used for `leader_node` best-effort when no
    /// raft membership query is wired (W5 will replace this with a
    /// real raft handle).
    self_node_id: u64,
    /// Bootstrap shard id — the single shard the cluster today
    /// runs. W5's `SplitShard` adds dynamic enumeration via the
    /// cluster control shard's state.
    bootstrap_shard: ShardId,
    /// Repair history ring. `None` for a runtime that hasn't yet
    /// wired the scrub scheduler — in that case `ListRepairs`
    /// returns an empty list (no records yet, not "unimplemented").
    repair_tracker: Option<Arc<RepairTracker>>,
    /// Cluster-wide tuning parameters store (ADR-025 W3). `None`
    /// in unit tests / `from_runtime` callers that pre-date the
    /// wiring — `GetTuningParams` / `SetTuningParams` then return
    /// `FailedPrecondition` instead of falling through to
    /// `Unimplemented` so the caller knows the dep is missing
    /// rather than the RPC.
    tuning: Option<crate::tuning::TuningStore>,
    /// Per-shard maintenance flag store (ADR-025 W4). `None` =
    /// `SetShardMaintenance` returns `FailedPrecondition`.
    maintenance: Option<Arc<MaintenanceMode>>,
    /// In-flight evacuation registry (ADR-025 W4). `None` =
    /// `CancelEvacuation` returns `FailedPrecondition`.
    evacuations: Option<Arc<EvacuationRegistry>>,
    /// On-demand scrub trigger (ADR-025 W4). `None` =
    /// `TriggerScrub` / `RepairChunk` return `FailedPrecondition`
    /// (which is what a single-node cluster sees today since the
    /// scrub scheduler is wired only when fabric peers exist).
    scrub: Option<Arc<ScrubScheduler>>,
    /// Per-pool overrides + rebalance tracker (ADR-025 W5).
    /// `None` = `SetPoolThresholds` / `RebalancePool` return
    /// `FailedPrecondition`.
    pool_mutations: Option<PoolMutationDeps>,
    /// Log store handle for `SplitShard` / `MergeShards`
    /// (ADR-025 W5). `None` = those RPCs return
    /// `FailedPrecondition`.
    log_store: Option<Arc<dyn LogOps + Send + Sync>>,
    /// `kiseki_storage_admin_calls_total{rpc, outcome}` counter,
    /// shared with the global Prometheus registry. `None` in unit
    /// tests — RPC handlers no-op the counter bump in that case.
    calls_total: Option<Arc<IntCounterVec>>,
}

impl Default for StorageAdminGrpc {
    fn default() -> Self {
        Self::for_tests()
    }
}

impl StorageAdminGrpc {
    /// Construct for the production runtime. W2 wires the live
    /// chunk store + cluster membership; W4 onwards adds the rest.
    #[must_use]
    pub fn from_runtime() -> Self {
        // W1 default — empty deps; runtime wires real ones via the
        // builder methods. Kept as a back-compat constructor while
        // run_main migrates to `with_chunk_store`/`with_cluster`/etc.
        Self::for_tests()
    }

    /// Construct with no deps wired — `for_tests` because that's all
    /// it's used for. Returns either an "empty cluster" successful
    /// response (read-only RPCs) or `UNIMPLEMENTED` (W3-W7 RPCs).
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            chunk_store: None,
            cluster_nodes: Vec::new(),
            self_node_id: 0,
            bootstrap_shard: ShardId(uuid::Uuid::nil()),
            repair_tracker: None,
            tuning: None,
            maintenance: None,
            evacuations: None,
            scrub: None,
            pool_mutations: None,
            log_store: None,
            calls_total: None,
        }
    }

    /// Builder: attach the local chunk store. Required for `ListPools` /
    /// `GetPool` / `ListDevices` / `GetDevice` / `ClusterStatus` / `PoolStatus`.
    #[must_use]
    pub fn with_chunk_store(mut self, store: Arc<dyn kiseki_chunk::AsyncChunkOps>) -> Self {
        self.chunk_store = Some(store);
        self
    }

    /// Builder: attach the cluster membership view.
    #[must_use]
    pub fn with_cluster(mut self, cluster_nodes: Vec<u64>, self_node_id: u64) -> Self {
        self.cluster_nodes = cluster_nodes;
        self.self_node_id = self_node_id;
        self
    }

    /// Builder: attach the bootstrap shard id (the single shard the
    /// cluster runs today). W5's `SplitShard` will add dynamic
    /// enumeration; for now `ListShards` returns this single entry.
    #[must_use]
    pub fn with_bootstrap_shard(mut self, shard: ShardId) -> Self {
        self.bootstrap_shard = shard;
        self
    }

    /// Builder: attach the repair history ring shared with the scrub
    /// scheduler. Required for `ListRepairs` to return real data;
    /// without it the RPC returns an empty list.
    #[must_use]
    pub fn with_repair_tracker(mut self, tracker: Arc<RepairTracker>) -> Self {
        self.repair_tracker = Some(tracker);
        self
    }

    /// Builder: attach the cluster-wide tuning parameters store
    /// (ADR-025 W3). Required for `GetTuningParams` /
    /// `SetTuningParams` — without it both RPCs return
    /// `FailedPrecondition`.
    #[must_use]
    pub fn with_tuning_store(mut self, tuning: crate::tuning::TuningStore) -> Self {
        self.tuning = Some(tuning);
        self
    }

    /// Builder: attach the per-shard maintenance flag store
    /// (ADR-025 W4). Same handle is wired into
    /// `ClusterChunkServer::with_maintenance` so the admin RPC
    /// flips the same atomic the write path consults.
    #[must_use]
    pub fn with_maintenance(mut self, m: Arc<MaintenanceMode>) -> Self {
        self.maintenance = Some(m);
        self
    }

    /// Builder: attach the in-flight evacuation registry
    /// (ADR-025 W4). The orchestrator that starts an evacuation
    /// (W5 `EvacuateDevice`) registers each progress handle here;
    /// the admin RPC looks up by id to cancel.
    #[must_use]
    pub fn with_evacuations(mut self, r: Arc<EvacuationRegistry>) -> Self {
        self.evacuations = Some(r);
        self
    }

    /// Builder: attach the scrub scheduler (ADR-025 W4). Required
    /// for `TriggerScrub` and `RepairChunk` — both return
    /// `FailedPrecondition` without it.
    #[must_use]
    pub fn with_scrub(mut self, s: Arc<ScrubScheduler>) -> Self {
        self.scrub = Some(s);
        self
    }

    /// Builder: attach the per-pool overrides + rebalance tracker
    /// (ADR-025 W5). Required for `SetPoolThresholds` and
    /// `RebalancePool`.
    #[must_use]
    pub fn with_pool_mutations(mut self, deps: PoolMutationDeps) -> Self {
        self.pool_mutations = Some(deps);
        self
    }

    /// Builder: attach the log store handle for shard split /
    /// merge (ADR-025 W5). Required for `SplitShard` and
    /// `MergeShards`.
    #[must_use]
    pub fn with_log_store(mut self, log: Arc<dyn LogOps + Send + Sync>) -> Self {
        self.log_store = Some(log);
        self
    }

    /// Builder: attach the global
    /// `kiseki_storage_admin_calls_total{rpc, outcome}` counter so
    /// every RPC bump is visible on `/metrics`. Skip in unit tests.
    #[must_use]
    pub fn with_metrics(mut self, calls_total: Arc<IntCounterVec>) -> Self {
        self.calls_total = Some(calls_total);
        self
    }

    /// Bump `kiseki_storage_admin_calls_total` for `(rpc, outcome)`.
    /// Outcome is one of `ok`, `client_error`, `server_error`,
    /// `unimplemented`. No-op when the counter dep isn't wired (unit tests).
    fn record_outcome(&self, rpc: &'static str, outcome: &'static str) {
        if let Some(c) = self.calls_total.as_ref() {
            c.with_label_values(&[rpc, outcome]).inc();
        }
    }

    /// Map a tonic `Status::code()` to the metric `outcome` label
    /// bucket (matches the Prometheus convention used elsewhere in
    /// the codebase: `client_error` for 4xx-equivalent codes,
    /// `server_error` for 5xx-equivalent, plus the `unimplemented`
    /// special case so W3-W7 stubs are visible on /metrics).
    fn outcome_for(status: &Status) -> &'static str {
        match status.code() {
            Code::Ok => "ok",
            Code::Unimplemented => "unimplemented",
            Code::InvalidArgument
            | Code::NotFound
            | Code::AlreadyExists
            | Code::FailedPrecondition
            | Code::OutOfRange
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::ResourceExhausted => "client_error",
            _ => "server_error",
        }
    }

    /// Wrap a handler body so every call emits an OTEL span and
    /// bumps `kiseki_storage_admin_calls_total{rpc, outcome}`.
    /// `rpc` is the fully-qualified span name (e.g.
    /// `"StorageAdminService.ListPools"`); it is also the value of
    /// the metric `rpc` label.
    async fn with_obs<T, F, Fut>(&self, rpc: &'static str, f: F) -> Result<Response<T>, Status>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Response<T>, Status>>,
    {
        let _span = kiseki_tracing::span(rpc);
        let res = f().await;
        let outcome = match &res {
            Ok(_) => "ok",
            Err(s) => Self::outcome_for(s),
        };
        self.record_outcome(rpc, outcome);
        res
    }

    /// `rpc` is the fully-qualified RPC name (e.g.
    /// `"StorageAdminService.GetTuningParams"`) so it matches the
    /// `with_obs` span/metric labels emitted by the implemented
    /// handlers — keeps the `(rpc, outcome)` label cardinality
    /// uniform across implemented and pending RPCs.
    fn unimpl(&self, rpc: &'static str, workstream: &str, what: &str) -> Status {
        let _span = kiseki_tracing::span(rpc);
        self.record_outcome(rpc, "unimplemented");
        Status::unimplemented(format!("{rpc}: ADR-025 {workstream} — {what}"))
    }

    fn now_iso() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // Cheap RFC 3339-ish — chrono would be heavier than the
        // single-line render is worth. Operators get the precision
        // they need from the underlying epoch + their timezone.
        format!("1970-01-01T00:00:00Z+{secs}s")
    }
}

// Map an `AffinityPool` to the wire-level `PoolInfo`. Pure / total
// fn so the per-pool RPCs share the encoding with `ClusterStatus`.
// `overrides` (W5) merges admin-set per-pool thresholds; pass
// `None` for callers that don't have access to the overrides
// store (`ClusterStatus`, BDD harnesses).
fn pool_to_proto_with_overrides(
    pool: &kiseki_chunk::pool::AffinityPool,
    overrides: Option<PoolThresholds>,
) -> pb::PoolInfo {
    let (durability_kind, replication_copies, ec_data_shards, ec_parity_shards) =
        match pool.durability {
            DurabilityStrategy::Replication { copies } => {
                ("replication".to_owned(), u32::from(copies), 0, 0)
            }
            DurabilityStrategy::ErasureCoding {
                data_shards,
                parity_shards,
            } => (
                "erasure_coding".to_owned(),
                0,
                u32::from(data_shards),
                u32::from(parity_shards),
            ),
        };
    let o = overrides.unwrap_or_default();
    pb::PoolInfo {
        pool_name: pool.name.clone(),
        durability_kind,
        replication_copies,
        ec_data_shards,
        ec_parity_shards,
        capacity_bytes: pool.capacity_bytes,
        used_bytes: pool.used_bytes,
        device_count: u32::try_from(pool.devices.len()).unwrap_or(u32::MAX),
        // ADR-024 defaults when no override is set; admin override
        // wins via the `overrides` arg.
        warning_threshold_pct: o.warning_pct.filter(|v| *v != 0).unwrap_or(70),
        critical_threshold_pct: o.critical_pct.filter(|v| *v != 0).unwrap_or(85),
        readonly_threshold_pct: o.readonly_pct.filter(|v| *v != 0).unwrap_or(95),
        target_fill_pct: o.target_fill_pct.filter(|v| *v != 0).unwrap_or(70),
    }
}

/// Back-compat wrapper for callers that don't have access to a
/// pool-overrides store (cluster-status flat list, BDD harnesses).
fn pool_to_proto(pool: &kiseki_chunk::pool::AffinityPool) -> pb::PoolInfo {
    pool_to_proto_with_overrides(pool, None)
}

fn device_class_to_wire(class: kiseki_chunk::pool::DeviceClass) -> &'static str {
    match class {
        kiseki_chunk::pool::DeviceClass::NvmeSsd => "nvme_ssd",
        kiseki_chunk::pool::DeviceClass::Ssd => "sata_ssd",
        kiseki_chunk::pool::DeviceClass::Hdd => "hdd",
        kiseki_chunk::pool::DeviceClass::Mixed => "mixed",
    }
}

fn device_to_proto(
    pool: &kiseki_chunk::pool::AffinityPool,
    dev: &kiseki_chunk::pool::PoolDevice,
) -> pb::DeviceInfo {
    // Per-device capacity/used isn't tracked separately today —
    // split the pool's totals across the device list as an
    // approximation. ADR-024's per-device tracking is a W5 follow-up.
    let n = pool.devices.len().max(1) as u64;
    pb::DeviceInfo {
        device_id: dev.id.clone(),
        pool_name: pool.name.clone(),
        device_class: device_class_to_wire(pool.device_class).to_owned(),
        capacity_bytes: pool.capacity_bytes / n,
        used_bytes: pool.used_bytes / n,
        online: dev.online,
        evacuating: false, // W5 wires drain orchestrator state
        evacuation_pct: 0,
        sampled_at: StorageAdminGrpc::now_iso(),
    }
}

#[async_trait]
impl StorageAdminService for StorageAdminGrpc {
    type DeviceHealthStream =
        Pin<Box<dyn Stream<Item = Result<pb::DeviceHealthEvent, Status>> + Send>>;
    type IOStatsStream = Pin<Box<dyn Stream<Item = Result<pb::IoStatsEvent, Status>> + Send>>;

    // --- Device management ---

    async fn list_devices(
        &self,
        req: Request<pb::ListDevicesRequest>,
    ) -> Result<Response<pb::ListDevicesResponse>, Status> {
        self.with_obs("StorageAdminService.ListDevices", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.ListDevices: chunk_store dep not wired \
                     (call StorageAdminGrpc::with_chunk_store at construction)",
                )
            })?;
            let filter = req.into_inner().pool_name;
            let pools = store.snapshot_pools().await;
            let mut devices = Vec::new();
            for pool in &pools {
                if !filter.is_empty() && pool.name != filter {
                    continue;
                }
                for dev in &pool.devices {
                    devices.push(device_to_proto(pool, dev));
                }
            }
            Ok(Response::new(pb::ListDevicesResponse { devices }))
        })
        .await
    }

    async fn get_device(
        &self,
        req: Request<pb::GetDeviceRequest>,
    ) -> Result<Response<pb::DeviceInfo>, Status> {
        self.with_obs("StorageAdminService.GetDevice", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.GetDevice: chunk_store dep not wired",
                )
            })?;
            let id = req.into_inner().device_id;
            if id.is_empty() {
                return Err(Status::invalid_argument("device_id is required"));
            }
            let (pool_name, dev) = store
                .find_device(&id)
                .await
                .ok_or_else(|| Status::not_found(format!("device {id} not found")))?;
            // Need the pool too for per-device fields. snapshot_pools is
            // already a small list; one extra walk is fine.
            let pool = store
                .snapshot_pools()
                .await
                .into_iter()
                .find(|p| p.name == pool_name)
                .ok_or_else(|| {
                    Status::internal(format!(
                        "device {id} found but its pool {pool_name} disappeared",
                    ))
                })?;
            Ok(Response::new(device_to_proto(&pool, &dev)))
        })
        .await
    }

    async fn add_device(
        &self,
        req: Request<pb::AddDeviceRequest>,
    ) -> Result<Response<pb::AddDeviceResponse>, Status> {
        self.with_obs("StorageAdminService.AddDevice", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.AddDevice: chunk_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.pool_name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            if r.device_id.is_empty() {
                return Err(Status::invalid_argument("device_id is required"));
            }
            let device = PoolDevice {
                id: r.device_id,
                online: true,
            };
            store
                .add_device_to_pool(&r.pool_name, device)
                .await
                .map_err(|e| {
                    if e.contains("not found") {
                        Status::not_found(e)
                    } else if e.contains("already") {
                        Status::already_exists(e)
                    } else {
                        Status::internal(e)
                    }
                })?;
            Ok(Response::new(pb::AddDeviceResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn remove_device(
        &self,
        req: Request<pb::RemoveDeviceRequest>,
    ) -> Result<Response<pb::RemoveDeviceResponse>, Status> {
        self.with_obs("StorageAdminService.RemoveDevice", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.RemoveDevice: chunk_store dep not wired",
                )
            })?;
            let id = req.into_inner().device_id;
            if id.is_empty() {
                return Err(Status::invalid_argument("device_id is required"));
            }
            store.remove_device(&id).await.map_err(|e| {
                if e.contains("not found") {
                    Status::not_found(e)
                } else {
                    Status::internal(e)
                }
            })?;
            Ok(Response::new(pb::RemoveDeviceResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn evacuate_device(
        &self,
        req: Request<pb::EvacuateDeviceRequest>,
    ) -> Result<Response<pb::EvacuateDeviceResponse>, Status> {
        self.with_obs("StorageAdminService.EvacuateDevice", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.EvacuateDevice: chunk_store dep not wired",
                )
            })?;
            let registry = self.evacuations.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.EvacuateDevice: evacuation registry dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.device_id.is_empty() {
                return Err(Status::invalid_argument("device_id is required"));
            }
            // Confirm the device exists before registering an
            // evacuation entry — otherwise CancelEvacuation has
            // a dangling id with no worker to cancel.
            let _ = store
                .find_device(&r.device_id)
                .await
                .ok_or_else(|| Status::not_found(format!("device {} not found", r.device_id)))?;
            // Stable evac id; the drain orchestrator (ADR-035 +
            // future Raft delta) will be the producer of the
            // actual progress updates.
            let evacuation_id = uuid::Uuid::new_v4().to_string();
            // Stub progress: 16-byte device id (we synthesize
            // since the API uses a string id; pad/truncate via
            // a hash so cancel correlation works without coupling
            // to a specific id format).
            let mut dev_bytes = [0u8; 16];
            for (i, b) in r.device_id.as_bytes().iter().take(16).enumerate() {
                dev_bytes[i] = *b;
            }
            let progress = Arc::new(EvacuationProgress::new(dev_bytes, 0));
            registry.register(evacuation_id.clone(), progress);
            Ok(Response::new(pb::EvacuateDeviceResponse {
                evacuation_id,
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn cancel_evacuation(
        &self,
        req: Request<pb::CancelEvacuationRequest>,
    ) -> Result<Response<pb::CancelEvacuationResponse>, Status> {
        self.with_obs("StorageAdminService.CancelEvacuation", || async move {
            let registry = self.evacuations.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.CancelEvacuation: evacuation registry dep not wired",
                )
            })?;
            let id = req.into_inner().evacuation_id;
            if id.is_empty() {
                return Err(Status::invalid_argument("evacuation_id is required"));
            }
            if !registry.cancel(&id) {
                return Err(Status::not_found(format!("evacuation {id} not found")));
            }
            // committed_at_log_index = 0 — node-local mutation;
            // W5 will replicate the cancellation via Raft if it
            // turns out cluster-wide visibility is needed.
            Ok(Response::new(pb::CancelEvacuationResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    // --- Pool management ---

    async fn list_pools(
        &self,
        _req: Request<pb::ListPoolsRequest>,
    ) -> Result<Response<pb::ListPoolsResponse>, Status> {
        self.with_obs("StorageAdminService.ListPools", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.ListPools: chunk_store dep not wired",
                )
            })?;
            let pools = store
                .snapshot_pools()
                .await
                .iter()
                .map(pool_to_proto)
                .collect();
            Ok(Response::new(pb::ListPoolsResponse { pools }))
        })
        .await
    }

    async fn get_pool(
        &self,
        req: Request<pb::GetPoolRequest>,
    ) -> Result<Response<pb::PoolInfo>, Status> {
        self.with_obs("StorageAdminService.GetPool", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.GetPool: chunk_store dep not wired",
                )
            })?;
            let name = req.into_inner().pool_name;
            if name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            let pool = store
                .snapshot_pools()
                .await
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| Status::not_found(format!("pool {name} not found")))?;
            // Merge ADR-025 W5 admin-set per-pool thresholds.
            let overrides = self
                .pool_mutations
                .as_ref()
                .and_then(|d| d.thresholds.get(&name));
            Ok(Response::new(pool_to_proto_with_overrides(
                &pool, overrides,
            )))
        })
        .await
    }

    async fn create_pool(
        &self,
        req: Request<pb::CreatePoolRequest>,
    ) -> Result<Response<pb::CreatePoolResponse>, Status> {
        self.with_obs("StorageAdminService.CreatePool", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.CreatePool: chunk_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.pool_name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            let device_class = parse_device_class(&r.device_class)?;
            let durability = parse_durability(
                &r.durability_kind,
                r.replication_copies,
                r.ec_data_shards,
                r.ec_parity_shards,
            )?;
            let pool = AffinityPool {
                name: r.pool_name.clone(),
                durability,
                device_class,
                capacity_bytes: r.initial_capacity_bytes,
                used_bytes: 0,
                devices: Vec::new(),
            };
            store.add_pool(pool).await.map_err(|e| {
                if e.contains("already exists") {
                    Status::already_exists(e)
                } else {
                    Status::internal(e)
                }
            })?;
            Ok(Response::new(pb::CreatePoolResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn set_pool_durability(
        &self,
        req: Request<pb::SetPoolDurabilityRequest>,
    ) -> Result<Response<pb::SetPoolDurabilityResponse>, Status> {
        self.with_obs("StorageAdminService.SetPoolDurability", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.SetPoolDurability: chunk_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.pool_name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            let strategy = parse_durability(
                &r.durability_kind,
                r.replication_copies,
                r.ec_data_shards,
                r.ec_parity_shards,
            )?;
            store
                .set_pool_durability(&r.pool_name, strategy)
                .await
                .map_err(|e| {
                    if e.contains("not found") {
                        Status::not_found(e)
                    } else if e.contains("non-empty") {
                        Status::failed_precondition(e)
                    } else {
                        Status::internal(e)
                    }
                })?;
            Ok(Response::new(pb::SetPoolDurabilityResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn set_pool_thresholds(
        &self,
        req: Request<pb::SetPoolThresholdsRequest>,
    ) -> Result<Response<pb::SetPoolThresholdsResponse>, Status> {
        self.with_obs("StorageAdminService.SetPoolThresholds", || async move {
            let deps = self.pool_mutations.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.SetPoolThresholds: pool_mutations dep not wired",
                )
            })?;
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.SetPoolThresholds: chunk_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.pool_name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            // Confirm the pool exists before storing overrides —
            // operators expect an explicit NotFound rather than
            // silently creating a record for a missing pool.
            if store
                .snapshot_pools()
                .await
                .iter()
                .all(|p| p.name != r.pool_name)
            {
                return Err(Status::not_found(format!("pool {} not found", r.pool_name)));
            }
            let thresholds = PoolThresholds {
                warning_pct: Some(r.warning_threshold_pct),
                critical_pct: Some(r.critical_threshold_pct),
                readonly_pct: Some(r.readonly_threshold_pct),
                target_fill_pct: Some(r.target_fill_pct),
            };
            deps.thresholds
                .set(&r.pool_name, thresholds)
                .map_err(Status::invalid_argument)?;
            Ok(Response::new(pb::SetPoolThresholdsResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn rebalance_pool(
        &self,
        req: Request<pb::RebalancePoolRequest>,
    ) -> Result<Response<pb::RebalancePoolResponse>, Status> {
        self.with_obs("StorageAdminService.RebalancePool", || async move {
            let deps = self.pool_mutations.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.RebalancePool: pool_mutations dep not wired",
                )
            })?;
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.RebalancePool: chunk_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.pool_name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            if store
                .snapshot_pools()
                .await
                .iter()
                .all(|p| p.name != r.pool_name)
            {
                return Err(Status::not_found(format!("pool {} not found", r.pool_name)));
            }
            // Real rebalance worker isn't implemented in this
            // crate; the tracker hands back a stable id so
            // operators can correlate logs and a future
            // ListRebalances RPC has a backing store.
            let rebalance_id = deps.rebalance.record(r.pool_name, r.throughput_mb_s);
            Ok(Response::new(pb::RebalancePoolResponse { rebalance_id }))
        })
        .await
    }

    // --- Performance tuning ---

    async fn get_tuning_params(
        &self,
        _req: Request<pb::GetTuningParamsRequest>,
    ) -> Result<Response<pb::TuningParams>, Status> {
        self.with_obs("StorageAdminService.GetTuningParams", || async move {
            let tuning = self.tuning.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.GetTuningParams: tuning store dep not wired",
                )
            })?;
            Ok(Response::new(tuning.get().await.to_proto()))
        })
        .await
    }

    async fn set_tuning_params(
        &self,
        req: Request<pb::SetTuningParamsRequest>,
    ) -> Result<Response<pb::SetTuningParamsResponse>, Status> {
        self.with_obs("StorageAdminService.SetTuningParams", || async move {
            let tuning = self.tuning.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.SetTuningParams: tuning store dep not wired",
                )
            })?;
            let pb_params = req
                .into_inner()
                .params
                .ok_or_else(|| Status::invalid_argument("params is required"))?;
            let params = crate::tuning::TuningParams::from_proto(&pb_params);
            // `set` runs validate() first; bounds errors map to
            // InvalidArgument naming the offending field.
            tuning.set(params).await?;
            // Raft replication lands in W5 — until then the
            // committed_at_log_index is best-effort 0 (single-node
            // local apply). Documented in ADR-025 W3 plan.
            Ok(Response::new(pb::SetTuningParamsResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    // --- Cluster observability ---

    async fn cluster_status(
        &self,
        _req: Request<pb::ClusterStatusRequest>,
    ) -> Result<Response<pb::AdminClusterStatus>, Status> {
        self.with_obs("StorageAdminService.ClusterStatus", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.ClusterStatus: chunk_store dep not wired",
                )
            })?;
            let pools = store.snapshot_pools().await;
            let total: u64 = pools.iter().map(|p| p.capacity_bytes).sum();
            let used: u64 = pools.iter().map(|p| p.used_bytes).sum();
            let node_count = if self.cluster_nodes.is_empty() {
                1
            } else {
                self.cluster_nodes.len()
            };
            // No raft-membership query wired yet (W5 territory). Best-
            // effort: report this node as the leader. Operators on a
            // multi-node cluster should treat this as an upper bound
            // until W5 plumbs the real query.
            let leader_node = if self.self_node_id == 0 {
                String::new()
            } else {
                format!("node-{}", self.self_node_id)
            };
            Ok(Response::new(pb::AdminClusterStatus {
                node_count: u32::try_from(node_count).unwrap_or(u32::MAX),
                shard_count: 1, // single-shard today; W5's SplitShard makes this dynamic
                pool_count: u32::try_from(pools.len()).unwrap_or(u32::MAX),
                total_capacity_bytes: total,
                used_capacity_bytes: used,
                maintenance_mode: false, // W4 wires SetShardMaintenance backing flag
                leader_node,
                sampled_at: Self::now_iso(),
            }))
        })
        .await
    }

    async fn pool_status(
        &self,
        req: Request<pb::PoolStatusRequest>,
    ) -> Result<Response<pb::AdminPoolStatus>, Status> {
        // Without per-pool overrides (W5), use ADR-024 defaults to
        // compute capacity_state.
        const WARN: u64 = 70;
        const CRIT: u64 = 85;
        const RO: u64 = 95;

        self.with_obs("StorageAdminService.PoolStatus", || async move {
            let store = self.chunk_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.PoolStatus: chunk_store dep not wired",
                )
            })?;
            let name = req.into_inner().pool_name;
            if name.is_empty() {
                return Err(Status::invalid_argument("pool_name is required"));
            }
            let pool = store
                .snapshot_pools()
                .await
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| Status::not_found(format!("pool {name} not found")))?;
            let overrides = self
                .pool_mutations
                .as_ref()
                .and_then(|d| d.thresholds.get(&name));
            let pool_proto = pool_to_proto_with_overrides(&pool, overrides);
            let devices = pool
                .devices
                .iter()
                .map(|d| device_to_proto(&pool, d))
                .collect();
            let fill_pct = pool
                .used_bytes
                .saturating_mul(100)
                .checked_div(pool.capacity_bytes)
                .unwrap_or(0);
            let capacity_state = if fill_pct >= RO {
                "readonly"
            } else if fill_pct >= CRIT {
                "critical"
            } else if fill_pct >= WARN {
                "warning"
            } else {
                "ok"
            };
            Ok(Response::new(pb::AdminPoolStatus {
                pool: Some(pool_proto),
                devices,
                capacity_state: capacity_state.to_owned(),
                // Per-pool chunk count + fragment count requires a
                // chunk-store query that doesn't exist yet (the underlying
                // ChunkStore tracks one global chunk map, not per-pool).
                // W5's CreatePool will land per-pool indexes; for now
                // these are 0 and operators rely on `chunk_count()` from
                // the existing /metrics endpoint instead.
                chunk_count: 0,
                fragments_total: 0,
            }))
        })
        .await
    }

    async fn device_health(
        &self,
        _req: Request<pb::DeviceHealthRequest>,
    ) -> Result<Response<Self::DeviceHealthStream>, Status> {
        Err(self.unimpl(
            "StorageAdminService.DeviceHealth",
            "W7",
            "server-streaming; broadcast(1024) channel from chunk subsystem",
        ))
    }

    async fn io_stats(
        &self,
        _req: Request<pb::IoStatsRequest>,
    ) -> Result<Response<Self::IOStatsStream>, Status> {
        Err(self.unimpl(
            "StorageAdminService.IOStats",
            "W7",
            "server-streaming; broadcast(1024) channel from chunk-cluster",
        ))
    }

    // --- Shard management ---

    async fn list_shards(
        &self,
        req: Request<pb::ListShardsRequest>,
    ) -> Result<Response<pb::ListShardsResponse>, Status> {
        self.with_obs("StorageAdminService.ListShards", || async move {
            let filter = req.into_inner().tenant_id;
            // Single-shard cluster today. The bootstrap shard belongs to
            // the bootstrap tenant by convention; tenant_id filter is
            // best-effort — anything that isn't the bootstrap tenant
            // returns empty.
            if filter.is_empty() || filter == Self::bootstrap_tenant_string() {
                Ok(Response::new(pb::ListShardsResponse {
                    shards: vec![self.bootstrap_admin_shard_info()],
                }))
            } else {
                Ok(Response::new(pb::ListShardsResponse { shards: Vec::new() }))
            }
        })
        .await
    }

    async fn get_shard(
        &self,
        req: Request<pb::GetShardRequest>,
    ) -> Result<Response<pb::AdminShardInfo>, Status> {
        self.with_obs("StorageAdminService.GetShard", || async move {
            let id = req.into_inner().shard_id;
            if id.is_empty() {
                return Err(Status::invalid_argument("shard_id is required"));
            }
            let info = self.bootstrap_admin_shard_info();
            if info.shard_id != id {
                return Err(Status::not_found(format!("shard {id} not found")));
            }
            Ok(Response::new(info))
        })
        .await
    }

    async fn split_shard(
        &self,
        req: Request<pb::SplitShardRequest>,
    ) -> Result<Response<pb::SplitShardResponse>, Status> {
        self.with_obs("StorageAdminService.SplitShard", || async move {
            let log = self.log_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.SplitShard: log_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.shard_id.is_empty() {
                return Err(Status::invalid_argument("shard_id is required"));
            }
            let shard = parse_shard_id(&r.shard_id)?;
            let new_shard = ShardId(uuid::Uuid::new_v4());
            // Delegate to the trait. node_id 0 here is fine for
            // single-node mode; multi-node clusters land split
            // ownership via the cluster control shard's Raft.
            let _returned = log
                .split_shard(shard, new_shard, kiseki_common::ids::NodeId(0))
                .map_err(|e| {
                    if matches!(e, kiseki_log::error::LogError::ShardNotFound(_)) {
                        Status::not_found(format!("shard {} not found", r.shard_id))
                    } else {
                        Status::internal(format!("split: {e}"))
                    }
                })?;
            // Proto convention: original shard becomes "left" (the
            // lower half of the key range after the split); new
            // shard is "right". `LogStore::split_shard` returns the
            // new shard id; we wire it as `right_shard_id`.
            Ok(Response::new(pb::SplitShardResponse {
                left_shard_id: shard.0.to_string(),
                right_shard_id: new_shard.0.to_string(),
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn merge_shards(
        &self,
        req: Request<pb::MergeShardsRequest>,
    ) -> Result<Response<pb::MergeShardsResponse>, Status> {
        self.with_obs("StorageAdminService.MergeShards", || async move {
            let log = self.log_store.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.MergeShards: log_store dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.left_shard_id.is_empty() || r.right_shard_id.is_empty() {
                return Err(Status::invalid_argument(
                    "left_shard_id and right_shard_id are required",
                ));
            }
            let left = parse_shard_id(&r.left_shard_id)?;
            let right = parse_shard_id(&r.right_shard_id)?;
            if left == right {
                return Err(Status::invalid_argument(
                    "left and right shards must differ",
                ));
            }
            // Convention: merge "right into left" → left becomes the
            // surviving target, right is decommissioned.
            log.merge_shards(left, right).map_err(|e| {
                if matches!(e, kiseki_log::error::LogError::ShardNotFound(_)) {
                    Status::not_found(format!("merge: {e}"))
                } else {
                    Status::internal(format!("merge: {e}"))
                }
            })?;
            Ok(Response::new(pb::MergeShardsResponse {
                merged_shard_id: left.0.to_string(),
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    async fn set_shard_maintenance(
        &self,
        req: Request<pb::SetShardMaintenanceRequest>,
    ) -> Result<Response<pb::SetShardMaintenanceResponse>, Status> {
        self.with_obs("StorageAdminService.SetShardMaintenance", || async move {
            let m = self.maintenance.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.SetShardMaintenance: maintenance dep not wired",
                )
            })?;
            let r = req.into_inner();
            if r.shard_id.is_empty() {
                return Err(Status::invalid_argument("shard_id is required"));
            }
            // Today only the bootstrap shard exists; the admin RPC
            // accepts that single id (or any id matching it) and
            // rejects everything else with NotFound. W5's
            // SplitShard makes this dynamic.
            let shard = parse_shard_id(&r.shard_id)?;
            if shard != self.bootstrap_shard {
                return Err(Status::not_found(format!("shard {} not found", r.shard_id)));
            }
            m.set(shard, r.enabled);
            // committed_at_log_index = 0 — node-local mutation;
            // W5 will replicate via Raft for cluster-wide effect.
            Ok(Response::new(pb::SetShardMaintenanceResponse {
                committed_at_log_index: 0,
            }))
        })
        .await
    }

    // --- Repair and scrub ---

    async fn trigger_scrub(
        &self,
        req: Request<pb::TriggerScrubRequest>,
    ) -> Result<Response<pb::TriggerScrubResponse>, Status> {
        self.with_obs("StorageAdminService.TriggerScrub", || async move {
            let scrub = self.scrub.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.TriggerScrub: scrub scheduler dep not wired \
                     (single-node cluster — no fabric peers to scrub against)",
                )
            })?;
            let pool_filter = req.into_inner().pool_name;
            let scrub_id = uuid::Uuid::new_v4().to_string();
            // Record the trigger in the repair tracker so
            // operators see it on `ListRepairs`. The scrub-pass
            // outcome lands as additional records when the
            // scheduler runs; this entry covers the trigger
            // itself (in_progress → succeeded on completion).
            if let Some(t) = self.repair_tracker.as_ref() {
                let detail = if pool_filter.is_empty() {
                    format!("scrub_id={scrub_id} pool=*")
                } else {
                    format!("scrub_id={scrub_id} pool={pool_filter}")
                };
                let id = t.start(RepairTrigger::Scrub, ChunkId([0; 32]), detail);
                let t_clone = Arc::clone(t);
                let scrub_handle = Arc::clone(scrub).trigger_now();
                tokio::spawn(async move {
                    let report = scrub_handle.await.unwrap_or_default();
                    let detail = format!(
                        "orphan_deleted={} repaired={} critical={} lost={}",
                        report.orphan.deleted,
                        report.under_replication.repaired,
                        report.under_replication.critical,
                        report.under_replication.lost,
                    );
                    t_clone.update_state(&id, RepairState::Succeeded, Some(detail));
                });
            } else {
                // No tracker — fire-and-forget the scrub. The
                // returned JoinHandle is dropped intentionally; the
                // scrub task continues running detached.
                drop(Arc::clone(scrub).trigger_now());
            }
            Ok(Response::new(pb::TriggerScrubResponse { scrub_id }))
        })
        .await
    }

    async fn repair_chunk(
        &self,
        req: Request<pb::AdminRepairChunkRequest>,
    ) -> Result<Response<pb::RepairChunkResponse>, Status> {
        self.with_obs("StorageAdminService.RepairChunk", || async move {
            let scrub = self.scrub.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "StorageAdminService.RepairChunk: scrub scheduler dep not wired \
                     (single-node cluster — no fabric peers to repair from)",
                )
            })?;
            let chunk_id_hex = req.into_inner().chunk_id_hex;
            let chunk_id = parse_chunk_id_hex(&chunk_id_hex)?;
            let repair_id = if let Some(t) = self.repair_tracker.as_ref() {
                t.record(RepairRecord {
                    repair_id: String::new(),
                    chunk_id,
                    trigger: RepairTrigger::Manual,
                    state: RepairState::InProgress,
                    started_at_ms: now_ms(),
                    finished_at_ms: None,
                    detail: format!("manual repair of {chunk_id_hex}"),
                })
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            let already_healthy = scrub.repair_one_chunk(chunk_id).await.map_err(|e| {
                if let Some(t) = self.repair_tracker.as_ref() {
                    t.update_state(&repair_id, RepairState::Failed, Some(e.clone()));
                }
                if e.contains("not found") {
                    Status::not_found(e)
                } else {
                    Status::internal(e)
                }
            })?;
            if let Some(t) = self.repair_tracker.as_ref() {
                let detail = if already_healthy {
                    "already healthy — no fragments missing".to_owned()
                } else {
                    "repair triggered — see scrub scheduler logs".to_owned()
                };
                t.update_state(&repair_id, RepairState::Succeeded, Some(detail));
            }
            Ok(Response::new(pb::RepairChunkResponse {
                repair_id,
                already_healthy,
            }))
        })
        .await
    }

    async fn list_repairs(
        &self,
        req: Request<pb::ListRepairsRequest>,
    ) -> Result<Response<pb::ListRepairsResponse>, Status> {
        self.with_obs("StorageAdminService.ListRepairs", || async move {
            let mut limit = req.into_inner().limit as usize;
            if limit == 0 {
                limit = 100;
            }
            if limit > 1000 {
                limit = 1000;
            }
            let records = match self.repair_tracker.as_ref() {
                Some(t) => t
                    .recent(limit)
                    .into_iter()
                    .map(|r| pb::RepairRecord {
                        repair_id: r.repair_id,
                        chunk_id_hex: hex_encode_chunk(r.chunk_id.0),
                        trigger: r.trigger.as_wire().to_owned(),
                        state: r.state.as_wire().to_owned(),
                        started_at: format!("ms:{}", r.started_at_ms),
                        finished_at: r
                            .finished_at_ms
                            .map(|ms| format!("ms:{ms}"))
                            .unwrap_or_default(),
                        detail: r.detail,
                    })
                    .collect(),
                // No tracker wired (single-node default); honest empty
                // list, NOT Unimplemented.
                None => Vec::new(),
            };
            Ok(Response::new(pb::ListRepairsResponse { repairs: records }))
        })
        .await
    }
}

impl StorageAdminGrpc {
    /// Until W5 wires multi-tenant shard enumeration, the bootstrap
    /// shard belongs to the well-known bootstrap tenant (ID derived
    /// from `u128(1)`). Matches what `runtime::run_main` installs.
    fn bootstrap_tenant_string() -> String {
        uuid::Uuid::from_u128(1).to_string()
    }

    fn bootstrap_admin_shard_info(&self) -> pb::AdminShardInfo {
        let leader_node = if self.self_node_id == 0 {
            String::new()
        } else {
            format!("node-{}", self.self_node_id)
        };
        let members = if self.cluster_nodes.is_empty() {
            vec![leader_node.clone()]
        } else {
            self.cluster_nodes
                .iter()
                .map(|id| format!("node-{id}"))
                .collect()
        };
        pb::AdminShardInfo {
            shard_id: self.bootstrap_shard.0.to_string(),
            tenant_id: Self::bootstrap_tenant_string(),
            leader_node,
            members,
            // Without a raft handle we can't report last_applied
            // accurately. W5 will plumb it.
            last_applied_log_index: 0,
            maintenance: false,
            entry_count: 0,
        }
    }
}

fn hex_encode_chunk(bytes: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode a 64-char hex string into a `ChunkId`. Returns
/// `InvalidArgument` on length or character errors so the admin
/// RPC surfaces the real reason instead of `Internal`.
fn parse_chunk_id_hex(s: &str) -> Result<ChunkId, Status> {
    if s.len() != 64 {
        return Err(Status::invalid_argument(format!(
            "chunk_id_hex must be 64 chars (32 bytes hex); got {}",
            s.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte_out) in out.iter_mut().enumerate() {
        let off = i * 2;
        *byte_out = u8::from_str_radix(&s[off..off + 2], 16)
            .map_err(|e| Status::invalid_argument(format!("chunk_id_hex byte {i}: {e}")))?;
    }
    Ok(ChunkId(out))
}

/// Parse a `ShardId` from its textual UUID form. `InvalidArgument`
/// on a malformed string.
fn parse_shard_id(s: &str) -> Result<ShardId, Status> {
    let u =
        uuid::Uuid::parse_str(s).map_err(|e| Status::invalid_argument(format!("shard_id: {e}")))?;
    Ok(ShardId(u))
}

/// Parse the `device_class` wire string into [`DeviceClass`].
/// Mirrors `device_class_to_wire`.
fn parse_device_class(s: &str) -> Result<DeviceClass, Status> {
    match s.to_ascii_lowercase().as_str() {
        "" | "nvme_ssd" | "nvme" => Ok(DeviceClass::NvmeSsd),
        "ssd" | "sata_ssd" => Ok(DeviceClass::Ssd),
        "hdd" => Ok(DeviceClass::Hdd),
        "mixed" => Ok(DeviceClass::Mixed),
        other => Err(Status::invalid_argument(format!(
            "device_class {other} not recognized; expected one of nvme_ssd / ssd / hdd / mixed",
        ))),
    }
}

/// Parse a (`durability_kind`, `replication_copies`,
/// `ec_data_shards`, `ec_parity_shards`) tuple into
/// [`DurabilityStrategy`]. Validates per ADR-005 / ADR-025:
/// replication copies in 2..=5; EC data shards in 2..=16; EC
/// parity shards in 1..=8.
fn parse_durability(
    kind: &str,
    replication_copies: u32,
    ec_data_shards: u32,
    ec_parity_shards: u32,
) -> Result<DurabilityStrategy, Status> {
    match kind {
        "replication" => {
            if !(2..=5).contains(&replication_copies) {
                return Err(Status::invalid_argument(format!(
                    "replication_copies = {replication_copies} out of [2, 5]",
                )));
            }
            // Bounds-checked above → fits in u8.
            Ok(DurabilityStrategy::Replication {
                copies: u8::try_from(replication_copies).expect("validated"),
            })
        }
        "erasure_coding" | "ec" => {
            if !(2..=16).contains(&ec_data_shards) {
                return Err(Status::invalid_argument(format!(
                    "ec_data_shards = {ec_data_shards} out of [2, 16]",
                )));
            }
            if !(1..=8).contains(&ec_parity_shards) {
                return Err(Status::invalid_argument(format!(
                    "ec_parity_shards = {ec_parity_shards} out of [1, 8]",
                )));
            }
            Ok(DurabilityStrategy::ErasureCoding {
                data_shards: u8::try_from(ec_data_shards).expect("validated"),
                parity_shards: u8::try_from(ec_parity_shards).expect("validated"),
            })
        }
        other => Err(Status::invalid_argument(format!(
            "durability_kind {other} not recognized; expected `replication` or `erasure_coding`",
        ))),
    }
}

/// Wall-clock now in Unix-millis. Shared with `RepairTracker` —
/// keep the function here so `RepairChunk` can stamp records
/// without pulling in another crate's helper.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ===========================================================================
// Tests — one per RPC. Real-impl assertions for landed RPCs;
// `_unimplemented_until_w*` for pending ones. The cardinality
// guards at the end keep the proto / impl / test counts in lockstep.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy, PoolDevice};
    use kiseki_chunk::store::ChunkStore;
    use kiseki_chunk::SyncBridge;
    use kiseki_chunk_cluster::repair_tracker::{RepairTracker, RepairTrigger};
    use kiseki_common::ids::ChunkId;
    use tonic::Code;

    /// Build a `StorageAdminGrpc` with a populated chunk store and
    /// 3-node cluster membership. Used by the W2 read-only tests.
    fn fixture_with_pools() -> (StorageAdminGrpc, Arc<RepairTracker>) {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool {
            name: "fast-nvme".into(),
            durability: DurabilityStrategy::ErasureCoding {
                data_shards: 4,
                parity_shards: 2,
            },
            capacity_bytes: 10_000_000_000,
            used_bytes: 2_500_000_000,
            device_class: DeviceClass::NvmeSsd,
            devices: vec![
                PoolDevice {
                    id: "nvme-a".into(),
                    online: true,
                },
                PoolDevice {
                    id: "nvme-b".into(),
                    online: true,
                },
                PoolDevice {
                    id: "nvme-c".into(),
                    online: false,
                },
            ],
        });
        store.add_pool(AffinityPool {
            name: "bulk-hdd".into(),
            durability: DurabilityStrategy::Replication { copies: 3 },
            capacity_bytes: 100_000_000_000,
            used_bytes: 90_000_000_000, // 90% — should land in capacity_state=critical
            device_class: DeviceClass::Hdd,
            devices: vec![PoolDevice {
                id: "hdd-x".into(),
                online: true,
            }],
        });
        let async_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = Arc::new(SyncBridge::new(store));
        let tracker = Arc::new(RepairTracker::new());
        let svc = StorageAdminGrpc::for_tests()
            .with_chunk_store(Arc::clone(&async_store))
            .with_cluster(vec![1, 2, 3], 1)
            .with_bootstrap_shard(ShardId(uuid::Uuid::from_u128(0xDEAD)))
            .with_repair_tracker(Arc::clone(&tracker));
        (svc, tracker)
    }

    fn fixture_empty() -> StorageAdminGrpc {
        StorageAdminGrpc::for_tests()
    }

    fn assert_unimplemented_with_workstream<T>(
        label: &str,
        workstream: &str,
        result: Result<tonic::Response<T>, tonic::Status>,
    ) {
        let err = result
            .map(|_| ())
            .err()
            .unwrap_or_else(|| panic!("{label}: should be Unimplemented, got Ok"));
        assert_eq!(
            err.code(),
            Code::Unimplemented,
            "{label}: code should be Unimplemented; got {err:?}",
        );
        let msg = err.message();
        assert!(
            msg.contains(workstream),
            "{label}: message should reference workstream {workstream}; got: {msg}",
        );
        assert!(
            msg.contains("ADR-025"),
            "{label}: message should reference ADR-025; got: {msg}",
        );
        assert!(
            msg.contains(label),
            "{label}: message should name the RPC; got: {msg}",
        );
    }

    // ====================================================================
    // W2 — read-only RPCs (real-impl tests)
    // ====================================================================

    #[tokio::test]
    async fn list_devices_returns_every_device_across_pools() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .list_devices(Request::new(pb::ListDevicesRequest::default()))
            .await
            .expect("list_devices ok");
        let devices = resp.into_inner().devices;
        assert_eq!(devices.len(), 4, "3 nvme + 1 hdd = 4");
        // Per-device fields
        let nvme: Vec<_> = devices
            .iter()
            .filter(|d| d.pool_name == "fast-nvme")
            .collect();
        assert_eq!(nvme.len(), 3);
        assert!(nvme.iter().all(|d| d.device_class == "nvme_ssd"));
        // Per-device capacity = pool / device_count = 10G / 3 ≈ 3.33G
        assert!(nvme.iter().all(|d| d.capacity_bytes == 10_000_000_000 / 3));
        // Online state propagates: 2 of 3 nvme are online + the hdd.
        assert_eq!(
            devices.iter().filter(|d| d.online).count(),
            3,
            "2 online nvme + 1 online hdd",
        );
    }

    #[tokio::test]
    async fn list_devices_filters_by_pool() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .list_devices(Request::new(pb::ListDevicesRequest {
                pool_name: "bulk-hdd".into(),
            }))
            .await
            .expect("list_devices ok");
        let devices = resp.into_inner().devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_id, "hdd-x");
    }

    #[tokio::test]
    async fn get_device_finds_by_id() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .get_device(Request::new(pb::GetDeviceRequest {
                device_id: "nvme-c".into(),
            }))
            .await
            .expect("get_device ok");
        let dev = resp.into_inner();
        assert_eq!(dev.device_id, "nvme-c");
        assert_eq!(dev.pool_name, "fast-nvme");
        assert!(!dev.online, "nvme-c is the offline one in the fixture");
    }

    #[tokio::test]
    async fn get_device_missing_returns_not_found() {
        let (svc, _) = fixture_with_pools();
        let err = svc
            .get_device(Request::new(pb::GetDeviceRequest {
                device_id: "no-such-device".into(),
            }))
            .await
            .expect_err("should be not_found");
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn get_device_empty_id_returns_invalid_argument() {
        let (svc, _) = fixture_with_pools();
        let err = svc
            .get_device(Request::new(pb::GetDeviceRequest::default()))
            .await
            .expect_err("should be invalid_argument");
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn list_pools_returns_both_pools_with_correct_durability() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .list_pools(Request::new(pb::ListPoolsRequest::default()))
            .await
            .expect("list_pools ok");
        let pools = resp.into_inner().pools;
        assert_eq!(pools.len(), 2);
        let nvme = pools
            .iter()
            .find(|p| p.pool_name == "fast-nvme")
            .expect("nvme");
        assert_eq!(nvme.durability_kind, "erasure_coding");
        assert_eq!(nvme.ec_data_shards, 4);
        assert_eq!(nvme.ec_parity_shards, 2);
        assert_eq!(nvme.replication_copies, 0);
        assert_eq!(nvme.device_count, 3);
        let hdd = pools
            .iter()
            .find(|p| p.pool_name == "bulk-hdd")
            .expect("hdd");
        assert_eq!(hdd.durability_kind, "replication");
        assert_eq!(hdd.replication_copies, 3);
        assert_eq!(hdd.ec_data_shards, 0);
        assert_eq!(hdd.ec_parity_shards, 0);
    }

    #[tokio::test]
    async fn get_pool_finds_by_name() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: "fast-nvme".into(),
            }))
            .await
            .expect("get_pool ok");
        let pool = resp.into_inner();
        assert_eq!(pool.pool_name, "fast-nvme");
        assert_eq!(pool.capacity_bytes, 10_000_000_000);
        assert_eq!(pool.used_bytes, 2_500_000_000);
    }

    #[tokio::test]
    async fn get_pool_missing_returns_not_found() {
        let (svc, _) = fixture_with_pools();
        let err = svc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: "no-such-pool".into(),
            }))
            .await
            .expect_err("should be not_found");
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn cluster_status_aggregates_across_pools_and_nodes() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .cluster_status(Request::new(pb::ClusterStatusRequest::default()))
            .await
            .expect("cluster_status ok");
        let cs = resp.into_inner();
        assert_eq!(cs.node_count, 3);
        assert_eq!(cs.pool_count, 2);
        assert_eq!(cs.total_capacity_bytes, 110_000_000_000);
        assert_eq!(cs.used_capacity_bytes, 92_500_000_000);
        assert_eq!(cs.shard_count, 1, "single-shard until W5");
        assert_eq!(cs.leader_node, "node-1");
        assert!(!cs.maintenance_mode);
        assert!(!cs.sampled_at.is_empty());
    }

    #[tokio::test]
    async fn cluster_status_without_chunk_store_is_failed_precondition() {
        let svc = fixture_empty();
        let err = svc
            .cluster_status(Request::new(pb::ClusterStatusRequest::default()))
            .await
            .expect_err("should fail_precondition");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn pool_status_critical_when_above_85pct_fill() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .pool_status(Request::new(pb::PoolStatusRequest {
                pool_name: "bulk-hdd".into(),
            }))
            .await
            .expect("pool_status ok");
        let ps = resp.into_inner();
        assert_eq!(
            ps.capacity_state, "critical",
            "90% > critical threshold of 85%"
        );
        let pool = ps.pool.expect("inner pool present");
        assert_eq!(pool.pool_name, "bulk-hdd");
        assert_eq!(ps.devices.len(), 1);
    }

    #[tokio::test]
    async fn pool_status_ok_when_under_warning_pct() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .pool_status(Request::new(pb::PoolStatusRequest {
                pool_name: "fast-nvme".into(),
            }))
            .await
            .expect("pool_status ok");
        assert_eq!(
            resp.into_inner().capacity_state,
            "ok",
            "25% well below 70% warning"
        );
    }

    #[tokio::test]
    async fn list_shards_returns_bootstrap_shard() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .list_shards(Request::new(pb::ListShardsRequest::default()))
            .await
            .expect("list_shards ok");
        let shards = resp.into_inner().shards;
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].leader_node, "node-1");
        assert_eq!(shards[0].members, vec!["node-1", "node-2", "node-3"]);
    }

    #[tokio::test]
    async fn list_shards_filters_by_unknown_tenant_returns_empty() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .list_shards(Request::new(pb::ListShardsRequest {
                tenant_id: uuid::Uuid::from_u128(99).to_string(),
            }))
            .await
            .expect("list_shards ok");
        assert!(resp.into_inner().shards.is_empty());
    }

    #[tokio::test]
    async fn get_shard_returns_bootstrap_when_id_matches() {
        let (svc, _) = fixture_with_pools();
        let id = uuid::Uuid::from_u128(0xDEAD).to_string();
        let resp = svc
            .get_shard(Request::new(pb::GetShardRequest {
                shard_id: id.clone(),
            }))
            .await
            .expect("get_shard ok");
        let shard = resp.into_inner();
        assert_eq!(shard.shard_id, id);
        assert_eq!(shard.members.len(), 3);
    }

    #[tokio::test]
    async fn get_shard_unknown_id_returns_not_found() {
        let (svc, _) = fixture_with_pools();
        let err = svc
            .get_shard(Request::new(pb::GetShardRequest {
                shard_id: uuid::Uuid::nil().to_string(),
            }))
            .await
            .expect_err("not_found");
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn get_shard_empty_id_returns_invalid_argument() {
        let (svc, _) = fixture_with_pools();
        let err = svc
            .get_shard(Request::new(pb::GetShardRequest::default()))
            .await
            .expect_err("invalid_argument");
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn list_repairs_empty_when_no_records() {
        let (svc, _) = fixture_with_pools();
        let resp = svc
            .list_repairs(Request::new(pb::ListRepairsRequest::default()))
            .await
            .expect("list_repairs ok");
        assert!(resp.into_inner().repairs.is_empty());
    }

    #[tokio::test]
    async fn list_repairs_returns_recent_records_newest_first() {
        let (svc, tracker) = fixture_with_pools();
        for i in 1u8..=5 {
            tracker.start(RepairTrigger::Scrub, ChunkId([i; 32]), format!("scrub {i}"));
        }
        let resp = svc
            .list_repairs(Request::new(pb::ListRepairsRequest::default()))
            .await
            .expect("list_repairs ok");
        let repairs = resp.into_inner().repairs;
        assert_eq!(repairs.len(), 5);
        // Newest first → chunk 0x05 leads.
        assert!(repairs[0].chunk_id_hex.starts_with("0505"));
        assert_eq!(repairs[0].trigger, "scrub");
        assert_eq!(repairs[0].state, "in_progress");
    }

    #[tokio::test]
    async fn list_repairs_clamps_limit_to_1000() {
        let (svc, tracker) = fixture_with_pools();
        for i in 0u8..200 {
            tracker.start(RepairTrigger::Scrub, ChunkId([i; 32]), "");
        }
        let resp = svc
            .list_repairs(Request::new(pb::ListRepairsRequest { limit: 50 }))
            .await
            .expect("list_repairs ok");
        assert_eq!(resp.into_inner().repairs.len(), 50);

        let resp = svc
            .list_repairs(Request::new(pb::ListRepairsRequest { limit: 9999 }))
            .await
            .expect("list_repairs ok");
        // Stored: 200; default cap=1000; we stored less than cap.
        assert_eq!(resp.into_inner().repairs.len(), 200);
    }

    #[tokio::test]
    async fn list_repairs_without_tracker_is_empty_not_unimplemented() {
        let svc = fixture_empty();
        let resp = svc
            .list_repairs(Request::new(pb::ListRepairsRequest::default()))
            .await
            .expect("list_repairs ok");
        assert!(resp.into_inner().repairs.is_empty());
    }

    // ====================================================================
    // Pending workstreams — Unimplemented ledger
    // ====================================================================

    // -- W3 (TuningParams) — landed --

    /// Build a fixture with a wired in-memory tuning store.
    fn fixture_with_tuning() -> StorageAdminGrpc {
        StorageAdminGrpc::for_tests().with_tuning_store(crate::tuning::TuningStore::in_memory())
    }

    #[tokio::test]
    async fn get_tuning_params_returns_defaults_on_fresh_store() {
        let r = fixture_with_tuning()
            .get_tuning_params(Request::new(pb::GetTuningParamsRequest::default()))
            .await
            .expect("ok");
        let p = r.into_inner();
        // Spot-check 3 fields against ADR-025 defaults; the
        // tuning module's tests assert all 8.
        assert_eq!(p.compaction_rate_mb_s, 100);
        assert_eq!(p.scrub_interval_h, 168);
        assert_eq!(p.raft_snapshot_interval, 10_000);
    }

    #[tokio::test]
    async fn get_tuning_params_without_dep_returns_failed_precondition() {
        let r = fixture_empty()
            .get_tuning_params(Request::new(pb::GetTuningParamsRequest::default()))
            .await;
        let err = r.expect_err("must error");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn set_tuning_params_round_trips_through_get() {
        let grpc = fixture_with_tuning();
        let new_params = pb::TuningParams {
            compaction_rate_mb_s: 250,
            gc_interval_s: 600,
            rebalance_rate_mb_s: 100,
            scrub_interval_h: 48,
            max_concurrent_repairs: 8,
            stream_proc_poll_ms: 50,
            inline_threshold_bytes: 8192,
            raft_snapshot_interval: 25_000,
        };
        grpc.set_tuning_params(Request::new(pb::SetTuningParamsRequest {
            params: Some(new_params),
        }))
        .await
        .expect("ok");
        let g = grpc
            .get_tuning_params(Request::new(pb::GetTuningParamsRequest::default()))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(g, new_params);
    }

    #[tokio::test]
    async fn set_tuning_params_rejects_out_of_range_with_invalid_argument() {
        let grpc = fixture_with_tuning();
        let bad = pb::TuningParams {
            compaction_rate_mb_s: 5, // min 10
            ..pb::TuningParams::default()
        };
        let r = grpc
            .set_tuning_params(Request::new(pb::SetTuningParamsRequest {
                params: Some(bad),
            }))
            .await;
        let err = r.expect_err("must reject");
        assert_eq!(err.code(), Code::InvalidArgument);
        assert!(err.message().contains("compaction_rate_mb_s"));
    }

    #[tokio::test]
    async fn set_tuning_params_without_params_returns_invalid_argument() {
        let grpc = fixture_with_tuning();
        let r = grpc
            .set_tuning_params(Request::new(pb::SetTuningParamsRequest { params: None }))
            .await;
        let err = r.expect_err("must reject");
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn set_tuning_params_without_dep_returns_failed_precondition() {
        let r = fixture_empty()
            .set_tuning_params(Request::new(pb::SetTuningParamsRequest {
                params: Some(pb::TuningParams::default()),
            }))
            .await;
        let err = r.expect_err("must error");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    /// Cardinality cross-check: every default-valued protobuf
    /// field is treated as "out of range" (most have lower bounds
    /// > 0). This catches the bug where a client forgets to
    /// populate one field and gets silent zero-init behavior.
    #[tokio::test]
    async fn set_tuning_params_rejects_unset_proto_default() {
        let grpc = fixture_with_tuning();
        let r = grpc
            .set_tuning_params(Request::new(pb::SetTuningParamsRequest {
                params: Some(pb::TuningParams::default()),
            }))
            .await;
        let err = r.expect_err("zero-init should reject");
        assert_eq!(err.code(), Code::InvalidArgument);
    }

    // -- W4 (Simple mutations) — landed --

    fn fixture_with_maintenance() -> (StorageAdminGrpc, Arc<MaintenanceMode>) {
        let m = Arc::new(MaintenanceMode::new());
        let grpc = StorageAdminGrpc::for_tests()
            .with_bootstrap_shard(ShardId(uuid::Uuid::from_u128(99)))
            .with_maintenance(Arc::clone(&m));
        (grpc, m)
    }

    fn fixture_with_evacuations() -> (StorageAdminGrpc, Arc<EvacuationRegistry>) {
        let r = Arc::new(EvacuationRegistry::new());
        let grpc = StorageAdminGrpc::for_tests().with_evacuations(Arc::clone(&r));
        (grpc, r)
    }

    #[tokio::test]
    async fn set_shard_maintenance_flips_flag_in_shared_store() {
        let (grpc, store) = fixture_with_maintenance();
        let shard = grpc.bootstrap_shard;
        assert!(!store.is_in_maintenance(shard));
        grpc.set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest {
            shard_id: shard.0.to_string(),
            enabled: true,
        }))
        .await
        .expect("ok");
        assert!(
            store.is_in_maintenance(shard),
            "admin RPC must flip the same atomic the data path consults"
        );
    }

    #[tokio::test]
    async fn set_shard_maintenance_disable_clears_flag() {
        let (grpc, store) = fixture_with_maintenance();
        store.set(grpc.bootstrap_shard, true);
        grpc.set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest {
            shard_id: grpc.bootstrap_shard.0.to_string(),
            enabled: false,
        }))
        .await
        .expect("ok");
        assert!(!store.is_in_maintenance(grpc.bootstrap_shard));
    }

    #[tokio::test]
    async fn set_shard_maintenance_unknown_shard_returns_not_found() {
        let (grpc, _) = fixture_with_maintenance();
        let r = grpc
            .set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest {
                shard_id: uuid::Uuid::from_u128(7).to_string(),
                enabled: true,
            }))
            .await;
        let err = r.expect_err("must error");
        assert_eq!(err.code(), Code::NotFound);
    }

    #[tokio::test]
    async fn set_shard_maintenance_invalid_uuid_returns_invalid_argument() {
        let (grpc, _) = fixture_with_maintenance();
        let r = grpc
            .set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest {
                shard_id: "not-a-uuid".into(),
                enabled: true,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn set_shard_maintenance_empty_id_returns_invalid_argument() {
        let (grpc, _) = fixture_with_maintenance();
        let r = grpc
            .set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest {
                shard_id: String::new(),
                enabled: false,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn set_shard_maintenance_without_dep_returns_failed_precondition() {
        let r = fixture_empty()
            .set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest {
                shard_id: uuid::Uuid::nil().to_string(),
                enabled: true,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn cancel_evacuation_cancels_registered_progress() {
        use kiseki_chunk::evacuation::EvacuationProgress;
        let (grpc, registry) = fixture_with_evacuations();
        let progress = Arc::new(EvacuationProgress::new([0xAB; 16], 100));
        registry.register("evac-001".into(), Arc::clone(&progress));
        grpc.cancel_evacuation(Request::new(pb::CancelEvacuationRequest {
            evacuation_id: "evac-001".into(),
        }))
        .await
        .expect("ok");
        assert!(progress
            .cancelled
            .load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancel_evacuation_unknown_id_returns_not_found() {
        let (grpc, _) = fixture_with_evacuations();
        let r = grpc
            .cancel_evacuation(Request::new(pb::CancelEvacuationRequest {
                evacuation_id: "no-such-evac".into(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::NotFound);
    }

    #[tokio::test]
    async fn cancel_evacuation_empty_id_returns_invalid_argument() {
        let (grpc, _) = fixture_with_evacuations();
        let r = grpc
            .cancel_evacuation(Request::new(pb::CancelEvacuationRequest {
                evacuation_id: String::new(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn cancel_evacuation_without_dep_returns_failed_precondition() {
        let r = fixture_empty()
            .cancel_evacuation(Request::new(pb::CancelEvacuationRequest {
                evacuation_id: "x".into(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn trigger_scrub_without_dep_returns_failed_precondition() {
        let r = fixture_empty()
            .trigger_scrub(Request::new(pb::TriggerScrubRequest::default()))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn repair_chunk_without_dep_returns_failed_precondition() {
        let r = fixture_empty()
            .repair_chunk(Request::new(pb::AdminRepairChunkRequest::default()))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn repair_chunk_invalid_hex_returns_invalid_argument() {
        // We need a fixture with scrub wired to even get past the
        // dep check. The scrub scheduler is heavy to construct in
        // a unit test, so the cheapest path is to set up a fake by
        // testing parse_chunk_id_hex directly.
        let r = parse_chunk_id_hex("not-32-bytes");
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[test]
    fn parse_chunk_id_hex_round_trips_with_hex_encode() {
        let bytes: [u8; 32] = [0x42; 32];
        let s = hex_encode_chunk(bytes);
        let parsed = parse_chunk_id_hex(&s).expect("ok");
        assert_eq!(parsed.0, bytes);
    }

    #[test]
    fn parse_chunk_id_hex_rejects_non_hex_chars() {
        // Right length, but contains a non-hex char.
        let bad = "g".repeat(64);
        assert!(parse_chunk_id_hex(&bad).is_err());
    }

    #[test]
    fn parse_shard_id_round_trips_with_uuid() {
        let u = uuid::Uuid::from_u128(0xdead_beef);
        let s = parse_shard_id(&u.to_string()).expect("ok");
        assert_eq!(s.0, u);
    }

    // -- W5 (Pool / device / shard mutations) — landed --

    /// Build a fixture with `chunk_store` + `evacuations` +
    /// `pool_mutations` all wired (W5 needs all three for the
    /// full surface). Returns the chunk store handle for tests
    /// that want to inspect post-RPC state.
    fn fixture_for_w5() -> (
        StorageAdminGrpc,
        Arc<dyn kiseki_chunk::AsyncChunkOps>,
        Arc<EvacuationRegistry>,
    ) {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool {
            name: "primary".into(),
            durability: DurabilityStrategy::Replication { copies: 3 },
            capacity_bytes: 10_000,
            used_bytes: 0,
            device_class: kiseki_chunk::pool::DeviceClass::NvmeSsd,
            devices: vec![PoolDevice {
                id: "dev-1".into(),
                online: true,
            }],
        });
        let chunk_store: Arc<dyn kiseki_chunk::AsyncChunkOps> = Arc::new(SyncBridge::new(store));
        let evacuations = Arc::new(EvacuationRegistry::new());
        let grpc = StorageAdminGrpc::for_tests()
            .with_chunk_store(Arc::clone(&chunk_store))
            .with_evacuations(Arc::clone(&evacuations))
            .with_pool_mutations(crate::pool_overrides::PoolMutationDeps::new());
        (grpc, chunk_store, evacuations)
    }

    #[tokio::test]
    async fn add_device_appends_to_pool_visible_via_list_devices() {
        let (grpc, _store, _) = fixture_for_w5();
        grpc.add_device(Request::new(pb::AddDeviceRequest {
            pool_name: "primary".into(),
            device_id: "new-dev".into(),
            capacity_bytes: 0,
            device_class: String::new(),
        }))
        .await
        .expect("ok");
        let r = grpc
            .list_devices(Request::new(pb::ListDevicesRequest::default()))
            .await
            .expect("ok");
        let ids: Vec<String> = r
            .into_inner()
            .devices
            .into_iter()
            .map(|d| d.device_id)
            .collect();
        assert!(ids.iter().any(|i| i == "new-dev"));
    }

    #[tokio::test]
    async fn add_device_duplicate_returns_already_exists() {
        let (grpc, _, _) = fixture_for_w5();
        grpc.add_device(Request::new(pb::AddDeviceRequest {
            pool_name: "primary".into(),
            device_id: "dev-1".into(), // already present in fixture
            capacity_bytes: 0,
            device_class: String::new(),
        }))
        .await
        .expect_err("should already exist");
    }

    #[tokio::test]
    async fn add_device_unknown_pool_returns_not_found() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .add_device(Request::new(pb::AddDeviceRequest {
                pool_name: "no-such".into(),
                device_id: "x".into(),
                capacity_bytes: 0,
                device_class: String::new(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::NotFound);
    }

    #[tokio::test]
    async fn remove_device_strips_from_pool() {
        let (grpc, _, _) = fixture_for_w5();
        grpc.remove_device(Request::new(pb::RemoveDeviceRequest {
            device_id: "dev-1".into(),
        }))
        .await
        .expect("ok");
        let r = grpc
            .list_devices(Request::new(pb::ListDevicesRequest::default()))
            .await
            .expect("ok");
        assert!(r.into_inner().devices.is_empty());
    }

    #[tokio::test]
    async fn remove_device_unknown_returns_not_found() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .remove_device(Request::new(pb::RemoveDeviceRequest {
                device_id: "no-such".into(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::NotFound);
    }

    #[tokio::test]
    async fn evacuate_device_registers_in_evacuations() {
        let (grpc, _, registry) = fixture_for_w5();
        let r = grpc
            .evacuate_device(Request::new(pb::EvacuateDeviceRequest {
                device_id: "dev-1".into(),
                throughput_mb_s: 0,
            }))
            .await
            .expect("ok")
            .into_inner();
        assert!(!r.evacuation_id.is_empty());
        assert!(registry.cancel(&r.evacuation_id), "id must be registered");
    }

    #[tokio::test]
    async fn evacuate_device_unknown_returns_not_found() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .evacuate_device(Request::new(pb::EvacuateDeviceRequest {
                device_id: "no-such".into(),
                throughput_mb_s: 0,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::NotFound);
    }

    #[tokio::test]
    async fn create_pool_then_get_pool_round_trips() {
        let (grpc, _, _) = fixture_for_w5();
        grpc.create_pool(Request::new(pb::CreatePoolRequest {
            pool_name: "secondary".into(),
            device_class: "ssd".into(),
            durability_kind: "replication".into(),
            replication_copies: 3,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            initial_capacity_bytes: 1_000_000,
        }))
        .await
        .expect("ok");
        let r = grpc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: "secondary".into(),
            }))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(r.replication_copies, 3);
        assert_eq!(r.capacity_bytes, 1_000_000);
    }

    #[tokio::test]
    async fn create_pool_duplicate_returns_already_exists() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .create_pool(Request::new(pb::CreatePoolRequest {
                pool_name: "primary".into(),
                device_class: "nvme".into(),
                durability_kind: "replication".into(),
                replication_copies: 2,
                ec_data_shards: 0,
                ec_parity_shards: 0,
                initial_capacity_bytes: 0,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::AlreadyExists);
    }

    #[tokio::test]
    async fn create_pool_invalid_durability_returns_invalid_argument() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .create_pool(Request::new(pb::CreatePoolRequest {
                pool_name: "x".into(),
                device_class: "nvme".into(),
                durability_kind: "fast-and-loose".into(),
                replication_copies: 1,
                ec_data_shards: 0,
                ec_parity_shards: 0,
                initial_capacity_bytes: 0,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn set_pool_durability_swaps_strategy_on_empty_pool() {
        let (grpc, _, _) = fixture_for_w5();
        grpc.set_pool_durability(Request::new(pb::SetPoolDurabilityRequest {
            pool_name: "primary".into(),
            durability_kind: "erasure_coding".into(),
            replication_copies: 0,
            ec_data_shards: 4,
            ec_parity_shards: 2,
        }))
        .await
        .expect("ok");
        let r = grpc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: "primary".into(),
            }))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(r.durability_kind, "erasure_coding");
        assert_eq!(r.ec_data_shards, 4);
        assert_eq!(r.ec_parity_shards, 2);
    }

    #[tokio::test]
    async fn set_pool_thresholds_writes_through_to_get_pool() {
        let (grpc, _, _) = fixture_for_w5();
        grpc.set_pool_thresholds(Request::new(pb::SetPoolThresholdsRequest {
            pool_name: "primary".into(),
            warning_threshold_pct: 60,
            critical_threshold_pct: 80,
            readonly_threshold_pct: 90,
            target_fill_pct: 75,
        }))
        .await
        .expect("ok");
        let r = grpc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: "primary".into(),
            }))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(r.warning_threshold_pct, 60);
        assert_eq!(r.critical_threshold_pct, 80);
        assert_eq!(r.readonly_threshold_pct, 90);
        assert_eq!(r.target_fill_pct, 75);
    }

    #[tokio::test]
    async fn set_pool_thresholds_validates_ordering() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .set_pool_thresholds(Request::new(pb::SetPoolThresholdsRequest {
                pool_name: "primary".into(),
                warning_threshold_pct: 90,
                critical_threshold_pct: 80, // less than warning — invalid
                readonly_threshold_pct: 95,
                target_fill_pct: 70,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn rebalance_pool_returns_unique_id_per_call() {
        let (grpc, _, _) = fixture_for_w5();
        let a = grpc
            .rebalance_pool(Request::new(pb::RebalancePoolRequest {
                pool_name: "primary".into(),
                throughput_mb_s: 0,
            }))
            .await
            .expect("ok")
            .into_inner();
        let b = grpc
            .rebalance_pool(Request::new(pb::RebalancePoolRequest {
                pool_name: "primary".into(),
                throughput_mb_s: 50,
            }))
            .await
            .expect("ok")
            .into_inner();
        assert_ne!(a.rebalance_id, b.rebalance_id);
    }

    #[tokio::test]
    async fn rebalance_pool_unknown_returns_not_found() {
        let (grpc, _, _) = fixture_for_w5();
        let r = grpc
            .rebalance_pool(Request::new(pb::RebalancePoolRequest {
                pool_name: "no-such".into(),
                throughput_mb_s: 0,
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::NotFound);
    }

    /// Spin a `MemShardStore` and wire it into the admin handler;
    /// `SplitShard` needs a real `LogOps` so the trait method
    /// dispatches to `LogStore::split_shard`.
    fn fixture_with_log() -> (StorageAdminGrpc, ShardId) {
        use kiseki_log::{shard::ShardConfig, MemShardStore};
        let log: Arc<dyn LogOps + Send + Sync> = Arc::new(MemShardStore::new());
        let shard = ShardId(uuid::Uuid::from_u128(42));
        log.create_shard(
            shard,
            kiseki_common::ids::OrgId(uuid::Uuid::nil()),
            kiseki_common::ids::NodeId(0),
            ShardConfig::default(),
        );
        let grpc = StorageAdminGrpc::for_tests().with_log_store(log);
        (grpc, shard)
    }

    #[tokio::test]
    async fn split_shard_returns_new_shard_id() {
        let (grpc, shard) = fixture_with_log();
        let r = grpc
            .split_shard(Request::new(pb::SplitShardRequest {
                shard_id: shard.0.to_string(),
                pivot_key: String::new(),
            }))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(r.left_shard_id, shard.0.to_string());
        assert!(!r.right_shard_id.is_empty());
        assert_ne!(r.left_shard_id, r.right_shard_id);
    }

    #[tokio::test]
    async fn split_shard_unknown_returns_not_found() {
        let (grpc, _) = fixture_with_log();
        let r = grpc
            .split_shard(Request::new(pb::SplitShardRequest {
                shard_id: uuid::Uuid::from_u128(999).to_string(),
                pivot_key: String::new(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::NotFound);
    }

    #[tokio::test]
    async fn merge_shards_validates_distinct_shards() {
        let (grpc, shard) = fixture_with_log();
        let r = grpc
            .merge_shards(Request::new(pb::MergeShardsRequest {
                left_shard_id: shard.0.to_string(),
                right_shard_id: shard.0.to_string(),
            }))
            .await;
        assert_eq!(r.expect_err("err").code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn merge_shards_returns_left_as_merged_id() {
        // Build a log with two shards.
        use kiseki_log::{shard::ShardConfig, MemShardStore};
        let log: Arc<dyn LogOps + Send + Sync> = Arc::new(MemShardStore::new());
        let left = ShardId(uuid::Uuid::from_u128(1));
        let right = ShardId(uuid::Uuid::from_u128(2));
        for s in [left, right] {
            log.create_shard(
                s,
                kiseki_common::ids::OrgId(uuid::Uuid::nil()),
                kiseki_common::ids::NodeId(0),
                ShardConfig::default(),
            );
        }
        let grpc = StorageAdminGrpc::for_tests().with_log_store(log);
        let r = grpc
            .merge_shards(Request::new(pb::MergeShardsRequest {
                left_shard_id: left.0.to_string(),
                right_shard_id: right.0.to_string(),
            }))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(r.merged_shard_id, left.0.to_string());
    }

    // -- W7 --

    #[tokio::test]
    async fn device_health_unimplemented_until_w7() {
        let r = fixture_empty()
            .device_health(Request::new(pb::DeviceHealthRequest::default()))
            .await;
        assert_unimplemented_with_workstream("DeviceHealth", "W7", r);
    }

    #[tokio::test]
    async fn io_stats_unimplemented_until_w7() {
        let r = fixture_empty()
            .io_stats(Request::new(pb::IoStatsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("IOStats", "W7", r);
    }

    // -- Cardinality cross-checks --

    /// Mechanical guard: `storage_admin.proto` must declare exactly
    /// 26 rpcs. ADR-025 §"Admin API surface" lists 25; ADR-034
    /// adds `MergeShards`.
    #[test]
    fn proto_declares_exactly_26_rpcs() {
        let proto_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent() // crates/
            .unwrap()
            .parent() // workspace root
            .unwrap()
            .join("specs/architecture/proto/kiseki/v1/storage_admin.proto");
        let src = std::fs::read_to_string(&proto_path).expect("read storage_admin.proto");
        let rpc_count = src
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with("rpc "))
            .count();
        assert_eq!(
            rpc_count, 26,
            "storage_admin.proto must declare exactly 26 rpcs; found {rpc_count}",
        );
    }

    /// Mechanical guard: total RPC test coverage in this module
    /// must equal 26 (one test per RPC, regardless of whether the
    /// test asserts a real impl or `_unimplemented_until_w*`).
    /// Catches the case where a workstream lands an impl but its
    /// scaffolding test is removed without a real-impl test
    /// taking its place.
    #[test]
    fn rpc_test_coverage_is_complete() {
        let this_file = include_str!("storage_admin.rs");
        // Count tests that target an RPC. RPC tests live in functions
        // whose names match an RPC name (snake_case) followed by
        // either `_unimplemented_until_w*` OR a real-behavior suffix.
        // The registry of which suffix corresponds to which RPC
        // lives below — when adding an RPC test, add its name here.
        let rpc_name_substrings: &[&str] = &[
            "list_devices",
            "get_device",
            "add_device",
            "remove_device",
            "evacuate_device",
            "cancel_evacuation",
            "list_pools",
            "get_pool",
            "create_pool",
            "set_pool_durability",
            "set_pool_thresholds",
            "rebalance_pool",
            "get_tuning_params",
            "set_tuning_params",
            "cluster_status",
            "pool_status",
            "device_health",
            "io_stats",
            "list_shards",
            "get_shard",
            "split_shard",
            "merge_shards",
            "set_shard_maintenance",
            "trigger_scrub",
            "repair_chunk",
            "list_repairs",
        ];
        assert_eq!(
            rpc_name_substrings.len(),
            26,
            "RPC name list must enumerate exactly 26",
        );
        for rpc in rpc_name_substrings {
            let covered = this_file
                .lines()
                .map(str::trim_start)
                .any(|l| l.starts_with("async fn ") && l.contains(rpc));
            assert!(
                covered,
                "RPC `{rpc}` has no test fn matching it in this module",
            );
        }
    }

    // -- Tracing + metrics observability --

    /// Build a fixture with a real Prometheus counter wired so tests
    /// can assert on `(rpc, outcome)` label-set increments.
    fn fixture_with_counter() -> (StorageAdminGrpc, Arc<IntCounterVec>) {
        let counter = Arc::new(
            IntCounterVec::new(
                prometheus::Opts::new("kiseki_storage_admin_calls_total_test", "test counter"),
                &["rpc", "outcome"],
            )
            .expect("metric"),
        );
        let (mut grpc, _) = fixture_with_pools();
        grpc = grpc.with_metrics(Arc::clone(&counter));
        (grpc, counter)
    }

    fn counter_value(c: &IntCounterVec, rpc: &str, outcome: &str) -> u64 {
        c.with_label_values(&[rpc, outcome]).get()
    }

    #[tokio::test]
    async fn metrics_increment_on_ok_outcome() {
        let (grpc, counter) = fixture_with_counter();
        grpc.list_pools(Request::new(pb::ListPoolsRequest::default()))
            .await
            .expect("ok");
        assert_eq!(
            counter_value(&counter, "StorageAdminService.ListPools", "ok"),
            1,
        );
    }

    #[tokio::test]
    async fn metrics_increment_on_client_error_outcome() {
        let (grpc, counter) = fixture_with_counter();
        // Empty pool_name → InvalidArgument → "client_error" bucket.
        let r = grpc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: String::new(),
            }))
            .await;
        assert!(r.is_err());
        assert_eq!(
            counter_value(&counter, "StorageAdminService.GetPool", "client_error"),
            1,
        );
    }

    #[tokio::test]
    async fn metrics_increment_on_not_found_outcome() {
        let (grpc, counter) = fixture_with_counter();
        let r = grpc
            .get_pool(Request::new(pb::GetPoolRequest {
                pool_name: "no-such-pool".into(),
            }))
            .await;
        assert!(r.is_err());
        assert_eq!(
            counter_value(&counter, "StorageAdminService.GetPool", "client_error"),
            1,
        );
    }

    #[tokio::test]
    async fn metrics_increment_on_unimplemented_outcome() {
        let (grpc, counter) = fixture_with_counter();
        // Pick a still-unimplemented RPC (W7 territory — streaming
        // RPCs land last) so this test continues to exercise the
        // `unimplemented` bucket.
        let r = grpc
            .device_health(Request::new(pb::DeviceHealthRequest::default()))
            .await;
        assert!(r.is_err());
        assert_eq!(
            counter_value(
                &counter,
                "StorageAdminService.DeviceHealth",
                "unimplemented"
            ),
            1,
        );
    }

    #[tokio::test]
    async fn metrics_increment_on_failed_precondition_outcome() {
        // chunk_store dep not wired → FailedPrecondition → client_error.
        let counter = Arc::new(
            IntCounterVec::new(
                prometheus::Opts::new("kiseki_storage_admin_test2", "test"),
                &["rpc", "outcome"],
            )
            .expect("metric"),
        );
        let grpc = StorageAdminGrpc::for_tests().with_metrics(Arc::clone(&counter));
        let r = grpc
            .list_pools(Request::new(pb::ListPoolsRequest::default()))
            .await;
        assert!(r.is_err());
        assert_eq!(
            counter_value(&counter, "StorageAdminService.ListPools", "client_error"),
            1,
        );
    }

    #[test]
    fn outcome_classifier_buckets_status_codes_correctly() {
        assert_eq!(
            StorageAdminGrpc::outcome_for(&Status::invalid_argument("x")),
            "client_error",
        );
        assert_eq!(
            StorageAdminGrpc::outcome_for(&Status::not_found("x")),
            "client_error",
        );
        assert_eq!(
            StorageAdminGrpc::outcome_for(&Status::failed_precondition("x")),
            "client_error",
        );
        assert_eq!(
            StorageAdminGrpc::outcome_for(&Status::unimplemented("x")),
            "unimplemented",
        );
        assert_eq!(
            StorageAdminGrpc::outcome_for(&Status::internal("x")),
            "server_error",
        );
        assert_eq!(
            StorageAdminGrpc::outcome_for(&Status::unavailable("x")),
            "server_error",
        );
    }

    #[tokio::test]
    async fn handler_without_metrics_dep_runs_silently() {
        // No `with_metrics` → record_outcome must no-op, not panic.
        let (grpc, _) = fixture_with_pools();
        // (no .with_metrics() so calls_total stays None)
        grpc.list_pools(Request::new(pb::ListPoolsRequest::default()))
            .await
            .expect("ok");
    }

    /// Mechanical guard: every implemented (non-`unimpl`) RPC body
    /// must call `with_obs(...)` so spans + metric bumps are
    /// uniform. The unimplemented stubs go through `self.unimpl(...)`
    /// which itself bumps the counter, so they're covered without
    /// `with_obs`.
    #[test]
    fn every_implemented_rpc_uses_with_obs() {
        let src = include_str!("storage_admin.rs");
        // Names of the 9 W2 implemented RPCs (snake_case).
        let implemented: &[&str] = &[
            "list_devices",
            "get_device",
            "list_pools",
            "get_pool",
            "cluster_status",
            "pool_status",
            "list_shards",
            "get_shard",
            "list_repairs",
            // ADR-025 W3 — TuningParams.
            "get_tuning_params",
            "set_tuning_params",
            // ADR-025 W4 — simple mutations.
            "set_shard_maintenance",
            "cancel_evacuation",
            "trigger_scrub",
            "repair_chunk",
            // ADR-025 W5 — pool/device/shard mutations.
            "add_device",
            "remove_device",
            "evacuate_device",
            "create_pool",
            "set_pool_durability",
            "set_pool_thresholds",
            "rebalance_pool",
            "split_shard",
            "merge_shards",
        ];
        for rpc in implemented {
            // Locate the `async fn <rpc>` line, then look ahead a
            // bounded number of lines for `self.with_obs(`. Bound is
            // generous to allow long signatures + doc comments.
            let lines: Vec<&str> = src.lines().collect();
            let idx = lines
                .iter()
                .position(|l| l.trim_start().starts_with(&format!("async fn {rpc}(")))
                .unwrap_or_else(|| panic!("no `async fn {rpc}(` in storage_admin.rs"));
            let window = &lines[idx..(idx + 20).min(lines.len())];
            let uses_with_obs = window.iter().any(|l| l.contains("self.with_obs("));
            assert!(
                uses_with_obs,
                "RPC `{rpc}` body must invoke `self.with_obs(...)` for tracing + metrics",
            );
        }
    }
}
