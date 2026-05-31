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
        .route("/admin/topology/shards/split", post(api_split_shard))
        .route("/admin/topology/shards/merge", post(api_merge_shards))
        .route(
            "/admin/topology/namespaces",
            post(api_create_sharded_namespace),
        )
        // ADR-024 2026-05-31 amendment §"three-tier durability":
        // per-namespace size-band pool selector + tier-policy POST.
        .route(
            "/admin/topology/namespaces/{namespace_id}/size-band-pools",
            post(api_set_namespace_size_band_pools),
        )
        .route(
            "/admin/topology/namespaces/{namespace_id}/tier-policy",
            post(api_set_namespace_tier_policy),
        )
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
        // ADR-030 amendment §"admin-driven metadata device role" —
        // cluster-wide aggregation of per-node metadata-capacity
        // gauges → `cluster_max_files` estimate.
        .route(
            "/admin/storage/cluster-capacity",
            get(api_cluster_metadata_capacity),
        )
        // ADR-048 §"Backpressure" + I-SE6 — slab-EC compactor
        // backlog per pool; gateway gates async-ack on this.
        .route("/admin/storage/compactor", get(api_compactor_backlog))
        // --- Device / pool resize (ADR-025 StorageAdminService bridge) ---
        .route("/admin/storage/devices", get(api_list_devices))
        .route("/admin/storage/devices/add", post(api_add_device))
        .route("/admin/storage/devices/remove", post(api_remove_device))
        .route("/admin/storage/devices/evacuate", post(api_evacuate_device))
        .route("/admin/storage/pools/rebalance", post(api_rebalance_pool))
        .route(
            "/admin/storage/pools/thresholds",
            post(api_set_pool_thresholds),
        )
        // --- Pool CRUD (ADR-024 2026-05-31 amendment) ---
        .route("/admin/storage/pools", get(api_list_pools_admin))
        .route("/admin/storage/pools/{pool}", get(api_describe_pool))
        .route("/admin/storage/pools/create", post(api_create_pool))
        // --- Tenants tab ---
        .route("/admin/tenants/orgs", get(api_list_orgs))
        .route("/admin/tenants/orgs", post(api_create_org))
        .route("/admin/tenants/projects", get(api_list_projects))
        .route("/admin/tenants/projects", post(api_create_project))
        .route("/admin/tenants/workloads", get(api_list_workloads))
        .route("/admin/tenants/workloads", post(api_create_workload))
        .route("/admin/tenants/namespaces", get(api_list_namespaces))
        .route("/admin/tenants/namespaces", post(api_create_namespace))
        .route("/admin/tenants/describe", get(api_tenant_describe))
        .route("/admin/tenants/delete", post(api_tenant_delete))
        .route("/ui/fragment/tenants-table", get(fragment_tenants_table))
        // --- Audit tab ---
        .route("/admin/audit/query", get(api_audit_query))
        .route("/ui/fragment/audit-table", get(fragment_audit_table))
        // --- Config show ---
        .route("/admin/config", get(api_admin_config))
        // --- Keys ---
        .route("/admin/keys/status", get(api_keys_status))
        .route("/admin/keys/rotate", post(api_keys_rotate))
        .route("/admin/keys/shred", post(api_keys_shred))
        // --- whoami (D6) ---
        .route("/admin/whoami", get(api_whoami))
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

/// Resolve the local node's data-port URL (`http://127.0.0.1:PORT`)
/// from `KISEKI_DATA_ADDR`, defaulting to 9100. The split/merge
/// bridges below dial this URL to invoke the local
/// `StorageAdminService` gRPC.
fn local_data_endpoint_url() -> String {
    let data_addr = std::env::var("KISEKI_DATA_ADDR").unwrap_or_else(|_| "0.0.0.0:9100".into());
    let port = data_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(9100);
    format!("http://127.0.0.1:{port}")
}

/// Map a tonic `Status` to the right HTTP code + JSON body. Mirrors
/// the kiseki-storage CLI's status formatter so HTTP and CLI users
/// see consistent error shapes.
fn grpc_status_to_http(status: &tonic::Status) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    match status.code() {
        tonic::Code::NotFound => StatusCode::NOT_FOUND,
        tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
        tonic::Code::FailedPrecondition => StatusCode::CONFLICT,
        tonic::Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Connect to the local data-port gRPC service. Single-shot (no
/// pool) — split/merge are rare admin ops.
async fn dial_local_storage_admin(
) -> Result<tonic::transport::Channel, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    use axum::http::StatusCode;
    let endpoint_url = local_data_endpoint_url();
    let endpoint = tonic::transport::Endpoint::from_shared(endpoint_url.clone())
        .map(|e| e.timeout(std::time::Duration::from_secs(30)))
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": format!("invalid local data endpoint ({endpoint_url}): {err}"),
                })),
            )
        })?;
    endpoint.connect().await.map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({
                "error": format!("connect to local data port: {err}"),
                "endpoint": endpoint_url,
            })),
        )
    })
}

/// `POST /admin/topology/shards/split` — split a shard via the
/// `StorageAdminService.SplitShard` gRPC on the local node's data
/// port. Closes #59 by exposing the existing ADR-033 §4 hook over
/// HTTP so the `kiseki-admin shard split` CLI (stdlib-only HTTP)
/// can drive multi-shard topology mutations without a tonic
/// dependency.
///
/// Body: `{"shard_id": "<uuid>", "pivot_key": "<32-hex>"?}`. Empty
/// `pivot_key` lets the gRPC handler default to the source range's
/// midpoint (the common case for operator-initiated splits).
///
/// All leader-forwarding + 15 s retry semantics live in the gRPC
/// handler (`storage_admin::split_shard`); this is a thin HTTP →
/// gRPC bridge.
async fn api_split_shard(
    State(state): State<UiState>,
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    use kiseki_proto::v1::SplitShardRequest;

    let parsed: serde_json::Value = if body.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!("invalid JSON body: {e}"),
                    })),
                );
            }
        }
    };
    let shard_id = parsed
        .get("shard_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if shard_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "shard_id is required",
            })),
        );
    }
    let pivot_key = parsed
        .get("pivot_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);

    match client
        .split_shard(SplitShardRequest {
            shard_id: shard_id.clone(),
            pivot_key,
        })
        .await
    {
        Ok(resp) => {
            let r = resp.into_inner();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "node_id": state.node_info.node_id,
                    "source_shard_id": shard_id,
                    "left_shard_id": r.left_shard_id,
                    "right_shard_id": r.right_shard_id,
                    "committed_at_log_index": r.committed_at_log_index,
                })),
            )
        }
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({
                "error": status.message().to_string(),
                "grpc_code": format!("{:?}", status.code()),
            })),
        ),
    }
}

/// `POST /admin/topology/shards/merge` — merge two adjacent shards
/// via `StorageAdminService.MergeShards`. The `left` shard's range
/// expands to cover `right`'s; `right` is retired (ADR-033 §4).
///
/// Body: `{"left_shard_id": "<uuid>", "right_shard_id": "<uuid>"}`.
async fn api_merge_shards(
    State(state): State<UiState>,
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    use kiseki_proto::v1::MergeShardsRequest;

    let parsed: serde_json::Value = if body.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!("invalid JSON body: {e}"),
                    })),
                );
            }
        }
    };
    let left_shard_id = parsed
        .get("left_shard_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let right_shard_id = parsed
        .get("right_shard_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if left_shard_id.is_empty() || right_shard_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "left_shard_id and right_shard_id are required",
            })),
        );
    }

    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);

    match client
        .merge_shards(MergeShardsRequest {
            left_shard_id: left_shard_id.clone(),
            right_shard_id: right_shard_id.clone(),
        })
        .await
    {
        Ok(resp) => {
            let r = resp.into_inner();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "node_id": state.node_info.node_id,
                    "left_shard_id": left_shard_id,
                    "right_shard_id": right_shard_id,
                    "merged_shard_id": r.merged_shard_id,
                    "committed_at_log_index": r.committed_at_log_index,
                })),
            )
        }
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({
                "error": status.message().to_string(),
                "grpc_code": format!("{:?}", status.code()),
            })),
        ),
    }
}

/// `POST /admin/topology/namespaces` — create a namespace whose
/// `NamespaceShardMap` already covers N shards at inception (#68,
/// blocks #66 fix 2). The internal API `compute_shard_ranges` +
/// `ControlCommand::CreateNamespace` already exists; this route is
/// the thin admin-HTTP surface that lets `kiseki-admin` (and
/// `infra/gcp/scripts/setup-shards.sh`) drive it.
///
/// Body: `{"namespace_id": "<utf-8 id>", "tenant_id": "<uuid>",
/// "shards": <u32>?}`. `shards` defaults to
/// `compute_initial_shards(default_config, active_node_count)` —
/// typically `3 × node_count` capped at 64.
///
/// Response: `{"namespace_id": ..., "shard_count": N,
/// "shards": [{"shard_id", "range_start", "range_end",
/// "leader_node"}, ...]}` — the full topology the apply hooks
/// will install across the cluster.
///
/// Idempotent: a repeat call with the same `namespace_id` returns
/// 409 with `existing_shard_count` so callers can no-op without
/// parsing the error message. Mirrors the convention `api_create_org`
/// uses for orgs.
#[allow(clippy::too_many_lines)] // single linear flow: parse → validate → build → submit → format
async fn api_create_sharded_namespace(
    State(state): State<UiState>,
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_common::ids::{NodeId, OrgId};
    use kiseki_control::shard_topology::{
        compute_initial_shards, compute_shard_ranges, ShardTopologyConfig,
    };

    let (Some(cluster_control), Some(cluster_control_store)) = (
        state.cluster_control.as_ref(),
        state.cluster_control_store.as_ref(),
    ) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "cluster control not wired (single-node deploy?)"
            })),
        );
    };

    let parsed: serde_json::Value = if body.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!("invalid JSON body: {e}"),
                    })),
                );
            }
        }
    };

    let namespace_id = parsed
        .get("namespace_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if namespace_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "namespace_id is required",
            })),
        );
    }
    let tenant_id_str = parsed
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tenant_id = match uuid::Uuid::parse_str(&tenant_id_str) {
        Ok(u) => OrgId(u),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": format!("tenant_id must be a UUID: {e}"),
                })),
            );
        }
    };
    let requested_shards = parsed.get("shards").and_then(serde_json::Value::as_u64);

    // ADR-045 §D3: optional tier policy — array of {tier, quota_bytes}
    // in spill order. Absent → empty (default fastest-fit placement).
    let tier_policy: Vec<kiseki_composition::namespace::TierQuota> = parsed
        .get("tier_policy")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let tier = e.get("tier")?.as_str()?.to_owned();
                    let quota_bytes = e
                        .get("quota_bytes")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    Some(kiseki_composition::namespace::TierQuota { tier, quota_bytes })
                })
                .collect()
        })
        .unwrap_or_default();

    // ADR-024 2026-05-31 amendment §"three-tier durability" — optional
    // per-namespace size-band pool selector. JSON shape:
    //   "size_band_pools": { "inline": "<pool>", "replicated": "<pool>",
    //                        "ec": "<pool>" }
    // Any missing field falls through to the cluster default chain.
    // Absent entirely → empty selector (all defaults).
    let size_band_pools = parsed
        .get("size_band_pools")
        .and_then(|v| v.as_object())
        .map(
            |obj| kiseki_composition::namespace::NamespaceSizeBandPools {
                inline: obj
                    .get("inline")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                replicated: obj
                    .get("replicated")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                ec: obj.get("ec").and_then(|v| v.as_str()).map(str::to_owned),
            },
        )
        .unwrap_or_default();

    // Idempotent re-invocation: if the namespace already exists, echo
    // the current shard count back with 409 so the caller can no-op
    // without parsing an error string.
    if let Some(existing) = cluster_control.namespace(&namespace_id).await {
        let n = existing.shards.len();
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": "namespace already exists",
                "namespace_id": namespace_id,
                "existing_shard_count": n,
            })),
        );
    }

    // Active node list from the raft_peers field (always populated on
    // a multi-node cluster). Single-node deployments degrade to
    // [NodeId(self)] so the apply hook still fires.
    let mut active_nodes: Vec<NodeId> = state
        .node_info
        .raft_peers
        .iter()
        .map(|(id, _)| NodeId(*id))
        .collect();
    if active_nodes.is_empty() {
        active_nodes.push(NodeId(state.node_info.node_id));
    }

    let config = ShardTopologyConfig::default();
    let shard_count = match requested_shards {
        Some(n) if n > 0 => match u32::try_from(n) {
            Ok(v) => v,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": "shards exceeds u32::MAX",
                    })),
                );
            }
        },
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": "shards must be a positive u32",
                })),
            );
        }
        None => compute_initial_shards(
            &config,
            u32::try_from(active_nodes.len()).unwrap_or(u32::MAX),
        ),
    };

    let ranges = compute_shard_ranges(shard_count, &active_nodes);
    let shards: Vec<crate::cluster_control::commands::ShardRecord> = ranges
        .iter()
        .map(|r| crate::cluster_control::commands::ShardRecord {
            shard_id: r.shard_id,
            range_start: r.range_start,
            range_end: r.range_end,
            leader_node: r.leader_node,
        })
        .collect();

    let cmd = crate::cluster_control::ControlCommand::CreateNamespace {
        namespace_id: namespace_id.clone(),
        tenant_id,
        shards: shards.clone(),
    };

    match cluster_control_store.submit(cmd).await {
        Ok(resp) => {
            let n = match resp {
                crate::cluster_control::ControlResponse::NamespaceCreated { shard_count } => {
                    shard_count
                }
                _ => shard_count,
            };

            // Issue #93: the control-plane submit creates per-shard
            // Raft groups + populates the NamespaceShardMap (via the
            // apply hooks), but does NOT register the namespace in
            // the gateway's composition store or emit a
            // NamespaceCreate delta. Without these two steps, any
            // gateway write that addresses the namespace by ID (the
            // native protocol path, FUSE/NFS writes, anything that
            // bypasses the S3 `ensure_namespace_exists` fallback)
            // returns "namespace not found".
            //
            // Mirror the steps the S3 first-touch path takes in
            // `mem_gateway::ensure_namespace_exists`:
            //   1. Add the namespace to the local composition store
            //      (idempotent — skip if already present).
            //   2. Emit a `NamespaceCreate` delta on the bootstrap
            //      shard so followers' hydrators register the
            //      namespace on their composition stores.
            //
            // namespace_id at this layer is a String; the composition
            // store is keyed by `NamespaceId(Uuid)`. The CLI + bench
            // pass UUID strings; legacy non-UUID names just skip this
            // step (the older behavior — admin RPC stays usable for
            // non-UUID namespace identifiers without surfacing 5xx).
            if let (Some(comps), Some(log)) =
                (state.compositions.as_ref(), state.log_store.as_ref())
            {
                if let Ok(ns_uuid) = uuid::Uuid::parse_str(&namespace_id) {
                    let ns_id = kiseki_common::ids::NamespaceId(ns_uuid);
                    if comps.namespace(ns_id).is_none() {
                        let shard_id = kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1));
                        let ns = kiseki_composition::namespace::Namespace {
                            id: ns_id,
                            tenant_id,
                            shard_id,
                            read_only: false,
                            versioning_enabled: false,
                            compliance_tags: Vec::new(),
                            tier_policy: tier_policy.clone(),
                            size_band_pools: size_band_pools.clone(),
                        };
                        comps.add_namespace(ns.clone());
                        if let Err(e) = kiseki_composition::log_bridge::emit_namespace_create(
                            log.as_ref(),
                            shard_id,
                            tenant_id,
                            &ns,
                        )
                        .await
                        {
                            // Roll back the local add — without
                            // follower visibility this would be a
                            // stealth single-node namespace. Same
                            // contract as `ensure_namespace_exists`.
                            comps.remove_namespace(ns_id);
                            tracing::warn!(
                                namespace_id = %ns_uuid,
                                error = %e,
                                "admin namespace-create: NamespaceCreate delta emit failed — rolled back",
                            );
                        }
                    }
                }
            }

            // GH #99/#101: the control-plane apply hook registers each
            // new shard's per-shard Raft group on every node, and each
            // shard's assigned `leader_node` initializes its membership
            // (`ShardStoreApplyHook::on_create_namespace`) so leadership
            // distributes across nodes. That initialization is
            // asynchronous, so before returning 201 we wait until every
            // shard has observed a leader — otherwise a client that
            // writes immediately after create races the per-shard
            // election and 5xx's with "leader unavailable". `submit` is
            // leader-only, so this node hosts a (follower) replica of
            // every shard and `shard_health` sees each leader once the
            // assigned leader's `AppendEntries` arrives. 30 s upper
            // bound matches `ControlPlaneProvisioner::provision`.
            if let Some(log) = state.log_store.as_ref() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                for s in &shards {
                    while std::time::Instant::now() < deadline {
                        if let Ok(info) = log.shard_health(s.shard_id).await {
                            if info.leader.is_some() {
                                break;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }

            let shard_json: Vec<serde_json::Value> = shards
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "shard_id": s.shard_id.0.to_string(),
                        "range_start": encode_range_hex(&s.range_start),
                        "range_end": encode_range_hex(&s.range_end),
                        "leader_node": s.leader_node.0,
                    })
                })
                .collect();
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({
                    "namespace_id": namespace_id,
                    "tenant_id": tenant_id.0.to_string(),
                    "shard_count": n,
                    "shards": shard_json,
                })),
            )
        }
        Err(e) => {
            let msg = e.to_string();
            // openraft returns `forward request to: NodeId(X)` when
            // this node isn't the leader. Translate so callers know
            // to retry against the leader.
            let code = if msg.contains("forward request to") {
                StatusCode::MISDIRECTED_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                code,
                axum::Json(serde_json::json!({
                    "error": msg,
                })),
            )
        }
    }
}

/// Encode a 32-byte hashed-key bound as a `0x`-prefixed lowercase
/// hex string. Mirrors `api::encode_hex_prefixed` (kept inline here
/// because that helper is private to api.rs and the contract — wire
/// shape per ADR-008 rev 2 — must stay aligned across both routes).
fn encode_range_hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + 64);
    s.push_str("0x");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// ADR-024 2026-05-31 amendment §"three-tier durability" —
/// `POST /admin/topology/namespaces/{namespace_id}/size-band-pools`
///
/// Body JSON shape (any subset of fields):
/// ```json
/// { "inline": "<pool>", "replicated": "<pool>", "ec": "<pool>" }
/// ```
/// An empty-string value clears that band back to the cluster default.
/// A missing field leaves that band's selector unchanged. Effects:
/// 1. Update the namespace record in the leader's `CompositionStore`.
/// 2. Re-emit a `NamespaceCreate` delta on the namespace's shard so
///    followers' hydrators replace their in-memory copy (the hydrator
///    treats a `NamespaceCreate` for a known id as an upsert, see
///    `hydrator.rs::namespace_inserts`).
///
/// Idempotent: posting the same body twice is a no-op. 404 if the
/// namespace is unknown; 503 if the composition store isn't wired
/// (degenerate single-node test setup).
async fn api_set_namespace_size_band_pools(
    State(state): State<UiState>,
    axum::extract::Path(namespace_id): axum::extract::Path<String>,
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    let (Some(comps), Some(log)) = (state.compositions.as_ref(), state.log_store.as_ref()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "composition store / log not wired",
            })),
        );
    };
    let Ok(ns_uuid) = uuid::Uuid::parse_str(&namespace_id) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "namespace_id must be a UUID",
            })),
        );
    };
    let ns_id = kiseki_common::ids::NamespaceId(ns_uuid);
    let Some(mut ns) = comps.namespace(ns_id) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "namespace not registered on this node",
            })),
        );
    };

    let parsed: serde_json::Value = if body.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!("invalid JSON body: {e}"),
                    })),
                );
            }
        }
    };

    // Per-band update: present + non-empty → set, present + empty → clear,
    // absent → leave alone. The CLI maps the `default` sentinel to empty.
    let apply = |slot: &mut Option<String>, key: &str| {
        if let Some(v) = parsed.get(key) {
            if let Some(s) = v.as_str() {
                if s.is_empty() {
                    *slot = None;
                } else {
                    *slot = Some(s.to_owned());
                }
            }
        }
    };
    apply(&mut ns.size_band_pools.inline, "inline");
    apply(&mut ns.size_band_pools.replicated, "replicated");
    apply(&mut ns.size_band_pools.ec, "ec");

    comps.add_namespace(ns.clone());
    if let Err(e) = kiseki_composition::log_bridge::emit_namespace_create(
        log.as_ref(),
        ns.shard_id,
        ns.tenant_id,
        &ns,
    )
    .await
    {
        // Roll back so leader + followers don't diverge. The caller
        // can retry; we surface the underlying error verbatim.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": format!("failed to replicate size_band_pools update: {e}"),
            })),
        );
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "namespace_id": namespace_id,
            "size_band_pools": {
                "inline": ns.size_band_pools.inline,
                "replicated": ns.size_band_pools.replicated,
                "ec": ns.size_band_pools.ec,
            }
        })),
    )
}

/// ADR-045 §D3 + ADR-024 amendment cross-ref —
/// `POST /admin/topology/namespaces/{namespace_id}/tier-policy`
///
/// Body: `{ "tier_policy": [ { "tier": "...", "quota_bytes": N } ... ] }`.
/// An empty array clears the policy back to "fastest-fit". Same
/// replication path as `api_set_namespace_size_band_pools`.
async fn api_set_namespace_tier_policy(
    State(state): State<UiState>,
    axum::extract::Path(namespace_id): axum::extract::Path<String>,
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    let (Some(comps), Some(log)) = (state.compositions.as_ref(), state.log_store.as_ref()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "composition store / log not wired",
            })),
        );
    };
    let Ok(ns_uuid) = uuid::Uuid::parse_str(&namespace_id) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "namespace_id must be a UUID",
            })),
        );
    };
    let ns_id = kiseki_common::ids::NamespaceId(ns_uuid);
    let Some(mut ns) = comps.namespace(ns_id) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "namespace not registered on this node",
            })),
        );
    };

    let parsed: serde_json::Value = if body.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!("invalid JSON body: {e}"),
                    })),
                );
            }
        }
    };

    let tier_policy: Vec<kiseki_composition::namespace::TierQuota> = parsed
        .get("tier_policy")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let tier = e.get("tier")?.as_str()?.to_owned();
                    let quota_bytes = e
                        .get("quota_bytes")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    Some(kiseki_composition::namespace::TierQuota { tier, quota_bytes })
                })
                .collect()
        })
        .unwrap_or_default();

    ns.tier_policy = tier_policy;
    comps.add_namespace(ns.clone());
    if let Err(e) = kiseki_composition::log_bridge::emit_namespace_create(
        log.as_ref(),
        ns.shard_id,
        ns.tenant_id,
        &ns,
    )
    .await
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": format!("failed to replicate tier_policy update: {e}"),
            })),
        );
    }
    let tiers_json: Vec<_> = ns
        .tier_policy
        .iter()
        .map(|t| {
            serde_json::json!({
                "tier": t.tier,
                "quota_bytes": t.quota_bytes,
            })
        })
        .collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "namespace_id": namespace_id,
            "tier_policy": tiers_json,
        })),
    )
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
        .unwrap_or(true);

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
        .unwrap_or(true);

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

/// Parse a JSON body into a `Value` (empty body → empty object).
fn parse_json_body(
    body: &str,
) -> Result<serde_json::Value, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    if body.trim().is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(body).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": format!("invalid JSON body: {e}") })),
        )
    })
}

/// `GET /admin/storage/devices?pool=<name>` — `StorageAdminService.ListDevices`.
async fn api_list_devices(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    let pool_name = q.get("pool").cloned().unwrap_or_default();
    match client
        .list_devices(kiseki_proto::v1::ListDevicesRequest { pool_name })
        .await
    {
        Ok(resp) => {
            let devices: Vec<serde_json::Value> = resp
                .into_inner()
                .devices
                .into_iter()
                .map(|d| {
                    serde_json::json!({
                        "device_id": d.device_id,
                        "pool_name": d.pool_name,
                        "device_class": d.device_class,
                        "capacity_bytes": d.capacity_bytes,
                        "used_bytes": d.used_bytes,
                        "online": d.online,
                        "evacuating": d.evacuating,
                        "evacuation_pct": d.evacuation_pct,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "devices": devices })),
            )
        }
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `POST /admin/storage/devices/add` — `StorageAdminService.AddDevice`.
/// Body: `{"pool_name","device_id","capacity_bytes"?,"device_class"?}`.
async fn api_add_device(body: String) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pool_name = parsed
        .get("pool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let device_id = parsed
        .get("device_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if pool_name.is_empty() || device_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "pool_name and device_id are required" })),
        );
    }
    let capacity_bytes = parsed
        .get("capacity_bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let device_class = parsed
        .get("device_class")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .add_device(kiseki_proto::v1::AddDeviceRequest {
            pool_name: pool_name.clone(),
            device_id: device_id.clone(),
            capacity_bytes,
            device_class,
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "pool_name": pool_name,
                "device_id": device_id,
                "committed_at_log_index": resp.into_inner().committed_at_log_index,
            })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `POST /admin/storage/devices/remove` — `StorageAdminService.RemoveDevice`.
async fn api_remove_device(
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let device_id = parsed
        .get("device_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if device_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "device_id is required" })),
        );
    }
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .remove_device(kiseki_proto::v1::RemoveDeviceRequest {
            device_id: device_id.clone(),
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "device_id": device_id,
                "committed_at_log_index": resp.into_inner().committed_at_log_index,
            })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `POST /admin/storage/devices/evacuate` — `StorageAdminService.EvacuateDevice`.
async fn api_evacuate_device(
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let device_id = parsed
        .get("device_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if device_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "device_id is required" })),
        );
    }
    let throughput_mb_s = parsed
        .get("throughput_mb_s")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .evacuate_device(kiseki_proto::v1::EvacuateDeviceRequest {
            device_id: device_id.clone(),
            throughput_mb_s,
        })
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "device_id": device_id, "evacuating": true })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `GET /admin/storage/pools` — `StorageAdminService.ListPools`. ADR-024
/// amendment surface; returns a JSON list with each pool's role,
/// durability, device class, capacity, and the size-band thresholds.
async fn api_list_pools_admin() -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .list_pools(kiseki_proto::v1::ListPoolsRequest {})
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "pools": resp.into_inner().pools.iter().map(pool_info_to_json).collect::<Vec<_>>(),
            })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `GET /admin/storage/pools/{pool}` — `StorageAdminService.GetPool`.
async fn api_describe_pool(
    axum::extract::Path(pool): axum::extract::Path<String>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .get_pool(kiseki_proto::v1::GetPoolRequest {
            pool_name: pool.clone(),
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            axum::Json(pool_info_to_json(&resp.into_inner())),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `POST /admin/storage/pools/create` — `StorageAdminService.CreatePool`.
/// Body: `{"pool_name", "role", "device_class", "durability_kind",
/// "replication_copies"?, "ec_data_shards"?, "ec_parity_shards"?,
/// "initial_capacity_bytes"?, "inline_threshold_bytes"?,
/// "replication_ceiling_bytes"?}`.
async fn api_create_pool(body: String) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pool_name = parsed
        .get("pool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if pool_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "pool_name is required" })),
        );
    }
    let role = parsed
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let device_class = parsed
        .get("device_class")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let durability_kind = parsed
        .get("durability_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if durability_kind.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "durability_kind is required (replication|erasure_coding|inline)",
            })),
        );
    }
    let json_u32 = |k: &str| -> u32 {
        parsed
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let json_u64 = |k: &str| -> u64 {
        parsed
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .create_pool(kiseki_proto::v1::CreatePoolRequest {
            pool_name: pool_name.clone(),
            device_class,
            durability_kind,
            replication_copies: json_u32("replication_copies"),
            ec_data_shards: json_u32("ec_data_shards"),
            ec_parity_shards: json_u32("ec_parity_shards"),
            initial_capacity_bytes: json_u64("initial_capacity_bytes"),
            role,
            inline_threshold_bytes: json_u64("inline_threshold_bytes"),
            replication_ceiling_bytes: json_u64("replication_ceiling_bytes"),
            requires_migration: parsed
                .get("requires_migration")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "pool_name": pool_name,
                "committed_at_log_index": resp.into_inner().committed_at_log_index,
            })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// Convert a `PoolInfo` proto into a JSON object surfaceable by the
/// admin CLI's `pool list` / `pool describe`.
fn pool_info_to_json(info: &kiseki_proto::v1::PoolInfo) -> serde_json::Value {
    serde_json::json!({
        "name": info.pool_name,
        "role": info.role,
        "durability_kind": info.durability_kind,
        "replication_copies": info.replication_copies,
        "ec_data_shards": info.ec_data_shards,
        "ec_parity_shards": info.ec_parity_shards,
        "capacity_bytes": info.capacity_bytes,
        "used_bytes": info.used_bytes,
        "device_count": info.device_count,
        "warning_threshold_pct": info.warning_threshold_pct,
        "critical_threshold_pct": info.critical_threshold_pct,
        "readonly_threshold_pct": info.readonly_threshold_pct,
        "target_fill_pct": info.target_fill_pct,
        "inline_threshold_bytes": info.inline_threshold_bytes,
        "replication_ceiling_bytes": info.replication_ceiling_bytes,
    })
}

/// `POST /admin/storage/pools/rebalance` — `StorageAdminService.RebalancePool`.
async fn api_rebalance_pool(
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pool_name = parsed
        .get("pool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if pool_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "pool_name is required" })),
        );
    }
    let throughput_mb_s = parsed
        .get("throughput_mb_s")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .rebalance_pool(kiseki_proto::v1::RebalancePoolRequest {
            pool_name: pool_name.clone(),
            throughput_mb_s,
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "pool_name": pool_name,
                "rebalance_id": resp.into_inner().rebalance_id,
            })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// `POST /admin/storage/pools/thresholds` — `StorageAdminService.SetPoolThresholds`.
async fn api_set_pool_thresholds(
    body: String,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use axum::http::StatusCode;
    use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let pool_name = parsed
        .get("pool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if pool_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "pool_name is required" })),
        );
    }
    let u32f = |k: &str| -> u32 {
        u32::try_from(
            parsed
                .get(k)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(0)
    };
    let channel = match dial_local_storage_admin().await {
        Ok(ch) => ch,
        Err(err) => return err,
    };
    let mut client = StorageAdminServiceClient::new(channel);
    match client
        .set_pool_thresholds(kiseki_proto::v1::SetPoolThresholdsRequest {
            pool_name: pool_name.clone(),
            warning_threshold_pct: u32f("warning_pct"),
            critical_threshold_pct: u32f("critical_pct"),
            readonly_threshold_pct: u32f("readonly_pct"),
            target_fill_pct: u32f("target_fill_pct"),
        })
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "pool_name": pool_name, "updated": true })),
        ),
        Err(status) => (
            grpc_status_to_http(&status),
            axum::Json(serde_json::json!({ "error": status.message() })),
        ),
    }
}

/// ADR-030 2026-05-31 amendment §"admin-driven metadata device role" —
/// `GET /admin/storage/cluster-capacity`
///
/// Aggregates per-node metadata-capacity gauges
/// (`kiseki_node_metadata_capacity_bytes{kind=...}`) across every
/// healthy peer the metrics aggregator has scraped. Returns a JSON
/// payload with:
///   - `nodes[]`: per-node breakdown (node_id, total/used/soft/hard,
///     media_type, per-node `max_files_estimate`).
///   - `aggregate`: cluster sums + the headline
///     `cluster_max_files_estimate`
///     (= `Σ soft_limit / PER_FILE_METADATA_FOOTPRINT_BYTES`,
///     `docs/performance/capacity-planning.md`).
async fn api_cluster_metadata_capacity(
    State(state): State<UiState>,
) -> axum::Json<serde_json::Value> {
    let snapshots = state.aggregator.all_snapshots().await;

    // Per-node rows. We accept whatever the aggregator has cached;
    // unreachable peers degrade to `healthy = false` with zeros.
    let mut nodes_json = Vec::with_capacity(snapshots.len());
    let mut sum_total: u64 = 0;
    let mut sum_used: u64 = 0;
    let mut sum_soft: u64 = 0;
    let mut sum_hard: u64 = 0;
    let mut sum_budget: u64 = 0;
    let mut healthy_nodes: u64 = 0;
    let footprint = crate::system_disk::PER_FILE_METADATA_FOOTPRINT_BYTES;
    for s in &snapshots {
        let cap_rows = parse_gauge_with_labels(
            &s.metrics_text,
            "kiseki_node_metadata_capacity_bytes",
            &["kind"],
        );
        let mut total = 0u64;
        let mut used = 0u64;
        let mut soft = 0u64;
        let mut hard = 0u64;
        let mut budget = 0u64;
        for row in &cap_rows {
            let kind = row.labels.get("kind").map(String::as_str).unwrap_or("");
            // Saturating cast: prom samples are f64; any value beyond
            // u64::MAX clamps rather than overflowing into nonsense.
            let v = row.value.max(0.0) as u64;
            match kind {
                "total" => total = v,
                "used" => used = v,
                "soft_limit" => soft = v,
                "hard_limit" => hard = v,
                "small_file_budget" => budget = v,
                _ => {}
            }
        }
        let media_rows = parse_gauge_with_labels(
            &s.metrics_text,
            "kiseki_node_metadata_media_type",
            &["kind"],
        );
        let media_type = media_rows
            .iter()
            .find(|r| (r.value - 1.0).abs() < f64::EPSILON)
            .and_then(|r| r.labels.get("kind").cloned())
            .unwrap_or_else(|| "unknown".to_owned());

        let node_max_files = soft.saturating_div(footprint.max(1));
        let used_pct = if total > 0 {
            (used as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let breach = if hard > 0 && used >= hard {
            "hard"
        } else if soft > 0 && used >= soft {
            "soft"
        } else {
            "ok"
        };
        if s.healthy {
            healthy_nodes += 1;
            sum_total = sum_total.saturating_add(total);
            sum_used = sum_used.saturating_add(used);
            sum_soft = sum_soft.saturating_add(soft);
            sum_hard = sum_hard.saturating_add(hard);
            sum_budget = sum_budget.saturating_add(budget);
        }
        nodes_json.push(serde_json::json!({
            "node_id": s.node_id,
            "address": s.address,
            "healthy": s.healthy,
            "media_type": media_type,
            "total_bytes": total,
            "used_bytes": used,
            "soft_limit_bytes": soft,
            "hard_limit_bytes": hard,
            "small_file_budget_bytes": budget,
            "used_pct": format!("{used_pct:.1}"),
            "max_files_estimate": node_max_files,
            "breach": breach,
        }));
    }
    let cluster_max_files = sum_soft.saturating_div(footprint.max(1));
    let aggregate = serde_json::json!({
        "healthy_nodes": healthy_nodes,
        "total_nodes": snapshots.len(),
        "total_bytes": sum_total,
        "used_bytes": sum_used,
        "soft_limit_bytes": sum_soft,
        "hard_limit_bytes": sum_hard,
        "small_file_budget_bytes": sum_budget,
        "cluster_max_files_estimate": cluster_max_files,
        "per_file_metadata_footprint_bytes": footprint,
    });
    axum::Json(serde_json::json!({
        "nodes": nodes_json,
        "aggregate": aggregate,
    }))
}

/// ADR-048 §"Backpressure" — surface the per-pool compactor backlog
/// gauges so operators can see whether the slab-EC migrator is
/// keeping up. Returns the parsed
/// `kiseki_compactor_backlog_seconds{pool=...}` rows from the local
/// node's `/metrics`.
async fn api_compactor_backlog(State(state): State<UiState>) -> axum::Json<serde_json::Value> {
    let metrics_text = (state.metrics_encode)();
    let rows =
        parse_gauge_with_labels(&metrics_text, "kiseki_compactor_backlog_seconds", &["pool"]);
    let pools: Vec<_> = rows
        .iter()
        .map(|r| {
            let pool = r
                .labels
                .get("pool")
                .cloned()
                .unwrap_or_else(|| "default".into());
            let age_s = r.value.max(0.0) as i64;
            serde_json::json!({
                "pool": pool,
                "backlog_seconds": age_s,
                // ADR-048 §"Backpressure" defaults — 30 s soft, 60 s hard.
                "soft_breach": age_s >= 30,
                "hard_breach": age_s >= 60,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "pools": pools,
    }))
}

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

    // D2: per-device gauges with labels {pool, device_id, kind}.
    // Operators read this to spot one struggling device inside a
    // healthy pool. Empty when no device-level metric has been
    // populated yet (single-node bring-up, in-memory backend).
    let device_rows = build_per_device_rows(&metrics_text);
    let devices: Vec<_> = device_rows
        .iter()
        .map(|d| {
            let used_pct = if d.total_bytes > 0 {
                (d.used_bytes as f64 / d.total_bytes as f64) * 100.0
            } else {
                0.0
            };
            serde_json::json!({
                "pool": d.pool,
                "device_id": d.device_id,
                "total_bytes": d.total_bytes,
                "used_bytes": d.used_bytes,
                "free_bytes": d.free_bytes,
                "used_pct": format!("{used_pct:.1}"),
                "read_errors": d.read_errors,
                "write_errors": d.write_errors,
            })
        })
        .collect();

    axum::Json(serde_json::json!({
        "pools": pools,
        "devices": devices,
    }))
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

    // D2 — per-device subtable. Reuses the same metrics scrape.
    let devices = build_per_device_rows(&metrics_text);
    let _ = write!(
        html,
        "<h4 style=\"margin-top:16px\">Devices</h4><table><thead><tr><th>Pool</th><th>Device</th><th>Used</th><th>Total</th><th>Free</th><th>Used %</th><th>R/W Errors</th></tr></thead><tbody>",
    );
    if devices.is_empty() {
        html.push_str(
            "<tr><td colspan=\"7\" style=\"color:var(--dim)\">No per-device gauges yet (FileBackedDevice publish pending)</td></tr>",
        );
    } else {
        for d in &devices {
            let used_pct = if d.total_bytes > 0 {
                (d.used_bytes as f64 / d.total_bytes as f64) * 100.0
            } else {
                0.0
            };
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1}%</td><td>{} / {}</td></tr>",
                html_escape(&d.pool),
                html_escape(&d.device_id),
                format_bytes(d.used_bytes),
                format_bytes(d.total_bytes),
                format_bytes(d.free_bytes),
                used_pct,
                d.read_errors,
                d.write_errors,
            );
        }
    }
    html.push_str("</tbody></table>");

    Html(html)
}

// ---------------------------------------------------------------------------
// D2: per-device pool gauges
// ---------------------------------------------------------------------------

/// One row in the per-device pool capacity view. Aggregates the three
/// `kind=` labels into a single struct so callers don't have to do
/// the join themselves.
#[derive(Debug, Clone)]
struct DeviceRow {
    pool: String,
    device_id: String,
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
    read_errors: u64,
    write_errors: u64,
}

/// Build per-device rows from a Prometheus exposition body.
///
/// Reads:
/// - `kiseki_pool_device_capacity_bytes{pool=,device_id=,kind=total|used|free}`
/// - `kiseki_pool_device_errors_total{device_id=,op=read|write}`
///
/// Devices missing a `kind=` sample default to 0 for that kind, and
/// `free` is derived from `total - used` when absent.
fn build_per_device_rows(metrics_text: &str) -> Vec<DeviceRow> {
    let cap = parse_gauge_with_labels(
        metrics_text,
        "kiseki_pool_device_capacity_bytes",
        &["pool", "device_id", "kind"],
    );
    let errs = parse_counter_with_labels(
        metrics_text,
        "kiseki_pool_device_errors_total",
        &["device_id", "op"],
    );
    // Aggregate by `device_id` (devices belong to exactly one pool).
    let mut acc: std::collections::BTreeMap<String, DeviceRow> = Default::default();
    for s in &cap {
        let device_id = s
            .labels
            .get("device_id")
            .cloned()
            .unwrap_or_else(|| "?".into());
        let pool = s
            .labels
            .get("pool")
            .cloned()
            .unwrap_or_else(|| "default".into());
        let kind = s.labels.get("kind").map(String::as_str).unwrap_or("total");
        let row = acc.entry(device_id.clone()).or_insert_with(|| DeviceRow {
            pool: pool.clone(),
            device_id: device_id.clone(),
            total_bytes: 0,
            used_bytes: 0,
            free_bytes: 0,
            read_errors: 0,
            write_errors: 0,
        });
        if row.pool.is_empty() {
            row.pool = pool;
        }
        let value = s.value as u64;
        match kind {
            "total" => row.total_bytes = value,
            "used" => row.used_bytes = value,
            "free" => row.free_bytes = value,
            _ => {}
        }
    }
    for s in &errs {
        let device_id = s
            .labels
            .get("device_id")
            .cloned()
            .unwrap_or_else(|| "?".into());
        let op = s.labels.get("op").map(String::as_str).unwrap_or("");
        if let Some(row) = acc.get_mut(&device_id) {
            match op {
                "read" => row.read_errors = s.value as u64,
                "write" => row.write_errors = s.value as u64,
                _ => {}
            }
        }
    }
    // Derive `free` when missing.
    for row in acc.values_mut() {
        if row.free_bytes == 0 && row.total_bytes >= row.used_bytes {
            row.free_bytes = row.total_bytes - row.used_bytes;
        }
    }
    acc.into_values().collect()
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
        // ADR-044: tenant orgs default to tenant-isolated dedup (secret
        // per-tenant HMAC chunk IDs). `CrossTenant` is reserved for
        // explicitly non-sensitive / system data.
        dedup_policy: kiseki_common::tenancy::DedupPolicy::TenantIsolated,
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

/// D3: nested tenant CRUD — create a project under an existing org.
///
/// Body: `{"org_id": "...", "name": "..."}`.
/// Returns `201 {"project_id": "..."}` on success, `404` when the
/// parent org is missing, `409` on duplicate name, `503` when the
/// tenant store is not wired on this node.
#[derive(serde::Deserialize)]
struct CreateProjectBody {
    org_id: String,
    name: String,
}

async fn api_create_project(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<CreateProjectBody>,
) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "tenant store not wired on this node"})),
        );
    };
    if store.get_org(&body.org_id).is_err() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": format!("org {} not found", body.org_id),
            })),
        );
    }
    let project_id = uuid::Uuid::new_v4().to_string();
    let proj = kiseki_control::tenant::Project {
        id: project_id.clone(),
        org_id: body.org_id,
        name: body.name,
        compliance_tags: vec![],
        quota: kiseki_common::tenancy::Quota {
            capacity_bytes: 0,
            iops: 0,
            metadata_ops_per_sec: 0,
        },
    };
    match store.create_project(proj) {
        Ok(()) => (
            axum::http::StatusCode::CREATED,
            axum::Json(serde_json::json!({"project_id": project_id})),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[derive(serde::Deserialize)]
struct CreateWorkloadBody {
    project_id: String,
    name: String,
}

async fn api_create_workload(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<CreateWorkloadBody>,
) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "tenant store not wired on this node"})),
        );
    };
    // Resolve org_id via the project lookup so the workload row is
    // fully populated (TenantStore::create_workload requires it).
    let Ok(project) = store.get_project(&body.project_id) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": format!("project {} not found", body.project_id),
            })),
        );
    };
    let workload_id = uuid::Uuid::new_v4().to_string();
    let wl = kiseki_control::tenant::Workload {
        id: workload_id.clone(),
        org_id: project.org_id,
        project_id: body.project_id,
        name: body.name,
        quota: kiseki_common::tenancy::Quota {
            capacity_bytes: 0,
            iops: 0,
            metadata_ops_per_sec: 0,
        },
    };
    match store.create_workload(wl) {
        Ok(()) => (
            axum::http::StatusCode::CREATED,
            axum::Json(serde_json::json!({"workload_id": workload_id})),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[derive(serde::Deserialize)]
struct CreateNamespaceBody {
    workload_id: String,
    name: String,
}

async fn api_create_namespace(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<CreateNamespaceBody>,
) -> impl IntoResponse {
    let Some(tenants) = state.tenants.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "tenant store not wired"})),
        );
    };
    let Some(namespaces) = state.namespaces.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "namespace store not wired"})),
        );
    };
    let Ok(workload) = tenants.get_workload(&body.workload_id) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": format!("workload {} not found", body.workload_id),
            })),
        );
    };
    let namespace_id = uuid::Uuid::new_v4().to_string();
    let _ = body.name; // namespace store does not persist a human name today
    let ns = kiseki_control::namespace::Namespace {
        id: namespace_id.clone(),
        org_id: workload.org_id,
        project_id: workload.project_id,
        shard_id: String::new(),
        compliance_tags: vec![],
        read_only: false,
    };
    match namespaces.create(ns) {
        Ok(()) => (
            axum::http::StatusCode::CREATED,
            axum::Json(serde_json::json!({"namespace_id": namespace_id})),
        ),
        Err(e) => (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[derive(serde::Deserialize)]
struct TenantDescribeQuery {
    id: String,
}

async fn api_tenant_describe(
    State(state): State<UiState>,
    Query(params): Query<TenantDescribeQuery>,
) -> impl IntoResponse {
    let Some(store) = state.tenants.as_ref() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "tenant store not wired"})),
        );
    };
    // Auto-detect type by looking it up in each store in order: org,
    // project, workload, namespace.
    if let Ok(o) = store.get_org(&params.id) {
        return (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "kind": "organization",
                "id": o.id,
                "name": o.name,
                "quota": {
                    "capacity_bytes": o.quota.capacity_bytes,
                    "iops": o.quota.iops,
                    "metadata_ops_per_sec": o.quota.metadata_ops_per_sec,
                },
                "compression_enabled": o.compression_enabled,
            })),
        );
    }
    if let Ok(p) = store.get_project(&params.id) {
        return (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "kind": "project",
                "id": p.id,
                "org_id": p.org_id,
                "name": p.name,
            })),
        );
    }
    if let Ok(w) = store.get_workload(&params.id) {
        return (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "kind": "workload",
                "id": w.id,
                "org_id": w.org_id,
                "project_id": w.project_id,
                "name": w.name,
            })),
        );
    }
    if let Some(ns_store) = state.namespaces.as_ref() {
        if let Ok(n) = ns_store.get(&params.id) {
            return (
                axum::http::StatusCode::OK,
                axum::Json(serde_json::json!({
                    "kind": "namespace",
                    "id": n.id,
                    "org_id": n.org_id,
                    "project_id": n.project_id,
                    "shard_id": n.shard_id,
                    "read_only": n.read_only,
                })),
            );
        }
    }
    (
        axum::http::StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": format!("no org / project / workload / namespace with id {}", params.id),
        })),
    )
}

#[derive(serde::Deserialize)]
struct TenantDeleteBody {
    id: String,
}

async fn api_tenant_delete(
    State(_state): State<UiState>,
    axum::Json(body): axum::Json<TenantDeleteBody>,
) -> impl IntoResponse {
    // TenantStore / NamespaceStore don't expose `delete` today (the
    // gRPC ControlService is the source of truth for tenant lifecycle).
    // Surface a stable error contract so the CLI prints a clear
    // message; the gRPC path remains canonical until the HTTP delete
    // verb lands. Tracked in the 2026-05-15 follow-ups doc — this
    // endpoint exists so operators get a typed error instead of a
    // 404 on the route.
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        axum::Json(serde_json::json!({
            "status": "error",
            "message": format!(
                "tenant delete for id {} is not exposed over HTTP yet; \
                 use the gRPC ControlService (see docs/api/grpc.md)",
                body.id,
            ),
        })),
    )
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
    /// D5: when true, return only the local node's audit shard.
    /// Default (false) fans out to all peers and merges results
    /// keyed by (tenant_id, sequence).
    #[serde(default)]
    local_only: bool,
    /// Internal — set by the coordinator on per-peer fan-out so the
    /// peer skips its own fan-out (avoids infinite loops). Operators
    /// should never set this directly.
    #[serde(default)]
    no_fanout: bool,
}

#[allow(clippy::too_many_lines)] // D5 fan-out logic is naturally long
async fn api_audit_query(
    State(state): State<UiState>,
    Query(params): Query<AuditQueryParams>,
) -> impl IntoResponse {
    let Some(audit) = state.audit.as_ref() else {
        return axum::Json(serde_json::json!({"events": [], "available": false}));
    };
    let tenant_id = parse_tenant(&params.tenant);
    let event_type = parse_event_type(params.event_type.as_deref());
    let limit = params.limit.unwrap_or(200);
    let query = AuditQuery {
        tenant_id,
        from: kiseki_common::ids::SequenceNumber(params.from.unwrap_or(1)),
        limit,
        event_type,
    };

    // Local query first — always.
    let local_events = audit.query(&query);
    let total = audit.total_events();

    // D5: fan out to peers when not explicitly local-only and not in
    // a per-peer recursion. Each peer record is keyed by `(node_id,
    // tenant_id, sequence)` for dedup; the global view sorts by
    // sequence and truncates to `limit`.
    let do_fanout = !params.local_only && !params.no_fanout;
    let aggregated_local = local_events
        .iter()
        .map(|e| audit_event_to_json(e, state.node_info.node_id))
        .collect::<Vec<_>>();

    if !do_fanout {
        return axum::Json(serde_json::json!({
            "events": aggregated_local,
            "total_events": total,
            "available": true,
            "aggregated": false,
            "node_id": state.node_info.node_id,
        }));
    }

    // Build the peer fan-out target list. Skip self (matched by
    // node_id) and any peer with no resolvable metrics address.
    let metrics_port = state
        .node_info
        .metrics_addr
        .split(':')
        .next_back()
        .unwrap_or("9090")
        .to_owned();
    let self_node_id = state.node_info.node_id;
    let peers: Vec<(u64, String)> = state
        .node_info
        .raft_peers
        .iter()
        .filter(|(id, _)| *id != self_node_id)
        .map(|(id, addr)| {
            let host = addr.split(':').next().unwrap_or("127.0.0.1");
            (*id, format!("{host}:{metrics_port}"))
        })
        .collect();

    // Compose the per-peer query string. Cap fan-out by `since`
    // window + `limit` to bound the byte budget; per-peer responses
    // can be large, so we deliberately re-use the same limit budget
    // per peer (the coordinator then truncates the merged set).
    let mut qs = Vec::new();
    if let Some(t) = &params.tenant {
        qs.push(format!("tenant={}", urlencode(t)));
    }
    if let Some(t) = &params.event_type {
        qs.push(format!("event_type={}", urlencode(t)));
    }
    qs.push(format!("limit={limit}"));
    qs.push(format!("from={}", params.from.unwrap_or(1)));
    // Important: tell the peer NOT to fan out further.
    qs.push("no_fanout=true".into());
    let qs_joined = qs.join("&");

    let mut merged: Vec<serde_json::Value> = aggregated_local;
    let mut unreachable: Vec<u64> = Vec::new();
    let mut reachable: Vec<u64> = vec![self_node_id];

    for (peer_id, host_port) in &peers {
        match fetch_peer_audit(host_port, &qs_joined).await {
            Ok(peer_events) => {
                reachable.push(*peer_id);
                for mut ev in peer_events {
                    // Tag with the originating node_id; the dedup key
                    // is (node_id, tenant_id, sequence) — without a
                    // node tag, two nodes may have overlapping local
                    // sequence numbers for the system shard.
                    if let Some(obj) = ev.as_object_mut() {
                        obj.entry("node_id").or_insert(serde_json::json!(peer_id));
                    }
                    merged.push(ev);
                }
            }
            Err(_) => unreachable.push(*peer_id),
        }
    }

    // Dedup by (node_id, tenant_id, sequence). Audit records carry
    // monotonic sequence per shard, so a single (node, tenant,
    // sequence) triple is the canonical id.
    let mut seen: std::collections::HashSet<(u64, String, u64)> = std::collections::HashSet::new();
    merged.retain(|ev| {
        let nid = ev
            .get("node_id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let tid = ev
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let seq = ev
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        seen.insert((nid, tid, seq))
    });

    // Sort newest-first by sequence; tie-break by node_id for
    // determinism. Then truncate to `limit` after the merge.
    merged.sort_by(|a, b| {
        let aseq = a
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let bseq = b
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        bseq.cmp(&aseq).then_with(|| {
            let an = a
                .get("node_id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let bn = b
                .get("node_id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            an.cmp(&bn)
        })
    });
    merged.truncate(limit);

    axum::Json(serde_json::json!({
        "events": merged,
        "total_events": total,
        "available": true,
        "aggregated": true,
        "node_id": self_node_id,
        "reachable_nodes": reachable,
        "unreachable_nodes": unreachable,
    }))
}

fn audit_event_to_json(e: &kiseki_audit::AuditEvent, node_id: u64) -> serde_json::Value {
    serde_json::json!({
        "sequence": e.sequence.0,
        "type": format!("{:?}", e.event_type),
        "tenant_id": e.tenant_id.map(|t| t.0.to_string()),
        "actor": e.actor,
        "description": e.description,
        "wall_ms": e.timestamp.wall.millis_since_epoch,
        "node_id": node_id,
    })
}

/// Best-effort %-encode for URL query values. Matches the kiseki-admin
/// CLI's own `url_encode` semantics.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Fetch a peer's `/admin/audit/query` response and return its events
/// array. Adds a short timeout so a slow peer doesn't pin the
/// coordinator's connection pool. Implemented over `tokio::net` with
/// no extra HTTP client dependency.
async fn fetch_peer_audit(host_port: &str, query: &str) -> Result<Vec<serde_json::Value>, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration};

    let path = format!("/admin/audit/query?{query}");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    let connect_fut = TcpStream::connect(host_port);
    let mut stream = timeout(Duration::from_secs(2), connect_fut)
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::with_capacity(64 * 1024);
    let read_fut = stream.read_to_end(&mut buf);
    timeout(Duration::from_secs(5), read_fut)
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or("malformed HTTP response")?;
    let header_lc = text[..body_start].to_ascii_lowercase();
    let raw_body = &text[body_start..];
    let body = if header_lc.contains("transfer-encoding: chunked") {
        decode_chunked_local(raw_body)
    } else {
        raw_body.to_string()
    };
    let resp: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode: {e}"))?;
    Ok(resp
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Minimal chunked-encoding decoder. Used only by the peer
/// audit-query fan-out path; the rest of the server uses axum's
/// hyper layer for chunked handling.
fn decode_chunked_local(input: &str) -> String {
    let mut result = String::new();
    let mut remaining = input;
    loop {
        let trimmed = remaining.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let line_end = trimmed.find("\r\n").unwrap_or(trimmed.len());
        let size = usize::from_str_radix(trimmed[..line_end].trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let data_start = line_end + 2;
        if data_start + size <= trimmed.len() {
            result.push_str(&trimmed[data_start..data_start + size]);
            remaining = &trimmed[data_start + size..];
            if remaining.starts_with("\r\n") {
                remaining = &remaining[2..];
            }
        } else {
            result.push_str(&trimmed[data_start..]);
            break;
        }
    }
    result
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

#[derive(serde::Deserialize)]
struct KeysShredBody {
    tenant_id: String,
    /// Operator-supplied reason — surfaced in the audit event for
    /// crypto-shred forensics (see
    /// `kiseki_audit::event::crypto_shred_force_override_event`).
    #[serde(default)]
    reason: Option<String>,
}

/// D4: tenant-scoped crypto-shred. IRREVERSIBLE — destroys the
/// tenant's KEK cache entry and emits a `KeyDestruction` audit
/// event. The actual KEK material lives in the tenant-side KMS
/// provider and is not retained on the gateway; clearing the
/// in-process cache + emitting the audit event is the
/// authoritative HTTP-surface action for this node.
async fn api_keys_shred(
    State(state): State<UiState>,
    axum::Json(body): axum::Json<KeysShredBody>,
) -> impl IntoResponse {
    // Parse the tenant id up front so a bad UUID returns 400 instead
    // of touching the key store / audit log.
    let Ok(uuid) = uuid::Uuid::parse_str(&body.tenant_id) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": format!("tenant_id `{}` is not a valid UUID", body.tenant_id),
            })),
        );
    };
    let tenant = kiseki_common::ids::OrgId(uuid);
    // Emit the audit event regardless of whether a key manager is
    // wired — the audit trail is the load-bearing artefact for
    // crypto-shred (ADR-014 §K11).
    if let Some(audit) = state.audit.as_ref() {
        let reason = body
            .reason
            .clone()
            .unwrap_or_else(|| "kiseki-admin keys shred".to_string());
        let event = kiseki_audit::event::crypto_shred_force_override_event(tenant, &reason);
        audit.append(event);
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "status": "ok",
            "message": format!(
                "crypto-shred audit event recorded for tenant {}; \
                 tenant-side KMS destruction is the authoritative step \
                 (see docs/admin/key-management.md)",
                tenant.0,
            ),
            "tenant_id": tenant.0.to_string(),
            "audit_recorded": state.audit.is_some(),
        })),
    )
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
// whoami (D6)
// ---------------------------------------------------------------------------

/// D6: surface the authenticated principal back to the client.
///
/// The metrics HTTP listener is plain HTTP today (operator-only,
/// firewalled to the admin VLAN). Reverse proxies (envoy, nginx)
/// typically forward the client cert's SAN in a header before
/// proxying to a backend; we honour the canonical ones so a TLS-
/// terminating proxy in front of this server still gives the client
/// a meaningful principal.
///
/// Headers consulted, in order:
/// - `x-kiseki-client-san` (kiseki convention)
/// - `x-ssl-client-san` (envoy default)
/// - `x-forwarded-client-cert` (proxy spec)
///
/// When no header is present, the response sets `san=null` and the
/// CLI prints "(no SAN) — connection is not mTLS-authenticated".
async fn api_whoami(
    State(state): State<UiState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let header_value = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let san = header_value("x-kiseki-client-san")
        .or_else(|| header_value("x-ssl-client-san"))
        .or_else(|| header_value("x-forwarded-client-cert"))
        .filter(|s| !s.is_empty());
    let tenant_id = header_value("x-kiseki-tenant-id");
    let workload_id = header_value("x-kiseki-workload-id");
    axum::Json(serde_json::json!({
        "node_id": state.node_info.node_id,
        "metrics_addr": state.node_info.metrics_addr,
        "san": san,
        "tenant_id": tenant_id,
        "workload_id": workload_id,
    }))
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
        let d1 = rows.iter().find(|r| r.device_id == "d-1").expect("d-1 row");
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
