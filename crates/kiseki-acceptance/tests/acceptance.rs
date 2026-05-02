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
mod world;

// ---------------------------------------------------------------------------
// World — shared state across all steps in a scenario
// ---------------------------------------------------------------------------

#[derive(World)]
#[world(init = Self::new)]
pub struct KisekiWorld {
    // --- Domain sub-structs (grouped by concern) ---
    /// In-memory domain objects for @unit steps. Will shrink as steps
    /// migrate to the server harness. @integration steps should NOT
    /// access this — if a step file imports from `legacy`, it's @unit.
    pub legacy: world::legacy::LegacyState,
    /// Control-plane state (ADR-027).
    pub control: world::control::ControlState,
    /// Small-file placement state (ADR-030).
    pub sf: world::small_file::SmallFileState,
    /// Block storage device state (ADR-029).
    pub block: world::block::BlockState,
    /// Raft cluster + drain + perf (ADR-037, ADR-035).
    pub raft: world::raft::RaftState,
    /// External KMS state (ADR-028).
    pub kms: world::kms::KmsState,
    /// pNFS Flexible Files state (ADR-038).
    pub pnfs: world::pnfs::PnfsState,
    /// Backup/restore state (ADR-016).
    pub backup: world::backup::BackupState,
    /// Per-scenario state for `@multi-node` cluster steps. The cluster
    /// itself is a process-level singleton in `steps::cluster_harness`.
    pub cluster: world::cluster::ClusterState,

    // --- Shared test state (used across step files) ---
    pub last_error: Option<String>,
    pub last_read_data: Option<Vec<u8>>,
    pub last_epoch: Option<u64>,
    pub last_sequence: Option<SequenceNumber>,
    pub last_shard_id: Option<ShardId>,
    pub last_chunk_id: Option<ChunkId>,
    pub last_composition_id: Option<CompositionId>,
    pub last_view_id: Option<ViewId>,
    pub last_workflow_ref: Option<WorkflowRef>,
    pub last_extent: Option<Extent>,
    pub writes_rejected: bool,
    pub reads_working: bool,

    // --- Name → ID mappings (Gherkin readability) ---
    pub shard_names: HashMap<String, ShardId>,
    pub tenant_ids: HashMap<String, OrgId>,
    pub namespace_ids: HashMap<String, NamespaceId>,
    pub view_ids: HashMap<String, ViewId>,
    pub workflow_names: HashMap<String, WorkflowRef>,

    // --- Server harness (@integration steps only) ---
    /// Running kiseki-server + network clients. Started lazily.
    pub server: Option<steps::harness::ServerHarness>,
}

impl Drop for KisekiWorld {
    fn drop(&mut self) {
        // Sub-struct Drop impls handle their own cleanup
        // (ServerHarness kills the process, LegacyState signals
        // NFS threads, BackupState aborts S3 task).
        drop(self.server.take());
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
        Self {
            legacy: world::legacy::LegacyState::new(),
            control: world::control::ControlState::new(),
            sf: world::small_file::SmallFileState::new(),
            block: world::block::BlockState::new(),
            raft: world::raft::RaftState::new(),
            kms: world::kms::KmsState::new(),
            pnfs: world::pnfs::PnfsState::new(),
            backup: world::backup::BackupState::new(),
            cluster: world::cluster::ClusterState::default(),
            last_error: None,
            last_read_data: None,
            last_epoch: None,
            last_sequence: None,
            last_shard_id: None,
            last_chunk_id: None,
            last_composition_id: None,
            last_view_id: None,
            last_workflow_ref: None,
            last_extent: None,
            writes_rejected: false,
            reads_working: false,
            shard_names: HashMap::new(),
            tenant_ids: HashMap::new(),
            namespace_ids: HashMap::new(),
            view_ids: HashMap::new(),
            workflow_names: HashMap::new(),
            server: None,
        }
    }

    /// Get or create a shard by name.
    pub fn ensure_shard(&mut self, name: &str) -> ShardId {
        if let Some(&id) = self.shard_names.get(name) {
            return id;
        }
        let id = ShardId(uuid::Uuid::new_v4());
        let tenant = self.ensure_tenant("org-pharma");
        self.legacy
            .log_store
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
        let _ = self.control.tenant_store.create_org(org);
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
        self.legacy.comp_store.add_namespace(Namespace {
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
        self.legacy
            .gateway
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
            .legacy
            .shard_map_store
            .create_namespace(
                &ns_key,
                tenant_id,
                &self.legacy.topology_config,
                &self.legacy.topology_active_nodes,
                requested_shards,
            )
            .expect("topology namespace creation should succeed");
        // Alias: store under human name too for step definition lookups.
        self.legacy.shard_map_store.alias(name, &ns_key);

        // Register each shard in the log store with its specific key range.
        for shard_range in &map.shards {
            self.legacy.log_store.create_shard(
                shard_range.shard_id,
                tenant_id,
                shard_range.leader_node,
                ShardConfig::default(),
            );
            self.legacy.log_store.update_shard_range(
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
        self.legacy.gateway.add_namespace(ns.clone()).await;

        self.namespace_ids.insert(name.to_owned(), ns_id);
    }

    /// Advance the key manager to the specified epoch by rotating.
    pub async fn advance_to_epoch(&self, target: u64) {
        use kiseki_keymanager::epoch::KeyManagerOps;
        while self.legacy.key_store.current_epoch().await.unwrap().0 < target {
            self.legacy.key_store.rotate().await.unwrap();
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
        let id = self.legacy.view_store.create_view(desc).unwrap();
        self.view_ids.insert(name.to_owned(), id);
        self.last_view_id = Some(id);
        id
    }

    /// Ensure the default gateway namespace is registered with the NFS
    /// context's tenant, so `gateway.write(...)` succeeds without callers
    /// needing to set up namespaces individually.
    pub async fn ensure_gateway_ns(&self) {
        let tenant_id = self.legacy.nfs_ctx.tenant_id;
        let ns_id = self.legacy.nfs_ctx.namespace_id;
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        // Ensure the log store has the shard (composition store log bridge needs it).
        self.legacy.log_store.create_shard(
            shard_id,
            tenant_id,
            kiseki_common::ids::NodeId(1),
            ShardConfig::default(),
        );
        self.legacy
            .gateway
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
        let mut proc = TrackedStreamProcessor::new(
            self.legacy.log_store.as_ref(),
            &mut self.legacy.view_store,
        );
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
            .unwrap_or(self.legacy.nfs_ctx.tenant_id)
    }

    /// Open or reuse a `PersistentShardStore` for persistence-feature
    /// scenarios. Creates a tempdir-backed redb on first call; subsequent
    /// calls return the existing handle until `restart_persistent_store`
    /// drops it.
    pub async fn persistent_store(
        &mut self,
    ) -> Arc<kiseki_log::persistent_store::PersistentShardStore> {
        if self.legacy.persistent_shard_store.is_none() {
            let dir = tempfile::tempdir().expect("persistent store tempdir");
            // Path matches production layout
            // (`runtime.rs`: `dir.join("raft").join("log.redb")`) so
            // persistence.feature's Background step can assert against
            // the same on-disk shape that `kiseki-server` produces.
            let raft_dir = dir.path().join("raft");
            std::fs::create_dir_all(&raft_dir).expect("mkdir <data_dir>/raft");
            let path = raft_dir.join("log.redb");
            let store = kiseki_log::persistent_store::PersistentShardStore::open(&path)
                .await
                .expect("open persistent store");
            self.legacy.persistent_shard_store = Some(Arc::new(store));
            self.legacy.persistent_temp_dir = Some(dir);
        }
        Arc::clone(
            self.legacy
                .persistent_shard_store
                .as_ref()
                .expect("just initialised"),
        )
    }

    /// Path the harness uses for the persistent shard log (mirrors
    /// the production runtime: `<DATA_DIR>/raft/log.redb`). Returns
    /// `None` until `persistent_store()` has been called at least
    /// once. Used by the persistence.feature Background step to
    /// assert the on-disk layout the spec documents.
    #[must_use]
    pub fn persistent_store_path(&self) -> Option<std::path::PathBuf> {
        self.legacy
            .persistent_temp_dir
            .as_ref()
            .map(|d| d.path().join("raft").join("log.redb"))
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
        let Some(dir) = self.legacy.persistent_temp_dir.as_ref() else {
            return;
        };
        let path = dir.path().join("raft").join("log.redb");
        // Drop the existing handle so we can reopen against the same path.
        self.legacy.persistent_shard_store = None;
        let store = kiseki_log::persistent_store::PersistentShardStore::open(&path)
            .await
            .expect("reopen persistent store");
        self.legacy.persistent_shard_store = Some(Arc::new(store));
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
            self.legacy.log_store.create_shard(
                shard_id,
                tenant_id,
                kiseki_common::ids::NodeId(1),
                ShardConfig::default(),
            );
            self.legacy
                .gateway
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

        self.legacy
            .gateway
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
        self.legacy
            .gateway
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

    // =====================================================================
    // Server harness — spawn a real kiseki-server for @integration steps
    // =====================================================================

    /// Start a server if not already running. Idempotent.
    pub async fn ensure_server(&mut self) -> Result<(), String> {
        if self.server.is_some() {
            return Ok(());
        }
        self.server = Some(steps::harness::ServerHarness::start().await?);
        Ok(())
    }

    /// Get a reference to the running server harness.
    /// Panics if `ensure_server()` was not called.
    pub fn server(&self) -> &steps::harness::ServerHarness {
        self.server
            .as_ref()
            .expect("server not started — call ensure_server() first")
    }

    /// Get a mutable reference to the running server harness.
    pub fn server_mut(&mut self) -> &mut steps::harness::ServerHarness {
        self.server
            .as_mut()
            .expect("server not started — call ensure_server() first")
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

    // Limit scenario concurrency. The default (one scenario per CPU
    // thread) lets ~16 scenarios run in parallel on a beefy laptop, and
    // each `@multi-node` scenario spawns 3-20 `kiseki-server` children
    // via the cluster harness singletons. Three singletons (3 + 6 + 20
    // nodes = 29 children) plus a dozen concurrent in-flight scenarios
    // saturate file descriptors and ports, deadlocking some scenarios
    // mid-step (observed locally as a 50%-completion hang on
    // 2026-05-02). Cap at 4 — enough parallelism that fast unit-style
    // scenarios don't bottleneck on slower @multi-node ones, but few
    // enough that the harness singletons can serve their per-scenario
    // mutex without resource starvation. Each cluster singleton is
    // already serialized by its `OwnedMutexGuard`, so the cap only
    // gates the *cross-singleton* concurrency.
    let runner = KisekiWorld::cucumber()
        .max_concurrent_scenarios(4)
        .filter_run("features/", move |feat, _, sc| {
            if skip_slow && sc.tags.iter().any(|t| t == "slow") {
                return false;
            }
            // Scenarios that require a real OS-level mount (privileged
            // docker container, ktls, kernel pNFS client). The in-process
            // BDD runner can't provide those primitives; the python e2e
            // suite (`tests/e2e/test_pnfs.py`) is the witness. Always
            // skip in the BDD run so a `todo!()` step doesn't fail CI
            // for work that's verified end-to-end elsewhere.
            if sc.tags.iter().any(|t| t == "e2e-deferred") {
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
