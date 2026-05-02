//! Legacy in-memory domain objects.
//!
//! These exist for @unit step definitions that test domain logic
//! directly (crypto, EC, composition store, etc.). As steps migrate
//! to the server harness, fields move out and this struct shrinks.
//!
//! @integration steps MUST NOT use any field in this struct.

use std::collections::HashMap;
use std::sync::Arc;

use kiseki_advisory::budget::{BudgetConfig, BudgetEnforcer};
use kiseki_advisory::workflow::WorkflowTable;
use kiseki_audit::store::AuditLog;
use kiseki_chunk::store::ChunkStore;
use kiseki_common::advisory::*;
use kiseki_common::ids::*;
use kiseki_common::time::*;
use kiseki_common::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_control::shard_topology::{NamespaceShardMapStore, ShardTopologyConfig};
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::nfs::NfsGateway;
use kiseki_gateway::nfs_ops::NfsContext;
use kiseki_keymanager::store::MemKeyStore;
use kiseki_log::shard::ShardConfig;
use kiseki_log::store::MemShardStore;
use kiseki_log::traits::LogOps;
use kiseki_view::view::ViewStore;

pub struct LegacyState {
    // Domain stores (in-memory)
    pub log_store: Arc<dyn LogOps + Send + Sync>,
    pub mem_shard_store: Arc<MemShardStore>,
    pub key_store: MemKeyStore,
    pub audit_log: Arc<AuditLog>,
    pub chunk_store: ChunkStore,
    pub comp_store: CompositionStore,
    pub view_store: ViewStore,
    pub advisory_table: WorkflowTable,
    pub budget_enforcer: BudgetEnforcer,

    // In-memory gateway pipeline
    pub gateway: Arc<InMemoryGateway>,
    pub nfs_ctx: Arc<NfsContext<Arc<InMemoryGateway>>>,

    // Shard topology (ADR-033)
    pub topology_config: ShardTopologyConfig,
    pub shard_map_store: Arc<NamespaceShardMapStore>,
    pub topology_active_nodes: Vec<NodeId>,

    // Telemetry bus (ADR-021)
    pub telemetry_bus: Arc<kiseki_advisory::TelemetryBus>,
    pub backpressure_subs:
        HashMap<String, tokio::sync::mpsc::Receiver<kiseki_advisory::BackpressureEvent>>,
    pub qos_subs: HashMap<String, tokio::sync::mpsc::Receiver<kiseki_advisory::QosHeadroomBucket>>,

    // Inline store (ADR-030)
    pub inline_store: Arc<kiseki_chunk::SmallObjectStore>,
    pub inline_temp_dir: Option<tempfile::TempDir>,
    pub last_inline_key: Option<[u8; 32]>,
    pub last_delta: Option<kiseki_log::delta::Delta>,

    // Persistence harness
    pub persistent_shard_store: Option<Arc<kiseki_log::persistent_store::PersistentShardStore>>,
    pub persistent_temp_dir: Option<tempfile::TempDir>,

    // TCP transport endpoints
    pub tcp_endpoints: HashMap<String, std::net::SocketAddr>,
    pub tcp_shutdowns: Vec<Arc<std::sync::atomic::AtomicBool>>,
    pub s3_tasks: Vec<tokio::task::JoinHandle<()>>,

    // Topology events (ADR-038 Phase 15d)
    pub topology_bus: Option<Arc<kiseki_control::topology_events::TopologyEventBus>>,
    pub topology_sub: Option<kiseki_control::topology_events::TopologyEventSubscriber>,
}

impl Drop for LegacyState {
    fn drop(&mut self) {
        for s in &self.tcp_shutdowns {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for h in self.s3_tasks.drain(..) {
            h.abort();
        }
    }
}

impl LegacyState {
    pub fn new() -> Self {
        let audit_log = Arc::new(AuditLog::new());
        let key_store = MemKeyStore::new().unwrap_or_else(|_| MemKeyStore::default());
        let _ = key_store.set_audit_log(
            Arc::clone(&audit_log) as Arc<dyn kiseki_audit::store::AuditOps + Send + Sync>
        );

        let mem_shard_store = Arc::new(MemShardStore::new());
        let log_store: Arc<dyn LogOps + Send + Sync> =
            Arc::clone(&mem_shard_store) as Arc<dyn LogOps + Send + Sync>;

        let inline_temp_dir = tempfile::tempdir().expect("tempdir for inline small_object store");
        let inline_store = Arc::new(
            kiseki_chunk::SmallObjectStore::open(&inline_temp_dir.path().join("objects.redb"))
                .expect("open small_object store"),
        );
        let _ = mem_shard_store.set_inline_store(
            Arc::clone(&inline_store) as Arc<dyn kiseki_common::inline_store::InlineStore>
        );
        let comp_store = CompositionStore::new().with_log(Arc::clone(&log_store));

        let gw_chunks = kiseki_chunk::ChunkStore::new();
        let mut gw_comps = CompositionStore::new()
            .with_log(Arc::clone(&log_store) as Arc<dyn LogOps + Send + Sync>);
        let gw_master = kiseki_crypto::keys::SystemMasterKey::new([0x42; 32], KeyEpoch(1));

        let default_ns = NamespaceId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"default"));
        let default_tenant = OrgId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"org-test"));
        let default_shard = ShardId(uuid::Uuid::from_u128(1));
        mem_shard_store.create_shard(
            default_shard,
            default_tenant,
            kiseki_common::ids::NodeId(1),
            ShardConfig::default(),
        );
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

        let gateway = Arc::new(
            InMemoryGateway::new(gw_comps, kiseki_chunk::arc_async(gw_chunks), gw_master)
                .with_shard_map(Arc::clone(&shard_map_store)),
        );
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
            topology_config: ShardTopologyConfig::default(),
            shard_map_store,
            topology_active_nodes: Vec::new(),
            telemetry_bus,
            backpressure_subs: HashMap::new(),
            qos_subs: HashMap::new(),
            inline_store,
            inline_temp_dir: Some(inline_temp_dir),
            last_inline_key: None,
            last_delta: None,
            persistent_shard_store: None,
            persistent_temp_dir: None,
            tcp_endpoints: HashMap::new(),
            tcp_shutdowns: Vec::new(),
            s3_tasks: Vec::new(),
            topology_bus: None,
            topology_sub: None,
        }
    }
}
