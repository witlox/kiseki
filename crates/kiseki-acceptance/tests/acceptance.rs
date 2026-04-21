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
use kiseki_control::tenant::TenantStore;
use kiseki_gateway::mem_gateway::InMemoryGateway;
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
    pub log_store: Arc<MemShardStore>,
    pub key_store: MemKeyStore,
    pub audit_log: AuditLog,
    pub chunk_store: ChunkStore,
    pub comp_store: CompositionStore,
    pub view_store: ViewStore,
    pub advisory_table: WorkflowTable,
    pub budget_enforcer: BudgetEnforcer,

    // === Integrated pipeline (R1) ===
    pub gateway: Arc<InMemoryGateway>,

    // === Test state ===
    pub last_error: Option<String>,
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
        let key_store = MemKeyStore::new().unwrap_or_else(|_| MemKeyStore::default());
        let log_store = Arc::new(MemShardStore::new());
        let comp_store = CompositionStore::new()
            .with_log(Arc::clone(&log_store) as Arc<dyn LogOps + Send + Sync>);

        // Integrated pipeline: InMemoryGateway chains encrypt → store → composition.
        let gw_chunks = kiseki_chunk::ChunkStore::new();
        let gw_comps = CompositionStore::new()
            .with_log(Arc::clone(&log_store) as Arc<dyn LogOps + Send + Sync>);
        let gw_master = kiseki_crypto::keys::SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gateway = Arc::new(InMemoryGateway::new(gw_comps, gw_chunks, gw_master));

        Self {
            log_store,
            key_store,
            audit_log: AuditLog::new(),
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
            last_error: None,
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
        });
        self.namespace_ids.insert(name.to_owned(), ns_id);
        ns_id
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
    pub fn poll_views(&mut self) {
        use kiseki_view::stream_processor::TrackedStreamProcessor;
        let mut proc = TrackedStreamProcessor::new(self.log_store.as_ref(), &mut self.view_store);
        for &view_id in self.view_ids.values() {
            proc.track(view_id);
        }
        proc.poll(1000);
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

    /// Write data through the integrated pipeline (gateway → encrypt → store).
    pub fn gateway_write(&self, ns_name: &str, data: &[u8]) -> Result<WriteResponse, String> {
        let ns_id = *self
            .namespace_ids
            .get(ns_name)
            .unwrap_or(&NamespaceId(uuid::Uuid::from_u128(1)));
        let tenant_id = *self
            .tenant_ids
            .values()
            .next()
            .unwrap_or(&OrgId(uuid::Uuid::from_u128(1)));

        // Ensure namespace exists in the gateway's composition store.
        let shard_id = *self
            .shard_names
            .values()
            .next()
            .unwrap_or(&ShardId(uuid::Uuid::from_u128(1)));
        self.gateway.add_namespace(Namespace {
            id: ns_id,
            tenant_id,
            shard_id,
            read_only: false,
        });

        self.gateway
            .write(WriteRequest {
                namespace_id: ns_id,
                tenant_id,
                data: data.to_vec(),
            })
            .map_err(|e| e.to_string())
    }

    /// Read data through the integrated pipeline (store → decrypt → gateway).
    pub fn gateway_read(
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
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Cucumber runner
// ---------------------------------------------------------------------------

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(KisekiWorld::cucumber().run("features/"));
}
