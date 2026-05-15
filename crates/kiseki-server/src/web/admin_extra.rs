#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::ref_option,
    clippy::manual_strip,
    clippy::map_unwrap_or,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::default_trait_access
)]
//! Operator + user visibility HTTP endpoints.
//!
//! These routes back the new dashboard tabs (Topology, Pools, Tenants,
//! Audit) and the new `kiseki-admin` subcommands (`shards`,
//! `forwarding`, `tenant`, `audit query`, `snapshot`, `drain`, `keys`,
//! `config show`). They are deliberately read-mostly and JSON-only so
//! both the htmx fragments and the stdlib-only `kiseki-admin` binary
//! can consume them without per-tab framing logic.
//!
//! Hard constraint (per the implementer scope): do NOT extend
//! `/cluster/info`'s JSON shape — Step C owns that contract. All new
//! data lives under `/admin/*` so older clients are unaffected.
//!
//! Auth posture matches the rest of `/ui/*` and `/admin/*`: the
//! metrics HTTP server is operator-only (firewalled to admin VLANs in
//! production). A proper RBAC gate is tracked separately in
//! `specs/findings/2026-05-15-ui-cli-followups.md`.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;

use kiseki_audit::store::AuditOps;
use kiseki_audit::AuditQuery;
use kiseki_control::tenant::TenantStore;
use kiseki_keymanager::KeyManagerOps;

use super::api::UiState;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Extend the UI router with the operator-visibility endpoints.
///
/// Returned as a separate router so unit tests can mount the new
/// surface in isolation without bringing up the full UiState.
pub fn admin_extra_routes() -> Router<UiState> {
    Router::new()
        // --- Topology tab ---
        .route("/admin/topology/shards", get(api_topology_shards))
        .route("/admin/topology/forwarding", get(api_topology_forwarding))
        .route(
            "/ui/fragment/topology-shards",
            get(fragment_topology_shards),
        )
        .route(
            "/ui/fragment/topology-forwarding",
            get(fragment_topology_forwarding),
        )
        // --- Pools tab ---
        .route("/admin/pools", get(api_pools))
        .route("/ui/fragment/pools-table", get(fragment_pools_table))
        // --- Tenants tab ---
        .route("/admin/tenants/orgs", get(api_list_orgs))
        .route("/admin/tenants/orgs", post(api_create_org))
        .route("/admin/tenants/projects", get(api_list_projects))
        .route("/admin/tenants/workloads", get(api_list_workloads))
        .route("/admin/tenants/namespaces", get(api_list_namespaces))
        .route("/ui/fragment/tenants-table", get(fragment_tenants_table))
        // --- Audit tab ---
        .route("/admin/audit/query", get(api_audit_query))
        .route("/ui/fragment/audit-table", get(fragment_audit_table))
        // --- Config show ---
        .route("/admin/config", get(api_admin_config))
        // --- Keys ---
        .route("/admin/keys/status", get(api_keys_status))
        .route("/admin/keys/rotate", post(api_keys_rotate))
        // --- Snapshots ---
        .route("/admin/snapshots", get(api_list_snapshots))
        .route("/admin/snapshots", post(api_create_snapshot))
        .route("/admin/snapshots/restore", post(api_restore_snapshot))
        // --- Drain ---
        .route("/admin/drains", get(api_list_drains))
        .route("/admin/drains", post(api_drain))
        .route("/admin/drains/cancel", post(api_drain_cancel))
}

// ---------------------------------------------------------------------------
// Topology — shards & forwarding (proxy)
// ---------------------------------------------------------------------------

async fn api_topology_shards(State(state): State<UiState>) -> impl IntoResponse {
    let shards = super::api::build_shards_for_test(&state).await;
    axum::Json(serde_json::json!({
        "node_id": state.node_info.node_id,
        "shards": shards,
    }))
}

async fn api_topology_forwarding(State(state): State<UiState>) -> impl IntoResponse {
    let metrics_text = (state.metrics_encode)();
    let forwards = parse_counter_with_labels(
        &metrics_text,
        "kiseki_native_proxy_forwards_total",
        &["source_node", "leader_node"],
    );
    let stale = parse_counter_with_labels(
        &metrics_text,
        "kiseki_native_topology_stale_leader_redirects_total",
        &["protocol"],
    );
    let proxy_fallback = std::env::var("KISEKI_NATIVE_PROXY_FALLBACK")
        .ok()
        .map(|v| matches!(v.as_str(), "on" | "1" | "true" | "yes"))
        .unwrap_or(false);

    axum::Json(serde_json::json!({
        "node_id": state.node_info.node_id,
        "proxy_fallback_enabled": proxy_fallback,
        "forwards": forwards.iter().map(sample_to_json).collect::<Vec<_>>(),
        "stale_leader_redirects": stale.iter().map(sample_to_json).collect::<Vec<_>>(),
    }))
}

async fn fragment_topology_shards(State(state): State<UiState>) -> Html<String> {
    use std::fmt::Write;
    let shards = super::api::build_shards_for_test(&state).await;
    let mut html = String::from(
        "<table><thead><tr><th>Shard</th><th>Namespace</th><th>Leader</th><th>Range</th></tr></thead><tbody>",
    );
    if shards.is_empty() {
        html.push_str(
            "<tr><td colspan=\"4\" style=\"color:var(--dim)\">No shards reported (single-node deploy or cold start)</td></tr>",
        );
    } else {
        for s in &shards {
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}…{}</td></tr>",
                short_id(&s.shard_id),
                html_escape(&s.namespace_id),
                s.leader_id
                    .map(|id| format!(
                        "{}@{}",
                        id,
                        s.leader_data_addr.clone().unwrap_or_else(|| "?".into())
                    ))
                    .unwrap_or_else(|| "none".into()),
                short_hex(&s.range_start),
                short_hex(&s.range_end),
            );
        }
    }
    html.push_str("</tbody></table>");
    Html(html)
}

async fn fragment_topology_forwarding(State(state): State<UiState>) -> Html<String> {
    use std::fmt::Write;
    let metrics_text = (state.metrics_encode)();
    let forwards = parse_counter_with_labels(
        &metrics_text,
        "kiseki_native_proxy_forwards_total",
        &["source_node", "leader_node"],
    );
    let stale = parse_counter_with_labels(
        &metrics_text,
        "kiseki_native_topology_stale_leader_redirects_total",
        &["protocol"],
    );
    let proxy_fallback = std::env::var("KISEKI_NATIVE_PROXY_FALLBACK")
        .ok()
        .map(|v| matches!(v.as_str(), "on" | "1" | "true" | "yes"))
        .unwrap_or(false);

    let mut html = String::new();
    let _ = write!(
        html,
        "<div class=\"chart-box\"><h3>Proxy Fallback</h3><div class=\"num\" style=\"font-size:18px\">{}</div><div class=\"sub\">KISEKI_NATIVE_PROXY_FALLBACK on node {}</div></div>",
        if proxy_fallback { "ON" } else { "off" },
        state.node_info.node_id,
    );

    let _ = write!(
        html,
        "<div class=\"chart-box\"><h3>Proxy Forwards</h3><table><thead><tr><th>Source</th><th>Leader</th><th>Count</th></tr></thead><tbody>",
    );
    if forwards.is_empty() {
        let _ = write!(
            html,
            "<tr><td colspan=\"3\" style=\"color:var(--dim)\">No forwards yet</td></tr>",
        );
    } else {
        for row in &forwards {
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                row.labels
                    .get("source_node")
                    .map(String::as_str)
                    .unwrap_or("?"),
                row.labels
                    .get("leader_node")
                    .map(String::as_str)
                    .unwrap_or("?"),
                row.value as u64,
            );
        }
    }
    html.push_str("</tbody></table></div>");

    let _ = write!(
        html,
        "<div class=\"chart-box\"><h3>Stale-Leader Redirects</h3><table><thead><tr><th>Protocol</th><th>Count</th></tr></thead><tbody>",
    );
    if stale.is_empty() {
        let _ = write!(
            html,
            "<tr><td colspan=\"2\" style=\"color:var(--dim)\">No redirects yet</td></tr>",
        );
    } else {
        for row in &stale {
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td></tr>",
                row.labels
                    .get("protocol")
                    .map(String::as_str)
                    .unwrap_or("?"),
                row.value as u64,
            );
        }
    }
    html.push_str("</tbody></table></div>");

    Html(html)
}

// ---------------------------------------------------------------------------
// Pools
// ---------------------------------------------------------------------------

async fn api_pools(State(state): State<UiState>) -> impl IntoResponse {
    let metrics_text = (state.metrics_encode)();
    let total =
        parse_gauge_with_labels(&metrics_text, "kiseki_pool_capacity_total_bytes", &["pool"]);
    let used = parse_gauge_with_labels(&metrics_text, "kiseki_pool_capacity_used_bytes", &["pool"]);

    let mut by_pool: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    for row in &total {
        let pool = row
            .labels
            .get("pool")
            .cloned()
            .unwrap_or_else(|| "default".into());
        by_pool.entry(pool).or_default().0 = row.value as u64;
    }
    for row in &used {
        let pool = row
            .labels
            .get("pool")
            .cloned()
            .unwrap_or_else(|| "default".into());
        by_pool.entry(pool).or_default().1 = row.value as u64;
    }

    let pools: Vec<_> = by_pool
        .into_iter()
        .map(|(name, (total, used))| {
            let free = total.saturating_sub(used);
            let used_pct = if total > 0 {
                (used as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            serde_json::json!({
                "name": name,
                "total_bytes": total,
                "used_bytes": used,
                "free_bytes": free,
                "used_pct": format!("{used_pct:.1}"),
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "pools": pools }))
}

async fn fragment_pools_table(State(state): State<UiState>) -> Html<String> {
    use std::fmt::Write;
    let metrics_text = (state.metrics_encode)();
    let total =
        parse_gauge_with_labels(&metrics_text, "kiseki_pool_capacity_total_bytes", &["pool"]);
    let used = parse_gauge_with_labels(&metrics_text, "kiseki_pool_capacity_used_bytes", &["pool"]);

    let mut by_pool: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    for row in &total {
        let pool = row
            .labels
            .get("pool")
            .cloned()
            .unwrap_or_else(|| "default".into());
        by_pool.entry(pool).or_default().0 = row.value as u64;
    }
    for row in &used {
        let pool = row
            .labels
            .get("pool")
            .cloned()
            .unwrap_or_else(|| "default".into());
        by_pool.entry(pool).or_default().1 = row.value as u64;
    }

    let mut html = String::from(
        "<table><thead><tr><th>Pool</th><th>Used</th><th>Total</th><th>Free</th><th>Used %</th></tr></thead><tbody>",
    );
    if by_pool.is_empty() {
        html.push_str(
            "<tr><td colspan=\"5\" style=\"color:var(--dim)\">No pools reported by this node yet</td></tr>",
        );
    } else {
        for (name, (t, u)) in &by_pool {
            let free = t.saturating_sub(*u);
            let used_pct = if *t > 0 {
                (*u as f64 / *t as f64) * 100.0
            } else {
                0.0
            };
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1}%</td></tr>",
                html_escape(name),
                format_bytes(*u),
                format_bytes(*t),
                format_bytes(free),
                used_pct,
            );
        }
    }
    html.push_str("</tbody></table>");
    Html(html)
}

// ---------------------------------------------------------------------------
// Tenants
// ---------------------------------------------------------------------------

async fn api_list_orgs(State(state): State<UiState>) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return axum::Json(serde_json::json!({"orgs": [], "available": false}));
    };
    let orgs: Vec<_> = store
        .list_orgs()
        .into_iter()
        .map(|o| {
            serde_json::json!({
                "id": o.id,
                "name": o.name,
                "capacity_bytes": o.quota.capacity_bytes,
                "iops": o.quota.iops,
            })
        })
        .collect();
    axum::Json(serde_json::json!({"orgs": orgs, "available": true}))
}

#[derive(serde::Deserialize)]
struct CreateOrgBody {
    name: String,
}

async fn api_create_org(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<CreateOrgBody>,
) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "tenant store not wired on this node"})),
        );
    };
    let org_id = uuid::Uuid::new_v4().to_string();
    let org = kiseki_control::tenant::Organization {
        id: org_id.clone(),
        name: body.name,
        compliance_tags: vec![],
        dedup_policy: kiseki_common::tenancy::DedupPolicy::CrossTenant,
        quota: kiseki_common::tenancy::Quota {
            capacity_bytes: 0,
            iops: 0,
            metadata_ops_per_sec: 0,
        },
        compression_enabled: false,
    };
    match store.create_org(org) {
        Ok(()) => (
            axum::http::StatusCode::CREATED,
            axum::Json(serde_json::json!({"org_id": org_id})),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn api_list_projects(State(state): State<UiState>) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return axum::Json(serde_json::json!({"projects": [], "available": false}));
    };
    let projects: Vec<_> = store
        .list_projects()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "org_id": p.org_id,
                "name": p.name,
                "capacity_bytes": p.quota.capacity_bytes,
            })
        })
        .collect();
    axum::Json(serde_json::json!({"projects": projects, "available": true}))
}

async fn api_list_workloads(State(state): State<UiState>) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return axum::Json(serde_json::json!({"workloads": [], "available": false}));
    };
    let workloads: Vec<_> = store
        .list_workloads()
        .into_iter()
        .map(|w| {
            serde_json::json!({
                "id": w.id,
                "org_id": w.org_id,
                "project_id": w.project_id,
                "name": w.name,
            })
        })
        .collect();
    axum::Json(serde_json::json!({"workloads": workloads, "available": true}))
}

async fn api_list_namespaces(State(state): State<UiState>) -> impl IntoResponse {
    let Some(store) = state.namespaces.as_ref() else {
        return axum::Json(serde_json::json!({"namespaces": [], "available": false}));
    };
    let ns: Vec<_> = store
        .list()
        .into_iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "org_id": n.org_id,
                "project_id": n.project_id,
                "shard_id": n.shard_id,
                "read_only": n.read_only,
            })
        })
        .collect();
    axum::Json(serde_json::json!({"namespaces": ns, "available": true}))
}

async fn fragment_tenants_table(State(state): State<UiState>) -> Html<String> {
    use std::fmt::Write;
    let mut html = String::new();
    let _ = write!(
        html,
        "<div class=\"chart-box\"><h3>Organizations</h3><table><thead><tr><th>ID</th><th>Name</th><th>Capacity</th></tr></thead><tbody>",
    );
    if let Some(store) = state.tenants.as_ref() {
        let orgs = store.list_orgs();
        if orgs.is_empty() {
            html.push_str(
                "<tr><td colspan=\"3\" style=\"color:var(--dim)\">No organizations yet</td></tr>",
            );
        } else {
            for o in &orgs {
                let _ = write!(
                    html,
                    "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                    short_id(&o.id),
                    html_escape(&o.name),
                    format_bytes(o.quota.capacity_bytes),
                );
            }
        }
    } else {
        html.push_str(
            "<tr><td colspan=\"3\" style=\"color:var(--dim)\">Tenant store not wired (single-node deploy)</td></tr>",
        );
    }
    html.push_str("</tbody></table></div>");

    let _ = write!(
        html,
        "<div class=\"chart-box\"><h3>Projects</h3><table><thead><tr><th>ID</th><th>Org</th><th>Name</th></tr></thead><tbody>",
    );
    if let Some(store) = state.tenants.as_ref() {
        let projects = store.list_projects();
        if projects.is_empty() {
            html.push_str(
                "<tr><td colspan=\"3\" style=\"color:var(--dim)\">No projects yet</td></tr>",
            );
        } else {
            for p in &projects {
                let _ = write!(
                    html,
                    "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                    short_id(&p.id),
                    short_id(&p.org_id),
                    html_escape(&p.name),
                );
            }
        }
    }
    html.push_str("</tbody></table></div>");

    let _ = write!(
        html,
        "<div class=\"chart-box\"><h3>Workloads</h3><table><thead><tr><th>ID</th><th>Org</th><th>Project</th><th>Name</th></tr></thead><tbody>",
    );
    if let Some(store) = state.tenants.as_ref() {
        let workloads = store.list_workloads();
        if workloads.is_empty() {
            html.push_str(
                "<tr><td colspan=\"4\" style=\"color:var(--dim)\">No workloads yet</td></tr>",
            );
        } else {
            for w in &workloads {
                let _ = write!(
                    html,
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    short_id(&w.id),
                    short_id(&w.org_id),
                    short_id(&w.project_id),
                    html_escape(&w.name),
                );
            }
        }
    }
    html.push_str("</tbody></table></div>");
    Html(html)
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct AuditQueryParams {
    /// Tenant id (UUID). Empty / unset = system shard.
    tenant: Option<String>,
    /// Event type filter — see `kiseki_audit::AuditEventType` variants.
    event_type: Option<String>,
    /// Severity — currently unused on the AuditOps surface; reserved
    /// so the CLI can pre-filter without changes here.
    #[allow(dead_code)]
    severity: Option<String>,
    /// Maximum events to return. Default 200.
    limit: Option<usize>,
    /// Sequence to start from (inclusive). Default 1.
    from: Option<u64>,
}

async fn api_audit_query(
    State(state): State<UiState>,
    Query(params): Query<AuditQueryParams>,
) -> impl IntoResponse {
    let Some(audit) = state.audit.as_ref() else {
        return axum::Json(serde_json::json!({"events": [], "available": false}));
    };
    let tenant_id = parse_tenant(&params.tenant);
    let event_type = parse_event_type(params.event_type.as_deref());
    let query = AuditQuery {
        tenant_id,
        from: kiseki_common::ids::SequenceNumber(params.from.unwrap_or(1)),
        limit: params.limit.unwrap_or(200),
        event_type,
    };
    let events = audit.query(&query);
    let total = audit.total_events();
    let events_json: Vec<_> = events
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "sequence": e.sequence.0,
                "type": format!("{:?}", e.event_type),
                "tenant_id": e.tenant_id.map(|t| t.0.to_string()),
                "actor": e.actor,
                "description": e.description,
                "wall_ms": e.timestamp.wall.millis_since_epoch,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "events": events_json,
        "total_events": total,
        "available": true,
    }))
}

async fn fragment_audit_table(
    State(state): State<UiState>,
    Query(params): Query<AuditQueryParams>,
) -> Html<String> {
    use std::fmt::Write;
    let Some(audit) = state.audit.as_ref() else {
        return Html(String::from(
            "<div class=\"alert-row\"><span class=\"dot blue\"></span><span class=\"msg\" style=\"color:var(--dim)\">Audit store not wired</span></div>",
        ));
    };
    let tenant_id = parse_tenant(&params.tenant);
    let event_type = parse_event_type(params.event_type.as_deref());
    let query = AuditQuery {
        tenant_id,
        from: kiseki_common::ids::SequenceNumber(params.from.unwrap_or(1)),
        limit: params.limit.unwrap_or(50),
        event_type,
    };
    let events = audit.query(&query);

    let mut html = String::from(
        "<table><thead><tr><th>Seq</th><th>Type</th><th>Tenant</th><th>Actor</th><th>Description</th></tr></thead><tbody>",
    );
    if events.is_empty() {
        html.push_str(
            "<tr><td colspan=\"5\" style=\"color:var(--dim)\">No audit events recorded</td></tr>",
        );
    } else {
        for e in events.iter().rev() {
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{:?}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                e.sequence.0,
                e.event_type,
                e.tenant_id
                    .map(|t| short_id(&t.0.to_string()))
                    .unwrap_or_else(|| "system".into()),
                html_escape(&e.actor),
                html_escape(&e.description),
            );
        }
    }
    html.push_str("</tbody></table>");
    Html(html)
}

// ---------------------------------------------------------------------------
// Config show
// ---------------------------------------------------------------------------

async fn api_admin_config(State(state): State<UiState>) -> impl IntoResponse {
    let knobs = [
        "KISEKI_NATIVE_PROXY_FALLBACK",
        "KISEKI_NATIVE_TRANSPORT",
        "KISEKI_RAFT_FLUSH_INTERVAL_MS",
        "KISEKI_COMPOSITION_FLUSH_INTERVAL_MS",
        "KISEKI_CHUNK_FLUSH_INTERVAL_MS",
        "KISEKI_PEER_DATA_ADDRS",
        "KISEKI_NATIVE_GATEWAY_POOL",
        "KISEKI_BACKUP_BACKEND",
        "KISEKI_ENABLE_TEST_KNOBS",
        "KISEKI_BDD_FAST",
        "KISEKI_DATA_ADDR",
        "KISEKI_ENDPOINT",
    ];
    let mut config = serde_json::Map::new();
    for k in knobs {
        let v = std::env::var(k).unwrap_or_default();
        config.insert(
            k.to_string(),
            serde_json::Value::String(if v.is_empty() {
                "(unset)".to_string()
            } else {
                v
            }),
        );
    }
    axum::Json(serde_json::json!({
        "node_id": state.node_info.node_id,
        "config": config,
    }))
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

async fn api_keys_status(State(state): State<UiState>) -> impl IntoResponse {
    let Some(km) = state.key_manager.as_ref() else {
        return axum::Json(serde_json::json!({"available": false}));
    };
    let current = km.current_epoch().await.ok().map(|e| e.0);
    let epochs = km.list_epochs().await;
    let epochs_json: Vec<_> = epochs
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "epoch": e.epoch.0,
                "is_current": e.is_current,
                "migration_complete": e.migration_complete,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "available": true,
        "current_epoch": current,
        "epochs": epochs_json,
    }))
}

async fn api_keys_rotate(State(state): State<UiState>) -> impl IntoResponse {
    let Some(km) = state.key_manager.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "key manager not wired"})),
        );
    };
    match km.rotate().await {
        Ok(epoch) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "message": format!("rotated to epoch {}", epoch.0),
                "new_epoch": epoch.0,
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

async fn api_list_snapshots(State(_state): State<UiState>) -> impl IntoResponse {
    let mgr = crate::backup::runtime_backup_manager();
    let Some(mgr) = mgr else {
        return axum::Json(serde_json::json!({"available": false, "snapshots": []}));
    };
    let snaps = mgr.list_snapshots().await;
    let json: Vec<_> = snaps
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "snapshot_id": s.snapshot_id,
                "metadata_bytes": s.metadata_bytes,
                "data_bytes": s.data_bytes,
                "shard_count": s.shard_count,
                "created_at": s.created_at,
            })
        })
        .collect();
    axum::Json(serde_json::json!({"available": true, "snapshots": json}))
}

#[derive(serde::Deserialize, Default)]
struct CreateSnapshotBody {
    #[allow(dead_code)]
    note: Option<String>,
}

async fn api_create_snapshot(
    State(_state): State<UiState>,
    axum::Json(_body): axum::Json<CreateSnapshotBody>,
) -> impl IntoResponse {
    let mgr = crate::backup::runtime_backup_manager();
    let Some(mgr) = mgr else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "backups not configured"})),
        );
    };
    // No shard provider wired through UiState — snapshots created via
    // the HTTP path capture an empty shard set today. The full gRPC
    // path (`admin_grpc::AdminGrpc`) is still used for production
    // backups; this endpoint exists for the operator CLI ergonomics.
    match mgr.create_snapshot(&[]).await {
        Ok(snap) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "snapshot_id": snap.snapshot_id,
                "metadata_bytes": snap.metadata_bytes,
                "data_bytes": snap.data_bytes,
                "shard_count": snap.shard_count,
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[derive(serde::Deserialize)]
struct RestoreSnapshotBody {
    snapshot_id: String,
}

async fn api_restore_snapshot(
    State(_state): State<UiState>,
    axum::Json(body): axum::Json<RestoreSnapshotBody>,
) -> impl IntoResponse {
    let mgr = crate::backup::runtime_backup_manager();
    let Some(mgr) = mgr else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "backups not configured"})),
        );
    };
    if body.snapshot_id.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "snapshot_id is required"})),
        );
    }
    match mgr.restore_snapshot(&body.snapshot_id).await {
        Ok(shards) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "shards_restored": shards.len(),
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

// ---------------------------------------------------------------------------
// Drains
// ---------------------------------------------------------------------------

async fn api_list_drains(State(state): State<UiState>) -> impl IntoResponse {
    let Some(orch) = state.drain.as_ref() else {
        return axum::Json(serde_json::json!({"available": false, "drains": []}));
    };
    let snap = orch.snapshot();
    let drains: Vec<_> = snap
        .into_iter()
        .map(|(id, rec)| {
            serde_json::json!({
                "node_id": id.0,
                "state": format!("{:?}", rec.state),
                "voter_in_shards": rec.voter_in_shards.len(),
                "drain_progress": rec.drain_progress.as_ref().map(|p| serde_json::json!({
                    "total_shards": p.total_shards,
                    "completed_shards": p.completed_shards,
                })),
            })
        })
        .collect();
    axum::Json(serde_json::json!({"available": true, "drains": drains}))
}

#[derive(serde::Deserialize)]
struct DrainBody {
    node_id: u64,
    admin: Option<String>,
}

async fn api_drain(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<DrainBody>,
) -> impl IntoResponse {
    let Some(orch) = state.drain.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "drain orchestrator not wired"})),
        );
    };
    let admin = body.admin.unwrap_or_else(|| "admin-cli".to_string());
    match orch.request_drain(kiseki_common::ids::NodeId(body.node_id), &admin) {
        Ok(()) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "message": format!("drain requested for node {}", body.node_id),
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

async fn api_drain_cancel(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<DrainBody>,
) -> impl IntoResponse {
    let Some(orch) = state.drain.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "drain orchestrator not wired"})),
        );
    };
    let admin = body.admin.unwrap_or_else(|| "admin-cli".to_string());
    match orch.cancel_drain(kiseki_common::ids::NodeId(body.node_id), &admin) {
        Ok(()) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "status": "ok",
                "message": format!("drain cancelled for node {}", body.node_id),
            })),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

// ---------------------------------------------------------------------------
// Public type aliases for `UiState`
// ---------------------------------------------------------------------------

/// Trait object for the audit store. `Arc<dyn AuditOps>` lets the
/// runtime wire either the in-memory `AuditLog` or a Raft-backed
/// implementation behind the same handle.
pub type AuditHandle = Arc<dyn AuditOps + Send + Sync>;

/// Trait object for the key manager.
pub type KeyManagerHandle = Arc<dyn KeyManagerOps + Send + Sync>;

/// Tenant store handle.
pub type TenantHandle = Arc<TenantStore>;

/// Namespace store handle.
pub type NamespaceHandle = Arc<kiseki_control::namespace::NamespaceStore>;

/// Drain orchestrator handle.
pub type DrainHandle = Arc<kiseki_control::node_lifecycle::DrainOrchestrator>;

// ---------------------------------------------------------------------------
// Prometheus text helpers
// ---------------------------------------------------------------------------

/// One sample row parsed from Prometheus text format.
#[derive(Debug, Clone)]
struct Sample {
    labels: std::collections::HashMap<String, String>,
    value: f64,
}

fn sample_to_json(s: &Sample) -> serde_json::Value {
    serde_json::json!({
        "labels": s.labels,
        "value": s.value,
    })
}

fn parse_counter_with_labels(text: &str, name: &str, _labels: &[&str]) -> Vec<Sample> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        // Match either `name ` (no labels) or `name{`.
        let matched = if line.starts_with(name) {
            let after = &line[name.len()..];
            after.starts_with('{') || after.starts_with(' ')
        } else {
            false
        };
        if !matched {
            continue;
        }
        if let Some(sample) = parse_sample_after_name(&line[name.len()..]) {
            out.push(sample);
        }
    }
    out
}

fn parse_gauge_with_labels(text: &str, name: &str, labels: &[&str]) -> Vec<Sample> {
    parse_counter_with_labels(text, name, labels)
}

fn parse_sample_after_name(rest: &str) -> Option<Sample> {
    let rest = rest.trim_start();
    let (labels_str, value_str) = if let Some(stripped) = rest.strip_prefix('{') {
        let end = stripped.find('}')?;
        let label_block = &stripped[..end];
        let value_part = stripped[end + 1..].trim();
        (Some(label_block), value_part)
    } else {
        (None, rest.trim_start())
    };
    let value = value_str
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())?;
    let mut labels = std::collections::HashMap::new();
    if let Some(block) = labels_str {
        for pair in block.split(',') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                let v = v.trim().trim_matches('"');
                labels.insert(k.trim().to_string(), v.to_string());
            }
        }
    }
    Some(Sample { labels, value })
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

fn short_id(s: &str) -> String {
    if s.len() <= 12 {
        s.to_string()
    } else {
        format!("{}…", &s[..12])
    }
}

fn short_hex(s: &str) -> String {
    if s.len() <= 8 {
        s.to_string()
    } else {
        s[..8].to_string()
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_099_511_627_776 {
        format!("{:.1} TB", bytes as f64 / 1_099_511_627_776.0)
    } else if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn parse_tenant(s: &Option<String>) -> Option<kiseki_common::ids::OrgId> {
    s.as_deref()
        .filter(|s| !s.is_empty() && *s != "system")
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(kiseki_common::ids::OrgId)
}

fn parse_event_type(s: Option<&str>) -> Option<kiseki_audit::AuditEventType> {
    use kiseki_audit::AuditEventType as T;
    let s = s?.to_ascii_lowercase();
    let s = s.replace('-', "");
    Some(match s.as_str() {
        "keygeneration" => T::KeyGeneration,
        "keyrotation" => T::KeyRotation,
        "keydestruction" => T::KeyDestruction,
        "keyaccess" => T::KeyAccess,
        "reencryption" => T::ReEncryption,
        "dataread" => T::DataRead,
        "datawrite" => T::DataWrite,
        "datadelete" => T::DataDelete,
        "authsuccess" => T::AuthSuccess,
        "authfailure" => T::AuthFailure,
        "tenantlifecycle" => T::TenantLifecycle,
        "adminaction" => T::AdminAction,
        "policychange" => T::PolicyChange,
        "maintenancemode" => T::MaintenanceMode,
        "advisoryworkflow" => T::AdvisoryWorkflow,
        "advisoryhint" => T::AdvisoryHint,
        "advisorybudgetexceeded" => T::AdvisoryBudgetExceeded,
        "securitydowngradeenabled" => T::SecurityDowngradeEnabled,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_no_labels() {
        let s = parse_sample_after_name(" 42").unwrap();
        assert!(s.labels.is_empty());
        assert!((s.value - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_sample_with_labels() {
        let s = parse_sample_after_name(r#"{source_node="1",leader_node="2"} 7"#).unwrap();
        assert_eq!(s.labels.get("source_node").unwrap(), "1");
        assert_eq!(s.labels.get("leader_node").unwrap(), "2");
        assert!((s.value - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_counter_filters_out_other_metrics() {
        let text = "# HELP foo\n\
                    # TYPE foo counter\n\
                    foo 1\n\
                    foo_bar{x=\"y\"} 99\n\
                    other 100\n";
        let rows = parse_counter_with_labels(text, "foo", &[]);
        // Prefix-check requires the next char to be `{` or ` ` so
        // `foo_bar` is filtered out.
        assert_eq!(rows.len(), 1);
        assert!((rows[0].value - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn event_type_parsing_accepts_dashes_and_case() {
        assert!(matches!(
            parse_event_type(Some("key-rotation")),
            Some(kiseki_audit::AuditEventType::KeyRotation)
        ));
        assert!(matches!(
            parse_event_type(Some("KEYROTATION")),
            Some(kiseki_audit::AuditEventType::KeyRotation)
        ));
        assert!(parse_event_type(Some("bogus")).is_none());
    }

    #[test]
    fn tenant_parsing_handles_system_and_empty() {
        assert!(parse_tenant(&None).is_none());
        assert!(parse_tenant(&Some(String::new())).is_none());
        assert!(parse_tenant(&Some("system".into())).is_none());
        let id = uuid::Uuid::new_v4();
        let parsed = parse_tenant(&Some(id.to_string())).unwrap();
        assert_eq!(parsed.0, id);
    }

    #[test]
    fn short_id_truncates_long_uuids() {
        let long = "12345678901234567890";
        // 12 ASCII chars + 3 bytes for the "…" ellipsis = 15
        let s = short_id(long);
        assert_eq!(s.chars().count(), 13);
        assert!(s.ends_with('…'));
    }

    // --- D2: per-device pool gauges show up in /admin/pools shape -----

    #[test]
    fn parse_device_gauge_with_pool_and_kind_labels() {
        let text = "# HELP kiseki_pool_device_capacity_bytes Per-device capacity\n\
                    # TYPE kiseki_pool_device_capacity_bytes gauge\n\
                    kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-1\",kind=\"total\"} 1000\n\
                    kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-1\",kind=\"used\"} 400\n\
                    kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-1\",kind=\"free\"} 600\n\
                    kiseki_pool_device_capacity_bytes{pool=\"warm\",device_id=\"d-2\",kind=\"total\"} 5000\n";
        let rows = parse_gauge_with_labels(
            text,
            "kiseki_pool_device_capacity_bytes",
            &["pool", "device_id", "kind"],
        );
        assert_eq!(rows.len(), 4, "expected 4 sample rows, got {}", rows.len());
        let hot_d1_total = rows
            .iter()
            .find(|s| {
                s.labels.get("pool").map(String::as_str) == Some("hot")
                    && s.labels.get("device_id").map(String::as_str) == Some("d-1")
                    && s.labels.get("kind").map(String::as_str) == Some("total")
            })
            .expect("hot/d-1/total row");
        assert!((hot_d1_total.value - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn build_per_device_pool_rows_groups_by_device() {
        let text = "kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-1\",kind=\"total\"} 1000\n\
                    kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-1\",kind=\"used\"} 400\n\
                    kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-1\",kind=\"free\"} 600\n\
                    kiseki_pool_device_capacity_bytes{pool=\"hot\",device_id=\"d-2\",kind=\"total\"} 2000\n";
        let rows = build_per_device_rows(text);
        assert_eq!(rows.len(), 2, "expected 2 device rows, got {}", rows.len());
        let d1 = rows
            .iter()
            .find(|r| r.device_id == "d-1")
            .expect("d-1 row");
        assert_eq!(d1.pool, "hot");
        assert_eq!(d1.total_bytes, 1000);
        assert_eq!(d1.used_bytes, 400);
        assert_eq!(d1.free_bytes, 600);
        let d2 = rows.iter().find(|r| r.device_id == "d-2").expect("d-2 row");
        assert_eq!(d2.total_bytes, 2000);
        // Missing `used`/`free` rows default to 0; free derives from
        // total - used when missing.
        assert_eq!(d2.used_bytes, 0);
        assert_eq!(d2.free_bytes, 2000);
    }
}
