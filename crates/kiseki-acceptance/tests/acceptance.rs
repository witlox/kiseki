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
    clippy::too_many_lines
)]

use std::collections::HashMap;

use cucumber::World;
use kiseki_advisory::budget::{BudgetConfig, BudgetEnforcer};
use kiseki_advisory::workflow::WorkflowTable;
use kiseki_audit::store::AuditLog;
use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::*;
use kiseki_common::tenancy::*;
use kiseki_common::time::*;
use kiseki_composition::composition::CompositionStore;
use kiseki_keymanager::store::MemKeyStore;
use kiseki_log::shard::{ShardConfig, ShardState};
use kiseki_log::store::MemShardStore;
use kiseki_log::traits::{AppendDeltaRequest, LogOps};
use kiseki_view::view::ViewStore;

mod steps;

// ---------------------------------------------------------------------------
// World — shared state across all steps in a scenario
// ---------------------------------------------------------------------------

#[derive(World)]
#[world(init = Self::new)]
pub struct KisekiWorld {
    // === Real implementations (in-memory stores) ===
    pub log_store: MemShardStore,
    pub key_store: MemKeyStore,
    pub audit_log: AuditLog,
    pub chunk_store: ChunkStore,
    pub comp_store: CompositionStore,
    pub view_store: ViewStore,
    pub advisory_table: WorkflowTable,
    pub budget_enforcer: BudgetEnforcer,

    // === Test state (results from WHEN steps, checked in THEN) ===
    pub last_error: Option<String>,
    pub last_sequence: Option<SequenceNumber>,
    pub last_shard_id: Option<ShardId>,
    pub shard_names: HashMap<String, ShardId>,
    pub tenant_ids: HashMap<String, OrgId>,
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
        Self {
            log_store: MemShardStore::new(),
            key_store,
            audit_log: AuditLog::new(),
            chunk_store: ChunkStore::new(),
            comp_store: CompositionStore::new(),
            view_store: ViewStore::new(),
            advisory_table: WorkflowTable::new(),
            budget_enforcer: BudgetEnforcer::new(BudgetConfig {
                hints_per_sec: 100,
                max_concurrent_workflows: 10,
                max_phases_per_workflow: 50,
            }),
            last_error: None,
            last_sequence: None,
            last_shard_id: None,
            shard_names: HashMap::new(),
            tenant_ids: HashMap::new(),
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
        id
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
}

// ---------------------------------------------------------------------------
// Cucumber runner
// ---------------------------------------------------------------------------

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(KisekiWorld::cucumber().run("features/"));
}
