//! gRPC `StorageAdminService` skeleton (ADR-025, W1).
//!
//! Operator-facing API for storage subsystem management — devices,
//! pools, shards, tuning parameters, observability streams, and
//! repair / scrub. Disjoint from `AdminService` (snapshots, ADR-016)
//! and `ControlService` (tenant-facing).
//!
//! W1 lands the proto + the wiring; every RPC body returns
//! `UNIMPLEMENTED` with a message naming the workstream that will
//! land it. Subsequent workstreams (W2-W7 per
//! `specs/implementation/adr-025-storage-admin-api.md`) replace the
//! `unimplemented` body with a real impl AND remove the matching
//! assertion in `tests` below — turning the test module into a
//! running ledger of what's left.
//!
//! The struct holds `Arc` handles to every dependency the future
//! RPCs need (chunk store, cluster chunk store, view store,
//! scrub scheduler, raft handle, tuning state). W1 leaves them as
//! `Option`s seeded with `None`; later workstreams flip each one
//! to `Some` as they wire it in.

use async_trait::async_trait;
use kiseki_proto::v1 as pb;
use kiseki_proto::v1::storage_admin_service_server::StorageAdminService;
use std::pin::Pin;
// Re-exported from tonic so kiseki-server doesn't need a direct
// tokio-stream dep — the trait the proto generates uses this exact
// path for its associated `Stream` types.
use tonic::codegen::tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// Handler for `StorageAdminService`. Holds optional `Arc` deps
/// the future workstreams will populate.
pub struct StorageAdminGrpc {
    // W2 — read-only RPCs need these
    // chunk_store: Option<Arc<...>>,
    // cluster_chunk_store: Option<Arc<...>>,
    //
    // W3 — tuning param state
    // tuning: Option<Arc<TuningState>>,
    //
    // W4 / W5 — mutating RPCs need raft handle + scrub scheduler
    // raft: Option<Arc<...>>,
    // scrub_scheduler: Option<Arc<...>>,
    //
    // W7 — streaming RPCs subscribe to broadcast channels
    // io_stats_bus: Option<broadcast::Sender<pb::IoStatsEvent>>,
    // device_health_bus: Option<broadcast::Sender<pb::DeviceHealthEvent>>,
    _phantom: std::marker::PhantomData<()>,
}

impl Default for StorageAdminGrpc {
    fn default() -> Self {
        Self::for_tests()
    }
}

impl StorageAdminGrpc {
    /// Construct the empty-deps skeleton. Used at runtime startup
    /// during W1; W2 onwards replaces this with a richer constructor
    /// that wires the real deps from `runtime.rs`.
    #[must_use]
    pub fn from_runtime() -> Self {
        Self::for_tests()
    }

    /// Construct with no deps wired — used by the scaffolding tests
    /// below and re-exported as the W1 default. Returns
    /// `UNIMPLEMENTED` from every RPC.
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }

    fn unimpl(rpc: &str, workstream: &str, what: &str) -> Status {
        Status::unimplemented(format!(
            "StorageAdminService.{rpc}: ADR-025 {workstream} — {what}",
        ))
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
        _req: Request<pb::ListDevicesRequest>,
    ) -> Result<Response<pb::ListDevicesResponse>, Status> {
        Err(Self::unimpl(
            "ListDevices",
            "W2",
            "read-only RPC; flatten chunk_store.pools().*.devices",
        ))
    }

    async fn get_device(
        &self,
        _req: Request<pb::GetDeviceRequest>,
    ) -> Result<Response<pb::DeviceInfo>, Status> {
        Err(Self::unimpl(
            "GetDevice",
            "W2",
            "read-only RPC; chunk_store.find_device(id) helper to add",
        ))
    }

    async fn add_device(
        &self,
        _req: Request<pb::AddDeviceRequest>,
    ) -> Result<Response<pb::AddDeviceResponse>, Status> {
        Err(Self::unimpl(
            "AddDevice",
            "W5",
            "Raft-coordinated; DeviceAdded delta on cluster control shard",
        ))
    }

    async fn remove_device(
        &self,
        _req: Request<pb::RemoveDeviceRequest>,
    ) -> Result<Response<pb::RemoveDeviceResponse>, Status> {
        Err(Self::unimpl(
            "RemoveDevice",
            "W5",
            "Raft-coordinated; FailedPrecondition when device has chunks",
        ))
    }

    async fn evacuate_device(
        &self,
        _req: Request<pb::EvacuateDeviceRequest>,
    ) -> Result<Response<pb::EvacuateDeviceResponse>, Status> {
        Err(Self::unimpl(
            "EvacuateDevice",
            "W5",
            "Raft-coordinated; hands off to drain orchestrator (ADR-035)",
        ))
    }

    async fn cancel_evacuation(
        &self,
        _req: Request<pb::CancelEvacuationRequest>,
    ) -> Result<Response<pb::CancelEvacuationResponse>, Status> {
        Err(Self::unimpl(
            "CancelEvacuation",
            "W4",
            "node-local flag flip on the drain orchestrator",
        ))
    }

    // --- Pool management ---

    async fn list_pools(
        &self,
        _req: Request<pb::ListPoolsRequest>,
    ) -> Result<Response<pb::ListPoolsResponse>, Status> {
        Err(Self::unimpl(
            "ListPools",
            "W2",
            "read-only RPC; chunk_store.pools() needs pub getter",
        ))
    }

    async fn get_pool(
        &self,
        _req: Request<pb::GetPoolRequest>,
    ) -> Result<Response<pb::PoolInfo>, Status> {
        Err(Self::unimpl(
            "GetPool",
            "W2",
            "read-only RPC; uses existing chunk_store.pool(name)",
        ))
    }

    async fn create_pool(
        &self,
        _req: Request<pb::CreatePoolRequest>,
    ) -> Result<Response<pb::CreatePoolResponse>, Status> {
        Err(Self::unimpl(
            "CreatePool",
            "W5",
            "Raft-coordinated; PoolCreated delta on cluster control shard",
        ))
    }

    async fn set_pool_durability(
        &self,
        _req: Request<pb::SetPoolDurabilityRequest>,
    ) -> Result<Response<pb::SetPoolDurabilityResponse>, Status> {
        Err(Self::unimpl(
            "SetPoolDurability",
            "W5",
            "Raft-coordinated; FailedPrecondition when pool non-empty (v1)",
        ))
    }

    async fn set_pool_thresholds(
        &self,
        _req: Request<pb::SetPoolThresholdsRequest>,
    ) -> Result<Response<pb::SetPoolThresholdsResponse>, Status> {
        Err(Self::unimpl(
            "SetPoolThresholds",
            "W5",
            "Raft-coordinated; PoolThresholdsChanged delta",
        ))
    }

    async fn rebalance_pool(
        &self,
        _req: Request<pb::RebalancePoolRequest>,
    ) -> Result<Response<pb::RebalancePoolResponse>, Status> {
        Err(Self::unimpl(
            "RebalancePool",
            "W5",
            "Raft-coordinated; spawns long-running task; status via PoolStatus",
        ))
    }

    // --- Performance tuning ---

    async fn get_tuning_params(
        &self,
        _req: Request<pb::GetTuningParamsRequest>,
    ) -> Result<Response<pb::TuningParams>, Status> {
        Err(Self::unimpl(
            "GetTuningParams",
            "W3",
            "reads from TuningState meta key in CompositionStore",
        ))
    }

    async fn set_tuning_params(
        &self,
        _req: Request<pb::SetTuningParamsRequest>,
    ) -> Result<Response<pb::SetTuningParamsResponse>, Status> {
        Err(Self::unimpl(
            "SetTuningParams",
            "W3",
            "Raft-coordinated; bounds-checked at deserialize",
        ))
    }

    // --- Cluster observability ---

    async fn cluster_status(
        &self,
        _req: Request<pb::ClusterStatusRequest>,
    ) -> Result<Response<pb::AdminClusterStatus>, Status> {
        Err(Self::unimpl(
            "ClusterStatus",
            "W2",
            "aggregate from raft membership + chunk_store + cluster_chunk_store",
        ))
    }

    async fn pool_status(
        &self,
        _req: Request<pb::PoolStatusRequest>,
    ) -> Result<Response<pb::AdminPoolStatus>, Status> {
        Err(Self::unimpl(
            "PoolStatus",
            "W2",
            "PoolInfo + per-device fill + capacity_state",
        ))
    }

    async fn device_health(
        &self,
        _req: Request<pb::DeviceHealthRequest>,
    ) -> Result<Response<Self::DeviceHealthStream>, Status> {
        Err(Self::unimpl(
            "DeviceHealth",
            "W7",
            "server-streaming; broadcast(1024) channel from chunk subsystem",
        ))
    }

    async fn io_stats(
        &self,
        _req: Request<pb::IoStatsRequest>,
    ) -> Result<Response<Self::IOStatsStream>, Status> {
        Err(Self::unimpl(
            "IOStats",
            "W7",
            "server-streaming; broadcast(1024) channel from chunk-cluster",
        ))
    }

    // --- Shard management ---

    async fn list_shards(
        &self,
        _req: Request<pb::ListShardsRequest>,
    ) -> Result<Response<pb::ListShardsResponse>, Status> {
        Err(Self::unimpl(
            "ListShards",
            "W2",
            "read-only RPC; cluster_chunk_store.cluster_shards()",
        ))
    }

    async fn get_shard(
        &self,
        _req: Request<pb::GetShardRequest>,
    ) -> Result<Response<pb::AdminShardInfo>, Status> {
        Err(Self::unimpl(
            "GetShard",
            "W2",
            "read-only RPC; cluster_shards() entry + raft state for shard",
        ))
    }

    async fn split_shard(
        &self,
        _req: Request<pb::SplitShardRequest>,
    ) -> Result<Response<pb::SplitShardResponse>, Status> {
        Err(Self::unimpl(
            "SplitShard",
            "W5",
            "reuses ADR-033 split machinery; RPC just triggers it",
        ))
    }

    async fn merge_shards(
        &self,
        _req: Request<pb::MergeShardsRequest>,
    ) -> Result<Response<pb::MergeShardsResponse>, Status> {
        Err(Self::unimpl(
            "MergeShards",
            "W5",
            "reuses ADR-034 merge; rejects cross-tenant with InvalidArgument",
        ))
    }

    async fn set_shard_maintenance(
        &self,
        _req: Request<pb::SetShardMaintenanceRequest>,
    ) -> Result<Response<pb::SetShardMaintenanceResponse>, Status> {
        Err(Self::unimpl(
            "SetShardMaintenance",
            "W4",
            "atomic flag in ClusterChunkServer; gates writes, allows reads",
        ))
    }

    // --- Repair and scrub ---

    async fn trigger_scrub(
        &self,
        _req: Request<pb::TriggerScrubRequest>,
    ) -> Result<Response<pb::TriggerScrubResponse>, Status> {
        Err(Self::unimpl(
            "TriggerScrub",
            "W4",
            "scrub_scheduler.trigger_now(); reports flow into ListRepairs",
        ))
    }

    async fn repair_chunk(
        &self,
        _req: Request<pb::AdminRepairChunkRequest>,
    ) -> Result<Response<pb::RepairChunkResponse>, Status> {
        Err(Self::unimpl(
            "RepairChunk",
            "W4",
            "direct GrpcFabricPeer::put_fragment for missing fragments",
        ))
    }

    async fn list_repairs(
        &self,
        _req: Request<pb::ListRepairsRequest>,
    ) -> Result<Response<pb::ListRepairsResponse>, Status> {
        Err(Self::unimpl(
            "ListRepairs",
            "W2",
            "scrub_scheduler.recent_reports(limit) helper to add",
        ))
    }
}

// ===========================================================================
// Scaffolding tests — pin every RPC at Unimplemented + workstream tag.
// W2-W7 each replace one assertion with a real-impl test as the matching
// RPC body lands.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    fn fixture() -> StorageAdminGrpc {
        StorageAdminGrpc::for_tests()
    }

    fn assert_unimplemented_with_workstream<T>(
        label: &str,
        workstream: &str,
        result: Result<tonic::Response<T>, tonic::Status>,
    ) {
        // T isn't bounded on Debug because two of the responses
        // (DeviceHealthStream, IOStatsStream) are trait objects
        // that don't implement it. Drop T early via `map(|_| ())`
        // so the panic message can be assembled without printing it.
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

    // -- Device management --

    #[tokio::test]
    async fn list_devices_unimplemented_until_w2() {
        let r = fixture()
            .list_devices(Request::new(pb::ListDevicesRequest::default()))
            .await;
        assert_unimplemented_with_workstream("ListDevices", "W2", r);
    }

    #[tokio::test]
    async fn get_device_unimplemented_until_w2() {
        let r = fixture()
            .get_device(Request::new(pb::GetDeviceRequest::default()))
            .await;
        assert_unimplemented_with_workstream("GetDevice", "W2", r);
    }

    #[tokio::test]
    async fn add_device_unimplemented_until_w5() {
        let r = fixture()
            .add_device(Request::new(pb::AddDeviceRequest::default()))
            .await;
        assert_unimplemented_with_workstream("AddDevice", "W5", r);
    }

    #[tokio::test]
    async fn remove_device_unimplemented_until_w5() {
        let r = fixture()
            .remove_device(Request::new(pb::RemoveDeviceRequest::default()))
            .await;
        assert_unimplemented_with_workstream("RemoveDevice", "W5", r);
    }

    #[tokio::test]
    async fn evacuate_device_unimplemented_until_w5() {
        let r = fixture()
            .evacuate_device(Request::new(pb::EvacuateDeviceRequest::default()))
            .await;
        assert_unimplemented_with_workstream("EvacuateDevice", "W5", r);
    }

    #[tokio::test]
    async fn cancel_evacuation_unimplemented_until_w4() {
        let r = fixture()
            .cancel_evacuation(Request::new(pb::CancelEvacuationRequest::default()))
            .await;
        assert_unimplemented_with_workstream("CancelEvacuation", "W4", r);
    }

    // -- Pool management --

    #[tokio::test]
    async fn list_pools_unimplemented_until_w2() {
        let r = fixture()
            .list_pools(Request::new(pb::ListPoolsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("ListPools", "W2", r);
    }

    #[tokio::test]
    async fn get_pool_unimplemented_until_w2() {
        let r = fixture()
            .get_pool(Request::new(pb::GetPoolRequest::default()))
            .await;
        assert_unimplemented_with_workstream("GetPool", "W2", r);
    }

    #[tokio::test]
    async fn create_pool_unimplemented_until_w5() {
        let r = fixture()
            .create_pool(Request::new(pb::CreatePoolRequest::default()))
            .await;
        assert_unimplemented_with_workstream("CreatePool", "W5", r);
    }

    #[tokio::test]
    async fn set_pool_durability_unimplemented_until_w5() {
        let r = fixture()
            .set_pool_durability(Request::new(pb::SetPoolDurabilityRequest::default()))
            .await;
        assert_unimplemented_with_workstream("SetPoolDurability", "W5", r);
    }

    #[tokio::test]
    async fn set_pool_thresholds_unimplemented_until_w5() {
        let r = fixture()
            .set_pool_thresholds(Request::new(pb::SetPoolThresholdsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("SetPoolThresholds", "W5", r);
    }

    #[tokio::test]
    async fn rebalance_pool_unimplemented_until_w5() {
        let r = fixture()
            .rebalance_pool(Request::new(pb::RebalancePoolRequest::default()))
            .await;
        assert_unimplemented_with_workstream("RebalancePool", "W5", r);
    }

    // -- Tuning --

    #[tokio::test]
    async fn get_tuning_params_unimplemented_until_w3() {
        let r = fixture()
            .get_tuning_params(Request::new(pb::GetTuningParamsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("GetTuningParams", "W3", r);
    }

    #[tokio::test]
    async fn set_tuning_params_unimplemented_until_w3() {
        let r = fixture()
            .set_tuning_params(Request::new(pb::SetTuningParamsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("SetTuningParams", "W3", r);
    }

    // -- Cluster observability --

    #[tokio::test]
    async fn cluster_status_unimplemented_until_w2() {
        let r = fixture()
            .cluster_status(Request::new(pb::ClusterStatusRequest::default()))
            .await;
        assert_unimplemented_with_workstream("ClusterStatus", "W2", r);
    }

    #[tokio::test]
    async fn pool_status_unimplemented_until_w2() {
        let r = fixture()
            .pool_status(Request::new(pb::PoolStatusRequest::default()))
            .await;
        assert_unimplemented_with_workstream("PoolStatus", "W2", r);
    }

    #[tokio::test]
    async fn device_health_unimplemented_until_w7() {
        let r = fixture()
            .device_health(Request::new(pb::DeviceHealthRequest::default()))
            .await;
        assert_unimplemented_with_workstream("DeviceHealth", "W7", r);
    }

    #[tokio::test]
    async fn io_stats_unimplemented_until_w7() {
        let r = fixture()
            .io_stats(Request::new(pb::IoStatsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("IOStats", "W7", r);
    }

    // -- Shard management --

    #[tokio::test]
    async fn list_shards_unimplemented_until_w2() {
        let r = fixture()
            .list_shards(Request::new(pb::ListShardsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("ListShards", "W2", r);
    }

    #[tokio::test]
    async fn get_shard_unimplemented_until_w2() {
        let r = fixture()
            .get_shard(Request::new(pb::GetShardRequest::default()))
            .await;
        assert_unimplemented_with_workstream("GetShard", "W2", r);
    }

    #[tokio::test]
    async fn split_shard_unimplemented_until_w5() {
        let r = fixture()
            .split_shard(Request::new(pb::SplitShardRequest::default()))
            .await;
        assert_unimplemented_with_workstream("SplitShard", "W5", r);
    }

    #[tokio::test]
    async fn merge_shards_unimplemented_until_w5() {
        let r = fixture()
            .merge_shards(Request::new(pb::MergeShardsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("MergeShards", "W5", r);
    }

    #[tokio::test]
    async fn set_shard_maintenance_unimplemented_until_w4() {
        let r = fixture()
            .set_shard_maintenance(Request::new(pb::SetShardMaintenanceRequest::default()))
            .await;
        assert_unimplemented_with_workstream("SetShardMaintenance", "W4", r);
    }

    // -- Repair and scrub --

    #[tokio::test]
    async fn trigger_scrub_unimplemented_until_w4() {
        let r = fixture()
            .trigger_scrub(Request::new(pb::TriggerScrubRequest::default()))
            .await;
        assert_unimplemented_with_workstream("TriggerScrub", "W4", r);
    }

    #[tokio::test]
    async fn repair_chunk_unimplemented_until_w4() {
        let r = fixture()
            .repair_chunk(Request::new(pb::AdminRepairChunkRequest::default()))
            .await;
        assert_unimplemented_with_workstream("RepairChunk", "W4", r);
    }

    #[tokio::test]
    async fn list_repairs_unimplemented_until_w2() {
        let r = fixture()
            .list_repairs(Request::new(pb::ListRepairsRequest::default()))
            .await;
        assert_unimplemented_with_workstream("ListRepairs", "W2", r);
    }

    // -- Cardinality cross-check --

    /// Mechanical guard that the proto file ships exactly the 26
    /// rpcs the W1 plan promises (25 from ADR-025 + `MergeShards`
    /// from ADR-034). If a future PR adds an rpc to the proto
    /// without adding an Unimplemented assertion above, this fails.
    /// If a PR removes an rpc, this also fails.
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
            "storage_admin.proto must declare exactly 26 rpcs \
             (25 from ADR-025 + MergeShards from ADR-034); found {rpc_count}",
        );
    }

    /// Cross-check: this test module must hold one
    /// `_unimplemented_until_w[0-9]` assertion per non-streaming
    /// RPC (24 unary) plus the two streaming ones — total 26 RPC
    /// assertions. Catches the case where a workstream lands but
    /// the test isn't replaced with the real-impl test.
    #[test]
    fn scaffolding_test_count_matches_rpc_count() {
        let this_file = include_str!("storage_admin.rs");
        let unimpl_test_count = this_file
            .matches("_unimplemented_until_w")
            .count()
            // Each test name appears twice in the source: once as
            // the function name in `async fn ...` and again on the
            // `#[tokio::test]` line that names it via path. Match
            // the function definition specifically.
            ;
        let fn_count = this_file
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("async fn ") && l.contains("_unimplemented_until_w"))
            .count();
        assert_eq!(
            fn_count, 26,
            "expected 26 _unimplemented_until_w* test fns (one per rpc); \
             found {fn_count}. Total `_unimplemented_until_w` substrings \
             (informational): {unimpl_test_count}",
        );
    }
}
