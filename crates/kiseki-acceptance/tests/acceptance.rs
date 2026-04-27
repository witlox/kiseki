//! BDD acceptance tests for Kiseki.
//!
//! Uses cucumber-rs to run Gherkin feature files from `specs/features/`.
//! Custom harness: `[[test]] harness = false` in Cargo.toml.
//!
//! Run with: `cargo test -p kiseki-acceptance`

#![allow(
    unused_variables,
    unused_imports,
    dead_code,
    unused_mut,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::uninlined_format_args
)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cucumber::World;
use kiseki_advisory::budget::{BudgetConfig, BudgetEnforcer};
use kiseki_advisory::workflow::WorkflowTable;
use kiseki_audit::store::AuditLog;
use kiseki_block::file::FileBackedDevice;
use kiseki_block::{DeviceBackend, Extent};
use kiseki_chunk::store::ChunkStore;
use kiseki_common::advisory::*;
use kiseki_common::ids::*;
use kiseki_common::tenancy::*;
use kiseki_common::time::*;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_control::federation::FederationRegistry;
use kiseki_control::flavor::Flavor;
use kiseki_control::iam::AccessRequest;
use kiseki_control::maintenance::MaintenanceState;
use kiseki_control::namespace::NamespaceStore;
use kiseki_control::retention::RetentionStore;
use kiseki_control::shard_topology::{NamespaceShardMapStore, ShardTopologyConfig};
use kiseki_control::storage_admin::StorageAdminService;
use kiseki_control::tenant::TenantStore;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::nfs::NfsGateway;
use kiseki_gateway::nfs_ops::NfsContext;
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};
use kiseki_keymanager::store::MemKeyStore;
use kiseki_log::shard::{ShardConfig, ShardState};
use kiseki_log::store::MemShardStore;
use kiseki_log::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};
use kiseki_view::view::ViewStore;

mod steps;

// ---------------------------------------------------------------------------
// World — shared state across all steps in a scenario
// ---------------------------------------------------------------------------

#[derive(World)]
#[world(init = Self::new)]
pub struct KisekiWorld {
    // === Real implementations (in-memory stores) ===
    pub log_store: Arc<dyn LogOps + Send + Sync>,
    /// Typed handle to the same in-memory store as `log_store`, exposed
    /// for step definitions that need access to MemShardStore-only API
    /// (split buffering, inline store wiring, etc.).
    pub mem_shard_store: Arc<MemShardStore>,
    pub key_store: MemKeyStore,
    pub audit_log: Arc<AuditLog>,
    pub chunk_store: ChunkStore,
    pub comp_store: CompositionStore,
    pub view_store: ViewStore,
    pub advisory_table: WorkflowTable,
    pub budget_enforcer: BudgetEnforcer,

    // === Integrated pipeline (R1) ===
    pub gateway: Arc<InMemoryGateway>,
    pub nfs_ctx: Arc<NfsContext<Arc<InMemoryGateway>>>,

    // === Test state ===
    pub last_error: Option<String>,
    pub last_read_data: Option<Vec<u8>>,
    pub last_epoch: Option<u64>,
    pub last_sequence: Option<SequenceNumber>,
    pub last_shard_id: Option<ShardId>,
    pub last_chunk_id: Option<ChunkId>,
    pub last_composition_id: Option<CompositionId>,
    pub last_view_id: Option<ViewId>,
    pub last_workflow_ref: Option<WorkflowRef>,

    // === Name → ID mappings ===
    pub shard_names: HashMap<String, ShardId>,
    pub tenant_ids: HashMap<String, OrgId>,
    pub namespace_ids: HashMap<String, NamespaceId>,
    pub view_ids: HashMap<String, ViewId>,
    pub workflow_names: HashMap<String, WorkflowRef>,

    // === Flags for behavioral assertions ===
    pub writes_rejected: bool,
    pub reads_working: bool,

    // === Control plane (ADR-027 migration) ===
    pub control_tenant_store: TenantStore,
    pub control_namespace_store: NamespaceStore,
    pub control_maintenance: MaintenanceState,
    pub control_last_org_id: Option<String>,
    pub control_last_project_id: Option<String>,
    pub control_last_workload_id: Option<String>,
    pub control_last_error: Option<String>,
    pub control_last_access_req: Option<AccessRequest>,
    pub control_audit_events: Vec<String>,
    pub control_plane_up: bool,
    pub control_org_capacity_used: u64,
    pub control_org_capacity_total: u64,
    pub control_workload_cap_used: u64,
    pub control_workload_cap_total: u64,
    pub control_last_write_error: Option<String>,
    pub control_last_quota_adjustment: bool,
    pub control_flavor_list: Vec<Flavor>,
    pub control_last_flavor_match: Option<Flavor>,
    pub control_last_flavor_error: Option<String>,
    pub control_retention_store: RetentionStore,
    pub control_federation_reg: FederationRegistry,
    pub control_advisory_state: kiseki_control::advisory_policy::OptOutState,
    pub control_active_workflows: u32,
    pub control_cluster_ceiling: kiseki_control::advisory_policy::HintBudget,
    pub control_org_policy: Option<kiseki_control::advisory_policy::ScopePolicy>,
    pub control_project_policy: Option<kiseki_control::advisory_policy::ScopePolicy>,
    pub control_workload_policy: Option<kiseki_control::advisory_policy::ScopePolicy>,
    pub control_last_policy_error: Option<String>,
    pub control_pool_authorized: std::collections::HashMap<String, String>,

    // === External KMS (ADR-028) ===
    pub kms_provider_type: Option<String>,
    pub kms_circuit_open: bool,
    pub kms_concurrent_count: u32,

    // === Storage admin (ADR-025) ===
    pub control_admin: StorageAdminService,

    // === Shard topology (ADR-033) ===
    pub topology_config: ShardTopologyConfig,
    pub shard_map_store: Arc<NamespaceShardMapStore>,
    pub topology_active_nodes: Vec<NodeId>,

    // === Small-file placement (ADR-030) ===
    pub sf_node_count: u64,
    pub sf_soft_limit_pct: u8,
    pub sf_hard_limit_pct: u8,
    pub sf_inline_floor: u16,
    pub sf_inline_ceiling: u32,
    pub sf_raft_inline_mbps: u32,
    pub sf_booted: bool,
    pub sf_rotational: bool,
    pub sf_media_type: String,
    pub sf_warning_emitted: bool,
    pub sf_current_shard: String,
    pub sf_min_budget_gb: u64,
    pub sf_estimated_files: u64,
    pub sf_inline_threshold: u64,
    pub sf_inline_file_count: u64,
    pub sf_capacity_pressure: bool,
    pub sf_threshold_increase_attempted: bool,
    pub sf_last_write_size: u64,
    pub sf_last_write_inline: bool,
    pub sf_last_read_inline: bool,
    pub sf_inline_rate_mbps: f64,
    pub sf_metadata_usage_pct: u64,
    pub sf_disk_full: bool,
    pub sf_gc_ran: bool,
    pub sf_small_file_ratio: f64,
    pub sf_homogeneous: bool,
    pub sf_writes_active: bool,
    pub sf_migration_active: bool,
    pub sf_migration_count: u64,
    pub sf_backoff_hours: u64,
    pub sf_hdd_voters: bool,
    pub sf_learner_active: bool,

    // === Block storage (ADR-029) ===
    pub block_device: Option<Box<dyn DeviceBackend>>,
    pub last_extent: Option<Extent>,
    pub block_device_path: Option<std::path::PathBuf>,
    pub block_extents: Vec<Extent>,
    pub block_temp_dir: Option<tempfile::TempDir>,
    pub block_scrub_report: Option<String>,

    // === Raft test cluster (ADR-037) ===
    pub raft_cluster: Option<kiseki_log::raft::test_cluster::RaftTestCluster>,

    // === Node lifecycle (ADR-035) ===
    pub drain_orch: Arc<kiseki_control::node_lifecycle::DrainOrchestrator>,
    /// Logical name (e.g. "n1", "n7") → NodeId, populated by drain step defs.
    pub node_names: HashMap<String, NodeId>,
    /// Most recent drain attempt outcome — Some when refused.
    pub last_drain_error: Option<String>,
    /// Per-shard Raft cluster spun up to demonstrate real voter
    /// replacement during drain orchestration.
    pub drain_raft: Option<kiseki_log::raft::test_cluster::RaftTestCluster>,

    // === TCP transport endpoints (ADR-022) ===
    /// Gateway name → bound TCP address (started on demand by step defs).
    pub tcp_endpoints: HashMap<String, std::net::SocketAddr>,
    /// Per-gateway shutdown signals so the NFS accept thread can exit
    /// cleanly when the World is dropped (no leaked threads under
    /// parallel cucumber runs).
    pub tcp_shutdowns: Vec<Arc<std::sync::atomic::AtomicBool>>,
    /// Per-gateway S3 task handles for graceful shutdown.
    pub s3_tasks: Vec<tokio::task::JoinHandle<()>>,

    // === Persistence harness (ADR-022) ===
    /// Per-scenario `PersistentShardStore` backed by a redb database in a
    /// tempdir. Persistence-feature steps drive writes through this store
    /// and call `restart_persistent_store()` between Given and Then to
    /// drop + reopen — proving the scenario survives a real reload, not
    /// the no-op that the in-memory `MemShardStore` provides.
    pub persistent_shard_store: Option<Arc<kiseki_log::persistent_store::PersistentShardStore>>,
    pub persistent_temp_dir: Option<tempfile::TempDir>,

    // === Tenant KMS providers (ADR-028) ===
    /// Per-provider-name `TenantKmsProvider` instances. Every named slot
    /// (`internal`, `vault`, `kmip`, `aws-kms`, `pkcs11`) is backed by a
    /// distinct `InternalProvider` with its own key — so the BDD exercises
    /// the trait through real production code rather than a local KEK
    /// roundtrip in the test body. Cloud-backend impls (vault/azure/gcp/aws)
    /// exist in `kiseki-keymanager` but require live endpoints; using the
    /// internal impl keeps the test deterministic without weakening the
    /// trait contract.
    pub kms_providers: HashMap<String, Arc<dyn kiseki_keymanager::TenantKmsProvider>>,

    // === Telemetry bus (ADR-021, I-WA5) ===
    pub telemetry_bus: Arc<kiseki_advisory::TelemetryBus>,
    /// Receivers cached per workload so subsequent Then steps can drain.
    pub backpressure_subs:
        HashMap<String, tokio::sync::mpsc::Receiver<kiseki_advisory::BackpressureEvent>>,
    pub qos_subs: HashMap<String, tokio::sync::mpsc::Receiver<kiseki_advisory::QosHeadroomBucket>>,

    // === Inline store (ADR-030, I-SF5) ===
    pub inline_store: Arc<kiseki_chunk::SmallObjectStore>,
    /// Owns the redb file backing `inline_store` for this scenario.
    pub inline_temp_dir: Option<tempfile::TempDir>,
    /// Hashed key of the most recent inline-payload delta — set by step
    /// definitions that need to verify offload.
    pub last_inline_key: Option<[u8; 32]>,
    /// Most recent appended delta (for inline-data assertions).
    pub last_delta: Option<kiseki_log::delta::Delta>,

    // === Backup / restore (ADR-016, Phase 14d) ===
    /// Active `BackupManager` for the scenario (built on demand by step
    /// definitions). Holds an Arc so `last_snapshot_id` lookups can
    /// reach back without re-binding.
    pub backup_manager: Option<std::sync::Arc<kiseki_backup::BackupManager>>,
    /// FS root the manager was bound to — kept alive so the tempdir
    /// persists for the scenario.
    pub backup_fs_dir: Option<tempfile::TempDir>,
    /// Direct trait handle (FS or S3) for assertions that bypass the
    /// manager — e.g. "the snapshot tarball is reachable through the
    /// S3 backend".
    pub backup_backend: Option<std::sync::Arc<dyn kiseki_backup::ObjectBackupBackend>>,
    /// Address of the in-process axum mock S3 (set when scenario picks S3).
    pub backup_s3_endpoint: Option<String>,
    /// Shards staged by Given steps before the When triggers a backup.
    pub backup_staged_shards: Vec<kiseki_backup::ShardSnapshot>,
    /// Snapshot returned by the most recent `create_snapshot` call.
    pub last_backup_snapshot: Option<kiseki_backup::BackupSnapshot>,
    /// Result of the most recent `restore_snapshot` call.
    pub last_restored_shards: Option<Vec<kiseki_backup::ShardSnapshot>>,
    /// All snapshots returned by the most recent `list_snapshots` call.
    pub last_snapshot_listing: Vec<kiseki_backup::BackupSnapshot>,
    /// Error captured from a backup operation that was expected to fail.
    pub last_backup_error: Option<String>,
    /// Background task hosting the in-process mock S3 server, dropped
    /// when the World is dropped to keep tests hermetic.
    pub backup_s3_task: Option<tokio::task::JoinHandle<()>>,

    // === Raft perf instrumentation (Phase 14f) ===
    /// Per-write latency for the most-recent batch of sequential writes.
    /// Populated by the perf When step, read by the Then assertion.
    pub raft_write_latencies: Vec<std::time::Duration>,
    /// Throughput observation: (operations, wall_clock).
    /// Set by the throughput When step, read by the Then assertion.
    pub raft_throughput: Option<(usize, std::time::Duration)>,
    /// Single-shard throughput baseline for the 10× comparison.
    pub raft_single_shard_throughput: Option<(usize, std::time::Duration)>,

    // === pNFS Phase 15a (ADR-038) ===
    /// fh4 MAC key under test — populated by the K_layout step.
    pub pnfs_mac_key: Option<kiseki_gateway::pnfs::PnfsFhMacKey>,
    /// fh4 currently held by the "client" — populated by issue/forge/expire steps.
    pub pnfs_fh: Option<kiseki_gateway::pnfs::PnfsFileHandle>,
    /// Most recent DS COMPOUND response — full XDR-decoded result list.
    /// Each entry is `(op_code, status, payload)` per RFC 5661 §15.2.
    pub pnfs_last_results: Vec<(u32, u32, Vec<u8>)>,
    /// Read counter on the tracking gateway (proves I-PN2 stateless +
    /// I-PN1 short-circuit on bad fh4).
    pub pnfs_gateway_reads: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Composition byte payload used by the read scenario — synthesised
    /// in `given_composition_with_size`.
    pub pnfs_composition_bytes: Option<Vec<u8>>,
    /// Outcome of the most recent NFS-security gate evaluation.
    pub pnfs_security_eval: Option<
        Result<
            kiseki_gateway::nfs_security::NfsSecurity,
            kiseki_gateway::nfs_security::NfsSecurityError,
        >,
    >,
    /// Audit log dedicated to pNFS scenarios (separate from the
    /// always-on `audit_log` so we can assert exact contents).
    pub pnfs_audit_log: std::sync::Arc<kiseki_audit::store::AuditLog>,
    /// Scenario-local DS context. Built lazily so steps can adjust the
    /// stripe size or clock.
    pub pnfs_ds_ctx: Option<
        std::sync::Arc<
            kiseki_gateway::pnfs_ds_server::DsContext<kiseki_gateway::mem_gateway::InMemoryGateway>,
        >,
    >,
    /// Bound DS listener address for TLS scenarios.
    pub pnfs_ds_addr: Option<std::net::SocketAddr>,
    /// Shutdown flag for the DS listener thread spawned in TLS scenarios.
    pub pnfs_ds_shutdown: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// MDS layout manager under test (Phase 15b).
    pub pnfs_mds_mgr: Option<std::sync::Arc<kiseki_gateway::pnfs::MdsLayoutManager>>,
    /// Most recent ServerLayout — captured for cache + LAYOUTGET scenarios.
    pub pnfs_last_layout: Option<kiseki_gateway::pnfs::ServerLayout>,
    /// Per-scenario monotonic clock so cache TTL tests are deterministic.
    pub pnfs_clock_ms: u64,

    // === Drain orchestration extras (multi-node-raft.feature drain
    //     scenarios — operational.rs `n1` patterns + this `node-1`
    //     naming use the same `world.drain_orch`).
    /// Shard name → owning node name. Drives the leadership-transfer
    /// assertions; the Raft test cluster doesn't expose per-shard
    /// "leader of which logical shard" so we maintain a small map
    /// here.
    pub shard_leaders: HashMap<String, String>,

    // === pNFS Phase 15d (TopologyEventBus) ===
    /// Topology event bus under test.
    pub topology_bus: Option<std::sync::Arc<kiseki_control::topology_events::TopologyEventBus>>,
    /// Active subscriber receiver — one per scenario.
    pub topology_sub: Option<kiseki_control::topology_events::TopologyEventSubscriber>,
}

impl Drop for KisekiWorld {
    fn drop(&mut self) {
        // Signal every spawned NFS accept thread to exit.
        for s in &self.tcp_shutdowns {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Abort outstanding S3 axum tasks (axum::serve loops forever
        // without a graceful-shutdown future).
        for h in self.s3_tasks.drain(..) {
            h.abort();
        }
        // Same for the backup-feature S3 mock.
        if let Some(h) = self.backup_s3_task.take() {
            h.abort();
        }
    }
}

impl std::fmt::Debug for KisekiWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KisekiWorld")
            .field("shards", &self.shard_names.len())
            .field("last_error", &self.last_error)
            .finish_non_exhaustive()
    }
}

impl KisekiWorld {
    fn new() -> Self {
        // Audit log first so key-store rotation / destruction events
        // can be sunk into it from the start.
        let audit_log = Arc::new(AuditLog::new());

        let key_store = MemKeyStore::new().unwrap_or_else(|_| MemKeyStore::default());
        // ADR-006 / I-K11: route key-lifecycle events into the shared
        // audit log so BDD assertions can query for emissions instead
        // of fabricating events.
        let _ = key_store.set_audit_log(
            Arc::clone(&audit_log) as Arc<dyn kiseki_audit::store::AuditOps + Send + Sync>
        );

        let mem_shard_store = Arc::new(MemShardStore::new());
        let log_store: Arc<dyn LogOps + Send + Sync> =
            Arc::clone(&mem_shard_store) as Arc<dyn LogOps + Send + Sync>;

        // Inline store (ADR-030): redb-backed small_object store wired into
        // the log store so deltas with has_inline_data=true are offloaded
        // on apply (I-SF5). Per-scenario tempdir keeps tests hermetic.
        let inline_temp_dir = tempfile::tempdir().expect("tempdir for inline small_object store");
        let inline_store = Arc::new(
            kiseki_chunk::SmallObjectStore::open(&inline_temp_dir.path().join("objects.redb"))
                .expect("open small_object store"),
        );
        let _ = mem_shard_store.set_inline_store(
            Arc::clone(&inline_store) as Arc<dyn kiseki_common::inline_store::InlineStore>
        );
        let comp_store = CompositionStore::new().with_log(Arc::clone(&log_store));

        // Integrated pipeline: InMemoryGateway chains encrypt → store → composition.
        let gw_chunks = kiseki_chunk::ChunkStore::new();
        let mut gw_comps = CompositionStore::new()
            .with_log(Arc::clone(&log_store) as Arc<dyn LogOps + Send + Sync>);
        let gw_master = kiseki_crypto::keys::SystemMasterKey::new([0x42; 32], KeyEpoch(1));

        // NFS context wrapping the gateway — for real NFS3/4 wire-format testing.
        let default_ns = NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default"));
        let default_tenant = OrgId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"org-test"));
        let default_shard = ShardId(uuid::Uuid::from_u128(1));
        mem_shard_store.create_shard(
            default_shard,
            default_tenant,
            kiseki_common::ids::NodeId(1),
            ShardConfig::default(),
        );
        // Register namespace on the composition store BEFORE wrapping in
        // InMemoryGateway — avoids async block_on inside cucumber's runtime.
        gw_comps.add_namespace(Namespace {
            id: default_ns,
            tenant_id: default_tenant,
            shard_id: default_shard,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });

        let shard_map_store = Arc::new(NamespaceShardMapStore::new());
        let telemetry_bus = Arc::new(kiseki_advisory::TelemetryBus::new());

        // ADR-028: provider registry with one InternalProvider per backend
        // name. Each gets its own randomly-generated key so a wrap from
        // "vault" cannot be unwrapped by "internal" (proves provider
        // isolation by construction).
        let mut kms_providers: HashMap<String, Arc<dyn kiseki_keymanager::TenantKmsProvider>> =
            HashMap::new();
        for name in ["internal", "vault", "kmip", "aws-kms", "pkcs11"] {
            let mut key = vec![0u8; 32];
            // Distinct deterministic key per provider name; not the byte-pattern
            // KEK from the deprecated `kek_for_provider` test helper.
            for (i, b) in name.bytes().enumerate() {
                key[i % 32] ^= b.wrapping_mul(7);
            }
            kms_providers.insert(
                name.to_string(),
                Arc::new(kiseki_keymanager::InternalProvider::new(key))
                    as Arc<dyn kiseki_keymanager::TenantKmsProvider>,
            );
        }
        let gateway = Arc::new(
            InMemoryGateway::new(gw_comps, Box::new(gw_chunks), gw_master)
                .with_shard_map(Arc::clone(&shard_map_store)),
        );
        // Wire the telemetry bus into the production gateway so its
        // write-path retriable errors emit real per-tenant backpressure.
        gateway.set_telemetry_bus(Arc::clone(&telemetry_bus));
        let nfs_gw = NfsGateway::new(Arc::clone(&gateway));
        let nfs_ctx = Arc::new(NfsContext::new(nfs_gw, default_tenant, default_ns));

        Self {
            log_store,
            mem_shard_store,
            key_store,
            audit_log,
            chunk_store: ChunkStore::new(),
            comp_store,
            view_store: ViewStore::new(),
            advisory_table: WorkflowTable::new(),
            budget_enforcer: BudgetEnforcer::new(BudgetConfig {
                hints_per_sec: 100,
                max_concurrent_workflows: 10,
                max_phases_per_workflow: 50,
            }),
            gateway,
            nfs_ctx,
            last_error: None,
            last_read_data: None,
            last_epoch: None,
            last_sequence: None,
            last_shard_id: None,
            last_chunk_id: None,
            last_composition_id: None,
            last_view_id: None,
            last_workflow_ref: None,
            shard_names: HashMap::new(),
            tenant_ids: HashMap::new(),
            namespace_ids: HashMap::new(),
            view_ids: HashMap::new(),
            workflow_names: HashMap::new(),
            writes_rejected: false,
            reads_working: false,
            control_tenant_store: TenantStore::new(),
            control_namespace_store: NamespaceStore::new(),
            control_maintenance: MaintenanceState::new(),
            control_last_org_id: None,
            control_last_project_id: None,
            control_last_workload_id: None,
            control_last_error: None,
            control_last_access_req: None,
            control_audit_events: Vec::new(),
            control_plane_up: true,
            control_org_capacity_used: 0,
            control_org_capacity_total: 0,
            control_workload_cap_used: 0,
            control_workload_cap_total: 0,
            control_last_write_error: None,
            control_last_quota_adjustment: false,
            control_flavor_list: Vec::new(),
            control_last_flavor_match: None,
            control_last_flavor_error: None,
            control_retention_store: RetentionStore::new(),
            control_federation_reg: FederationRegistry::new(),
            control_advisory_state: kiseki_control::advisory_policy::OptOutState::Enabled,
            control_active_workflows: 0,
            control_cluster_ceiling: kiseki_control::advisory_policy::HintBudget::default(),
            control_org_policy: None,
            control_project_policy: None,
            control_workload_policy: None,
            control_last_policy_error: None,
            control_pool_authorized: HashMap::new(),
            kms_provider_type: None,
            kms_circuit_open: false,
            kms_concurrent_count: 0,
            control_admin: StorageAdminService::new(),
            topology_config: ShardTopologyConfig::default(),
            shard_map_store,
            topology_active_nodes: Vec::new(),
            sf_node_count: 3,
            sf_soft_limit_pct: 50,
            sf_hard_limit_pct: 75,
            sf_inline_floor: 128,
            sf_inline_ceiling: 65536,
            sf_raft_inline_mbps: 10,
            sf_booted: false,
            sf_rotational: false,
            sf_media_type: String::new(),
            sf_warning_emitted: false,
            sf_current_shard: String::new(),
            sf_min_budget_gb: u64::MAX,
            sf_estimated_files: 0,
            sf_inline_threshold: 4096,
            sf_inline_file_count: 0,
            sf_capacity_pressure: false,
            sf_threshold_increase_attempted: false,
            sf_last_write_size: 0,
            sf_last_write_inline: false,
            sf_last_read_inline: false,
            sf_inline_rate_mbps: 0.0,
            sf_metadata_usage_pct: 0,
            sf_disk_full: false,
            sf_gc_ran: false,
            sf_small_file_ratio: 0.0,
            sf_homogeneous: false,
            sf_writes_active: false,
            sf_migration_active: false,
            sf_migration_count: 0,
            sf_backoff_hours: 0,
            sf_hdd_voters: false,
            sf_learner_active: false,
            block_device: None,
            last_extent: None,
            block_device_path: None,
            block_extents: Vec::new(),
            block_temp_dir: None,
            block_scrub_report: None,
            raft_cluster: None,
            inline_store,
            inline_temp_dir: Some(inline_temp_dir),
            last_inline_key: None,
            last_delta: None,
            backup_manager: None,
            backup_fs_dir: None,
            backup_backend: None,
            backup_s3_endpoint: None,
            backup_staged_shards: Vec::new(),
            last_backup_snapshot: None,
            last_restored_shards: None,
            last_snapshot_listing: Vec::new(),
            last_backup_error: None,
            backup_s3_task: None,
            raft_write_latencies: Vec::new(),
            raft_throughput: None,
            raft_single_shard_throughput: None,
            pnfs_mac_key: None,
            pnfs_fh: None,
            pnfs_last_results: Vec::new(),
            pnfs_gateway_reads: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pnfs_composition_bytes: None,
            pnfs_security_eval: None,
            pnfs_audit_log: std::sync::Arc::new(kiseki_audit::store::AuditLog::new()),
            pnfs_ds_ctx: None,
            pnfs_ds_addr: None,
            pnfs_ds_shutdown: None,
            pnfs_mds_mgr: None,
            pnfs_last_layout: None,
            pnfs_clock_ms: 1_000_000,
            shard_leaders: HashMap::new(),
            topology_bus: None,
            topology_sub: None,
            telemetry_bus,
            kms_providers,
            persistent_shard_store: None,
            persistent_temp_dir: None,
            backpressure_subs: HashMap::new(),
            qos_subs: HashMap::new(),
            tcp_endpoints: HashMap::new(),
            tcp_shutdowns: Vec::new(),
            s3_tasks: Vec::new(),
            drain_orch: Arc::new(kiseki_control::node_lifecycle::DrainOrchestrator::new()),
            node_names: HashMap::new(),
            last_drain_error: None,
            drain_raft: None,
        }
    }

    /// Get or create a shard by name.
    pub fn ensure_shard(&mut self, name: &str) -> ShardId {
        if let Some(&id) = self.shard_names.get(name) {
            return id;
        }
        let id = ShardId(uuid::Uuid::new_v4());
        let tenant = self.ensure_tenant("org-pharma");
        self.log_store
            .create_shard(id, tenant, NodeId(1), ShardConfig::default());
        self.shard_names.insert(name.to_owned(), id);
        id
    }

    /// Get or create a tenant by name.
    pub fn ensure_tenant(&mut self, name: &str) -> OrgId {
        if let Some(&id) = self.tenant_ids.get(name) {
            return id;
        }
        let id = OrgId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            name.as_bytes(),
        ));
        self.tenant_ids.insert(name.to_owned(), id);

        // Also populate control-plane tenant store (ADR-027 migration).
        self.ensure_control_tenant(name);

        id
    }

    /// Ensure a tenant exists in the control-plane store.
    pub fn ensure_control_tenant(&mut self, name: &str) {
        use kiseki_control::tenant::Organization;
        let org = Organization {
            id: name.to_owned(),
            name: name.to_owned(),
            compliance_tags: vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr],
            dedup_policy: DedupPolicy::CrossTenant,
            quota: Quota {
                capacity_bytes: 500_000_000_000_000,
                iops: 100_000,
                metadata_ops_per_sec: 10_000,
            },
            compression_enabled: false,
        };
        let _ = self.control_tenant_store.create_org(org);
    }

    /// Get or create a namespace by name.
    pub fn ensure_namespace(&mut self, name: &str, shard_name: &str) -> NamespaceId {
        if let Some(&id) = self.namespace_ids.get(name) {
            return id;
        }
        let ns_id = NamespaceId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            name.as_bytes(),
        ));
        let shard_id = self.ensure_shard(shard_name);
        let tenant_id = self.ensure_tenant("org-pharma");
        self.comp_store.add_namespace(Namespace {
            id: ns_id,
            tenant_id,
            shard_id,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        });
        self.namespace_ids.insert(name.to_owned(), ns_id);
        ns_id
    }

    /// Async sibling of `ensure_namespace` that ALSO registers the
    /// namespace with the gateway. Required for steps that drive
    /// `gateway_write` directly — the sync `ensure_namespace` only
    /// touches the standalone comp_store and can't reach the
    /// gateway's async `add_namespace` API.
    pub async fn ensure_namespace_in_gateway(
        &mut self,
        name: &str,
        shard_name: &str,
    ) -> NamespaceId {
        let ns_id = self.ensure_namespace(name, shard_name);
        let tenant_id = self.ensure_tenant("org-pharma");
        let shard_id = self.ensure_shard(shard_name);
        self.gateway
            .add_namespace(Namespace {
                id: ns_id,
                tenant_id,
                shard_id,
                read_only: false,
                versioning_enabled: false,
                compliance_tags: Vec::new(),
            })
            .await;
        ns_id
    }

    /// Create a multi-shard namespace via the real shard topology store,
    /// then register each shard in the log store with its key range and
    /// register the namespace in the gateway's composition store.
    ///
    /// This is the integrated path: topology → log store → composition → gateway.
    pub async fn ensure_topology_namespace(
        &mut self,
        name: &str,
        tenant_name: &str,
        requested_shards: Option<u32>,
    ) {
        use kiseki_control::shard_topology;

        let tenant_id = self.ensure_tenant(tenant_name);
        let ns_id = NamespaceId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            name.as_bytes(),
        ));

        // Create namespace in the shard map store under the UUID string
        // (matches gateway routing: NamespaceId.0.to_string()) and also
        // store an alias under the human name for step definition lookups.
        let ns_key = ns_id.0.to_string();
        let map = self
            .shard_map_store
            .create_namespace(
                &ns_key,
                tenant_id,
                &self.topology_config,
                &self.topology_active_nodes,
                requested_shards,
            )
            .expect("topology namespace creation should succeed");
        // Alias: store under human name too for step definition lookups.
        self.shard_map_store.alias(name, &ns_key);

        // Register each shard in the log store with its specific key range.
        for shard_range in &map.shards {
            self.log_store.create_shard(
                shard_range.shard_id,
                tenant_id,
                shard_range.leader_node,
                ShardConfig::default(),
            );
            self.log_store.update_shard_range(
                shard_range.shard_id,
                shard_range.range_start,
                shard_range.range_end,
            );
        }

        // Register namespace in the gateway's composition store,
        // pointing to the first shard as default.
        let default_shard = map.shards[0].shard_id;
        let ns = Namespace {
            id: ns_id,
            tenant_id,
            shard_id: default_shard,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        };
        self.gateway.add_namespace(ns.clone()).await;

        self.namespace_ids.insert(name.to_owned(), ns_id);
    }

    /// Advance the key manager to the specified epoch by rotating.
    pub async fn advance_to_epoch(&self, target: u64) {
        use kiseki_keymanager::epoch::KeyManagerOps;
        while self.key_store.current_epoch().await.unwrap().0 < target {
            self.key_store.rotate().await.unwrap();
        }
    }

    /// Get or create a view by name.
    pub fn ensure_view(&mut self, name: &str) -> ViewId {
        if let Some(&id) = self.view_ids.get(name) {
            return id;
        }
        use kiseki_view::descriptor::{ConsistencyModel, ProtocolSemantics, ViewDescriptor};
        use kiseki_view::view::ViewOps;
        let desc = ViewDescriptor {
            view_id: ViewId(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_DNS,
                name.as_bytes(),
            )),
            tenant_id: OrgId(uuid::Uuid::from_u128(100)),
            source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
            protocol: ProtocolSemantics::Posix,
            consistency: ConsistencyModel::ReadYourWrites,
            discardable: false,
            version: 1,
        };
        let id = self.view_store.create_view(desc).unwrap();
        self.view_ids.insert(name.to_owned(), id);
        self.last_view_id = Some(id);
        id
    }

    /// Ensure the default gateway namespace is registered with the NFS
    /// context's tenant, so `gateway.write(...)` succeeds without callers
    /// needing to set up namespaces individually.
    pub async fn ensure_gateway_ns(&self) {
        let tenant_id = self.nfs_ctx.tenant_id;
        let ns_id = self.nfs_ctx.namespace_id;
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        // Ensure the log store has the shard (composition store log bridge needs it).
        self.log_store.create_shard(
            shard_id,
            tenant_id,
            kiseki_common::ids::NodeId(1),
            ShardConfig::default(),
        );
        self.gateway
            .add_namespace(Namespace {
                id: ns_id,
                tenant_id,
                shard_id,
                read_only: false,
                versioning_enabled: false,
                compliance_tags: Vec::new(),
            })
            .await;
    }

    /// Make a test timestamp.
    pub fn timestamp(&self) -> DeltaTimestamp {
        DeltaTimestamp {
            hlc: HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: NodeId(1),
            },
            wall: WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            },
            quality: ClockQuality::Ntp,
        }
    }

    /// Run the stream processor to advance all tracked views from the log.
    pub async fn poll_views(&mut self) {
        use kiseki_view::stream_processor::TrackedStreamProcessor;
        let mut proc = TrackedStreamProcessor::new(self.log_store.as_ref(), &mut self.view_store);
        for &view_id in self.view_ids.values() {
            proc.track(view_id);
        }
        proc.poll(1000).await;
    }

    /// Make a standard append request.
    pub fn make_append_request(&self, shard_id: ShardId, key_byte: u8) -> AppendDeltaRequest {
        let tenant_id = *self
            .tenant_ids
            .get("org-pharma")
            .unwrap_or(&OrgId(uuid::Uuid::from_u128(100)));
        AppendDeltaRequest {
            shard_id,
            tenant_id,
            operation: kiseki_log::delta::OperationType::Create,
            timestamp: self.timestamp(),
            hashed_key: [key_byte; 32],
            chunk_refs: vec![],
            payload: vec![0xab; 64],
            has_inline_data: false,
        }
    }

    /// Resolve the tenant to use for gateway operations.
    ///
    /// Prefers "org-pharma" when registered (most scenarios), then falls back
    /// to any registered tenant, and finally to the NFS context's default.
    pub fn gateway_tenant(&self) -> OrgId {
        self.tenant_ids
            .get("org-pharma")
            .or_else(|| self.tenant_ids.values().next())
            .copied()
            .unwrap_or(self.nfs_ctx.tenant_id)
    }

    /// Open or reuse a `PersistentShardStore` for persistence-feature
    /// scenarios. Creates a tempdir-backed redb on first call; subsequent
    /// calls return the existing handle until `restart_persistent_store`
    /// drops it.
    pub async fn persistent_store(
        &mut self,
    ) -> Arc<kiseki_log::persistent_store::PersistentShardStore> {
        if self.persistent_shard_store.is_none() {
            let dir = tempfile::tempdir().expect("persistent store tempdir");
            let path = dir.path().join("raft-log.redb");
            let store = kiseki_log::persistent_store::PersistentShardStore::open(&path)
                .await
                .expect("open persistent store");
            self.persistent_shard_store = Some(Arc::new(store));
            self.persistent_temp_dir = Some(dir);
        }
        Arc::clone(
            self.persistent_shard_store
                .as_ref()
                .expect("just initialised"),
        )
    }

    /// Simulate a server restart: drop the in-memory state of the
    /// `PersistentShardStore`, then reopen it against the same redb path.
    /// Anything that wasn't durably committed to redb is lost; anything
    /// that was survives. The tempdir is preserved so the redb file persists.
    ///
    /// No-op when no persistent store has been opened — scenarios that
    /// don't touch the persistent store (chunk/view/key/inline restart
    /// scenarios, still wired against in-memory stores) treat the
    /// "server restart" step as effectively a flush, since their
    /// in-memory state IS what survives in their world model.
    pub async fn restart_persistent_store(&mut self) {
        let Some(dir) = self.persistent_temp_dir.as_ref() else {
            return;
        };
        let path = dir.path().join("raft-log.redb");
        // Drop the existing handle so we can reopen against the same path.
        self.persistent_shard_store = None;
        let store = kiseki_log::persistent_store::PersistentShardStore::open(&path)
            .await
            .expect("reopen persistent store");
        self.persistent_shard_store = Some(Arc::new(store));
    }

    /// Write data through the integrated pipeline (gateway → encrypt → store).
    pub async fn gateway_write(&self, ns_name: &str, data: &[u8]) -> Result<WriteResponse, String> {
        self.gateway_write_as(ns_name, data, self.gateway_tenant())
            .await
    }

    /// Write data through the pipeline with an explicit tenant.
    pub async fn gateway_write_as(
        &self,
        ns_name: &str,
        data: &[u8],
        tenant_id: OrgId,
    ) -> Result<WriteResponse, String> {
        let ns_id = *self
            .namespace_ids
            .get(ns_name)
            .unwrap_or(&NamespaceId(uuid::Uuid::from_u128(1)));

        // If the namespace was already registered (e.g., via ensure_topology_namespace),
        // use its existing shards. Otherwise create a default full-range shard.
        if !self.namespace_ids.contains_key(ns_name) {
            let shard_id = ShardId(uuid::Uuid::from_u128(1));
            self.log_store.create_shard(
                shard_id,
                tenant_id,
                kiseki_common::ids::NodeId(1),
                ShardConfig::default(),
            );
            self.gateway
                .add_namespace(Namespace {
                    id: ns_id,
                    tenant_id,
                    shard_id,
                    read_only: false,
                    versioning_enabled: false,
                    compliance_tags: Vec::new(),
                })
                .await;
        }

        self.gateway
            .write(WriteRequest {
                namespace_id: ns_id,
                tenant_id,
                data: data.to_vec(),
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Read data through the integrated pipeline (store → decrypt → gateway).
    pub async fn gateway_read(
        &self,
        composition_id: CompositionId,
        tenant_id: OrgId,
        ns_name: &str,
    ) -> Result<ReadResponse, String> {
        let ns_id = *self
            .namespace_ids
            .get(ns_name)
            .unwrap_or(&NamespaceId(uuid::Uuid::from_u128(1)));
        self.gateway
            .read(ReadRequest {
                tenant_id,
                namespace_id: ns_id,
                composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Cucumber runner
// ---------------------------------------------------------------------------

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    // Optional env-var filter for ad-hoc per-subset timing measurements.
    // KISEKI_SCENARIO_FILTER=substring restricts to scenarios whose name
    // contains the substring. KISEKI_FEATURE_FILTER=substring restricts
    // to feature files whose path contains the substring. Both default
    // to "no extra restriction".
    let scenario_filter = std::env::var("KISEKI_SCENARIO_FILTER").ok();
    let feature_filter = std::env::var("KISEKI_FEATURE_FILTER").ok();

    // `@slow` scenarios were genuinely 1-2 s each on macOS (osxfs/virtiofs
    // fsync overhead + tokio timer coalescing pushing election windows
    // past the 150-300 ms threshold). On Linux with `epoll` + native fs
    // they're hundreds of ms tops — see specs/ops/runtimes.md per-feature
    // table. So skip `@slow` only when the host is macOS AND the
    // `slow-tests` feature is off; on Linux always include them, and on
    // macOS include them when the feature is explicitly requested.
    let skip_slow = cfg!(target_os = "macos") && !cfg!(feature = "slow-tests");

    let runner = KisekiWorld::cucumber().filter_run("features/", move |feat, _, sc| {
        if skip_slow && sc.tags.iter().any(|t| t == "slow") {
            return false;
        }
        if let Some(ref fname) = feature_filter {
            if !feat
                .path
                .as_deref()
                .is_some_and(|p| p.to_string_lossy().contains(fname))
            {
                return false;
            }
        }
        if let Some(ref name) = scenario_filter {
            if !sc.name.contains(name) {
                return false;
            }
        }
        true
    });

    rt.block_on(runner);
}
