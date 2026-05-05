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
    pub cluster_guard: Option<
        tokio::sync::OwnedMutexGuard<crate::steps::cluster_harness::ClusterHarness>,
    >,
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
}

impl NativeWorld {
    /// Resolve a Gherkin tenant name to its `OrgId`. Mints a fresh
    /// UUID on first lookup so the same name persists across all
    /// steps in a scenario.
    pub fn tenant_id_for(&mut self, name: &str) -> OrgId {
        if let Some(id) = self.tenants.get(name) {
            return *id;
        }
        let id = OrgId(uuid::Uuid::new_v4());
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
        self.clients
            .get(name)
            .unwrap_or_else(|| panic!("native client {name:?} not configured (missing Given step?)"))
    }
}
