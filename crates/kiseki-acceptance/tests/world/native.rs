#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Per-scenario state for `@native` (ADR-042) BDD scenarios.
//!
//! The 1-node mTLS cluster itself is a process-level singleton (see
//! `steps::cluster_harness::acquire_cluster_1_mtls`). This struct
//! holds the per-scenario named-client registry and the typed
//! mappings the Gherkin steps need (`tenant_name → OrgId`,
//! `namespace_name → NamespaceId`, etc.) so each scenario flows
//! cleanly between its `Given` / `When` / `Then` steps.

use std::collections::HashMap;
use std::sync::Arc;

use kiseki_client::native::NativeClient;
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

/// One entry per named client (`client-a`, `client-b`, ...). Holds
/// the dialled tonic Channel + the SAN URI on the cert + the tenant
/// the cert binds to.
pub struct NamedClient {
    pub san_uri: String,
    pub tenant_name: String,
    pub tenant_id: OrgId,
    pub client: Arc<NativeClient>,
}

#[derive(Default)]
pub struct NativeWorld {
    /// Lock on the 1-node mTLS cluster, held for the lifetime of the
    /// scenario. Drop releases it. cucumber-rs runs scenarios
    /// concurrently and the singleton must be serialized for
    /// destructive scenarios (drain mode).
    pub cluster_guard:
        Option<tokio::sync::OwnedMutexGuard<crate::steps::cluster_harness::ClusterHarness>>,
    /// Named clients keyed by Gherkin name (`client-a`).
    pub clients: HashMap<String, NamedClient>,
    /// Tenant name → UUID. Reused across scenarios for the same name
    /// so audit principals remain stable.
    pub tenants: HashMap<String, OrgId>,
    /// Namespace name → UUID, scoped per-scenario.
    pub namespaces: HashMap<String, NamespaceId>,
    /// Last successful PUT — captured for follow-up GET / Lookup
    /// steps.
    pub last_composition: Option<CompositionId>,
    pub last_etag: Option<Vec<u8>>,
    /// Last gRPC error captured by a `When` step. The matching `Then`
    /// step asserts on its code / message.
    pub last_status: Option<tonic::Status>,
    /// Configured per-stream cap (set by `Given the per-stream cap is
    /// 64 MiB`); steps read this when they need to mint a payload of
    /// the right size. Bytes.
    pub per_stream_cap_bytes: Option<u64>,
    /// Configured inline threshold. Bytes.
    pub inline_threshold_bytes: Option<u64>,
    /// Audit-event sightings during the scenario — tracked through a
    /// hookable `AuditSink` injected into the SanInterceptor.
    pub audit_security_events: Vec<(String, Option<String>)>,
    /// Optional crypto boundary the scenario set on the namespace
    /// before exercising it (default: ServerOnly).
    pub crypto_boundary: Option<String>,
    /// Topology version observed on the most recent successful RPC's
    /// trailing metadata, when the step captured it.
    pub last_topology_version: Option<u64>,
    /// Synthetic `TopologyCache` driven by `@routing` / `@topology`
    /// scenarios — exercises the per-edge selector + version-regress
    /// logic without spinning up a full multi-node mTLS cluster.
    /// Empty for scenarios that don't touch routing/topology paths.
    pub topology_cache: Option<std::sync::Arc<kiseki_client::native::TopologyCache>>,
    /// Local-environment binding capabilities for `@routing` —
    /// scenarios set this from the "the local client environment
    /// has X available" Given step.
    pub local_capabilities: Option<kiseki_client::native::LocalCapabilities>,
    /// Per-node edge-selection outcomes captured by the `When the
    /// client opens connections...` step. Keyed by node_id.
    pub edge_selections: HashMap<u64, kiseki_client::native::EdgeSelection>,
    /// Synthetic ConnectionPool driven by the @binding-restart /
    /// @drain scenarios.
    pub connection_pool: Option<std::sync::Arc<kiseki_client::native::ConnectionPool>>,
    /// Drain accounting captured per scenario — the count of edges
    /// reconcile_with_topology marked as draining on the most
    /// recent observed topology change.
    pub last_drained: Option<usize>,
    /// Selector report captured by @binding-probe scenarios — used
    /// by the Then steps to assert on banner content / error
    /// shape.
    pub selector_outcome: Option<SelectorOutcomeStash>,
    /// Background TCP listeners kept alive for the lifetime of a
    /// @binding-restart scenario. Each `JoinHandle` drives an
    /// accept loop that swallows incoming connections so the
    /// `TcpFramedClient::connect_plaintext` handshake succeeds.
    /// The handles abort on Drop (scenario teardown).
    pub synthetic_listeners: Vec<tokio::task::JoinHandle<()>>,
    /// Per-scenario typed scratch space — keyed by step-defined
    /// strings, values are `Box<dyn Any + Send + Sync>` so a new
    /// scenario family can stash an arbitrary type without adding a
    /// dedicated field. Used by the ADR-042 §4 proxy-gate scenarios to
    /// hold the `Arc<ProxyClient>` and the `validate_forward`
    /// result across step boundaries.
    pub scratch: HashMap<String, Box<dyn std::any::Any + Send + Sync>>,
}

/// Captured selector outcome for `@binding-probe` scenarios. Either
/// the (plan, report) pair or the typed error.
pub enum SelectorOutcomeStash {
    /// Successful selector run — both plan and report stashed.
    Success {
        plan: kiseki_transport::native::SelectorPlan,
        report: kiseki_transport::native::SelectorReport,
    },
    /// Failed selector run — error captured for `Then` assertions.
    Failure(kiseki_transport::native::BindingSelectorError),
}

impl NativeWorld {
    /// Resolve a Gherkin tenant name to its `OrgId`.
    ///
    /// The first tenant a Background asks for ("org-pharma" in
    /// `native-gateway.feature`) is mapped to the cluster's bootstrap
    /// tenant — the only tenant that the harness's S3 bucket-create
    /// path registers namespaces under today, since the existing S3
    /// gateway path is single-tenant. Subsequent tenants get fresh
    /// UUIDs; scenarios that exercise multi-tenant rejection (e.g.
    /// "org-bank" vs "org-pharma") rely on those mismatches.
    pub fn tenant_id_for(&mut self, name: &str) -> OrgId {
        if let Some(id) = self.tenants.get(name) {
            return *id;
        }
        let id = if self.tenants.is_empty() {
            // Bootstrap tenant — matches `runtime.rs`'s
            // `Uuid::from_u128(1)` constant. The S3 bucket-create
            // path registers under this tenant; native gateway
            // scenarios talking to those buckets must use the same.
            OrgId(uuid::Uuid::from_u128(1))
        } else {
            OrgId(uuid::Uuid::new_v4())
        };
        self.tenants.insert(name.to_string(), id);
        id
    }

    /// Resolve a Gherkin namespace name to its `NamespaceId`.
    pub fn namespace_id_for(&mut self, name: &str) -> NamespaceId {
        if let Some(id) = self.namespaces.get(name) {
            return *id;
        }
        let id = NamespaceId(uuid::Uuid::new_v4());
        self.namespaces.insert(name.to_string(), id);
        id
    }

    /// Borrow a previously-configured client. Panics if the name
    /// hasn't been declared via the `Given native client "<name>" is
    /// configured ...` step.
    pub fn client(&self, name: &str) -> &NamedClient {
        self.clients.get(name).unwrap_or_else(|| {
            panic!("native client {name:?} not configured (missing Given step?)")
        })
    }
}
