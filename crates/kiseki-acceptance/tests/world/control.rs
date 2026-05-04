#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Control-plane state (ADR-027).

use kiseki_control::federation::FederationRegistry;
use kiseki_control::flavor::Flavor;
use kiseki_control::iam::AccessRequest;
use kiseki_control::maintenance::MaintenanceState;
use kiseki_control::namespace::NamespaceStore;
use kiseki_control::retention::RetentionStore;
use kiseki_control::storage_admin::StorageAdminService;
use kiseki_control::tenant::TenantStore;
use std::collections::HashMap;

pub struct ControlState {
    pub tenant_store: TenantStore,
    pub namespace_store: NamespaceStore,
    pub maintenance: MaintenanceState,
    pub last_org_id: Option<String>,
    pub last_project_id: Option<String>,
    pub last_workload_id: Option<String>,
    pub last_error: Option<String>,
    pub last_access_req: Option<AccessRequest>,
    pub audit_events: Vec<String>,
    pub plane_up: bool,
    pub org_capacity_used: u64,
    pub org_capacity_total: u64,
    pub workload_cap_used: u64,
    pub workload_cap_total: u64,
    pub last_write_error: Option<String>,
    pub last_quota_adjustment: bool,
    pub flavor_list: Vec<Flavor>,
    pub last_flavor_match: Option<Flavor>,
    pub last_flavor_error: Option<String>,
    pub retention_store: RetentionStore,
    pub federation_reg: FederationRegistry,
    pub advisory_state: kiseki_control::advisory_policy::OptOutState,
    pub active_workflows: u32,
    pub cluster_ceiling: kiseki_control::advisory_policy::HintBudget,
    pub org_policy: Option<kiseki_control::advisory_policy::ScopePolicy>,
    pub project_policy: Option<kiseki_control::advisory_policy::ScopePolicy>,
    pub workload_policy: Option<kiseki_control::advisory_policy::ScopePolicy>,
    pub last_policy_error: Option<String>,
    pub pool_authorized: HashMap<String, String>,
    pub admin: StorageAdminService,
}

impl ControlState {
    pub fn new() -> Self {
        Self {
            tenant_store: TenantStore::new(),
            namespace_store: NamespaceStore::new(),
            maintenance: MaintenanceState::new(),
            last_org_id: None,
            last_project_id: None,
            last_workload_id: None,
            last_error: None,
            last_access_req: None,
            audit_events: Vec::new(),
            plane_up: true,
            org_capacity_used: 0,
            org_capacity_total: 0,
            workload_cap_used: 0,
            workload_cap_total: 0,
            last_write_error: None,
            last_quota_adjustment: false,
            flavor_list: Vec::new(),
            last_flavor_match: None,
            last_flavor_error: None,
            retention_store: RetentionStore::new(),
            federation_reg: FederationRegistry::new(),
            advisory_state: kiseki_control::advisory_policy::OptOutState::Enabled,
            active_workflows: 0,
            cluster_ceiling: kiseki_control::advisory_policy::HintBudget::default(),
            org_policy: None,
            project_policy: None,
            workload_policy: None,
            last_policy_error: None,
            pool_authorized: HashMap::new(),
            admin: StorageAdminService::new(),
        }
    }
}
