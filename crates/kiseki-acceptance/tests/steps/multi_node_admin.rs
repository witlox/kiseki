#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Steps for `@integration @multi-node @shard-mgmt` scenarios — admin
//! gRPC SplitShard / MergeShards driven against a real spawned 3-node
//! cluster. Closes the @library→@integration fidelity gap for the
//! shard lifecycle path that the gate-2 audit flagged.
//!
//! These scenarios exercise:
//!   * `StorageAdminService.SplitShard` / `MergeShards` over the
//!     plaintext data port (no mTLS in the standard 3-node singleton)
//!   * `RaftShardStore::split_shard` / `merge_shards` against the
//!     multiplexed Raft transport (ADR-041) — the source shard's
//!     state mutations replicate via consensus to all 3 nodes
//!   * Multi-node consistency: `GetShard` for the bootstrap shard
//!     succeeds on every node both before and after the operation,
//!     so the source-side range tightening and state reset never
//!     loses the bootstrap shard from any replica.

use cucumber::{then, when};
use kiseki_proto::v1 as pb;
use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
use tonic::transport::{Channel, Endpoint};

use crate::steps::multi_node::cluster;
use crate::KisekiWorld;

/// The bootstrap shard id matches what `runtime::run_main` installs:
/// `Uuid::from_u128(1)`. Every multi-node node creates this same
/// shard at startup, so it is the canonical target for the `@shard-mgmt`
/// scenarios (no extra setup needed).
fn bootstrap_shard_id_string() -> String {
    uuid::Uuid::from_u128(1).to_string()
}

/// Build a plaintext gRPC channel to a node at `port` (where
/// StorageAdminService is mounted). The standard 3-node harness boots
/// without mTLS, so http:// / no-tls is correct here.
async fn admin_client_for_port(port: u16) -> StorageAdminServiceClient<Channel> {
    let url = format!("http://127.0.0.1:{port}");
    let endpoint = Endpoint::from_shared(url.clone()).expect("admin gRPC endpoint");
    let channel = endpoint
        .connect()
        .await
        .unwrap_or_else(|e| panic!("connect to admin gRPC at {url}: {e}"));
    StorageAdminServiceClient::new(channel)
}

/// Snapshot the (node_id, grpc_data port) pairs without holding the
/// cluster borrow across `await` points.
fn snapshot_admin_ports(w: &KisekiWorld) -> Vec<(u64, u16)> {
    let g = cluster(w);
    g.nodes().map(|n| (n.node_id, n.ports.grpc_data)).collect()
}

fn node1_admin_port(w: &KisekiWorld) -> u16 {
    cluster(w).node(1).ports.grpc_data
}

#[when("the admin calls SplitShard for the bootstrap shard via node-1 admin gRPC")]
async fn when_admin_split_bootstrap(w: &mut KisekiWorld) {
    let shard_id = bootstrap_shard_id_string();
    // node-1 is the bootstrap node and the convention initial leader;
    // SplitShard mutations run via the source's Raft group so the
    // call should land on the leader to avoid a no-leader retry.
    let port = node1_admin_port(w);
    let mut client = admin_client_for_port(port).await;
    let resp = client
        .split_shard(pb::SplitShardRequest {
            shard_id: shard_id.clone(),
            // Empty pivot_key = engine picks midpoint (the multi-node
            // RaftShardStore::split_shard always computes its own
            // midpoint from the source shard's range).
            pivot_key: String::new(),
        })
        .await;
    match resp {
        Ok(r) => {
            let inner = r.into_inner();
            w.cluster
                .name_index_state
                .insert("split_left_shard_id".into(), inner.left_shard_id.clone());
            w.cluster
                .name_index_state
                .insert("split_right_shard_id".into(), inner.right_shard_id.clone());
            w.last_error = None;
        }
        Err(s) => {
            let msg = format!(
                "SplitShard via node-1 admin gRPC failed: {:?}: {}",
                s.code(),
                s.message()
            );
            w.last_error = Some(msg.clone());
            panic!("{msg}");
        }
    }
}

#[then("the SplitShard response carries a non-empty right_shard_id distinct from the left")]
async fn then_split_response_has_right(w: &mut KisekiWorld) {
    let left = w
        .cluster
        .name_index_state
        .get("split_left_shard_id")
        .cloned()
        .expect("SplitShard step must run first");
    let right = w
        .cluster
        .name_index_state
        .get("split_right_shard_id")
        .cloned()
        .expect("SplitShard step must run first");
    assert!(
        !right.is_empty(),
        "SplitShard returned empty right_shard_id",
    );
    assert_ne!(
        left, right,
        "SplitShard left and right shard ids must differ",
    );
    // Sanity: the left should be the original bootstrap shard (the
    // proto convention is that the original keeps the lower half).
    assert_eq!(
        left,
        bootstrap_shard_id_string(),
        "SplitShard left_shard_id should be the original bootstrap shard",
    );
}

#[then("the bootstrap shard remains queryable on every node")]
async fn then_bootstrap_queryable_on_every_node(w: &mut KisekiWorld) {
    let shard_id = bootstrap_shard_id_string();
    let ports = snapshot_admin_ports(w);
    for (node_id, port) in ports {
        let mut client = admin_client_for_port(port).await;
        let resp = client
            .get_shard(pb::GetShardRequest {
                shard_id: shard_id.clone(),
            })
            .await;
        match resp {
            Ok(r) => {
                let info = r.into_inner();
                assert_eq!(
                    info.shard_id, shard_id,
                    "node-{node_id}: GetShard returned wrong shard_id",
                );
            }
            Err(s) => panic!(
                "node-{node_id}: GetShard for bootstrap shard failed: {:?}: {}",
                s.code(),
                s.message()
            ),
        }
    }
}

#[when("the admin calls MergeShards merging the right back into the left via node-1 admin gRPC")]
async fn when_admin_merge_back(w: &mut KisekiWorld) {
    let left = w
        .cluster
        .name_index_state
        .get("split_left_shard_id")
        .cloned()
        .expect("SplitShard step must run before MergeShards");
    let right = w
        .cluster
        .name_index_state
        .get("split_right_shard_id")
        .cloned()
        .expect("SplitShard step must run before MergeShards");
    let port = node1_admin_port(w);
    let mut client = admin_client_for_port(port).await;
    let resp = client
        .merge_shards(pb::MergeShardsRequest {
            left_shard_id: left.clone(),
            right_shard_id: right.clone(),
        })
        .await;
    match resp {
        Ok(r) => {
            let inner = r.into_inner();
            w.cluster
                .name_index_state
                .insert("merged_shard_id".into(), inner.merged_shard_id.clone());
            w.last_error = None;
        }
        Err(s) => {
            let msg = format!(
                "MergeShards via node-1 admin gRPC failed: {:?}: {}",
                s.code(),
                s.message()
            );
            w.last_error = Some(msg.clone());
            panic!("{msg}");
        }
    }
}

#[then("every node logged the apply hook registering the new shard locally")]
async fn then_apply_hook_fired_on_every_node(w: &mut KisekiWorld) {
    // The post-#4 control-plane apply hook hits a metric we already
    // export: each node's `kiseki_raft_transport_registry_size` jumps
    // by +1 when its multiplexed listener gets a new shard
    // registration. Bootstrap baseline is 2 (the per-node bootstrap
    // shard's group + the control-plane group); after a split the
    // gauge should read 3 on every node.
    //
    // The cluster harness is a process-level singleton reused across
    // every `@multi-node` scenario, so a node may have been killed +
    // restarted by an earlier `@leader-change` scenario. Restarted
    // nodes momentarily reset their multiplexed listener registry
    // until the per-shard groups re-register via control-plane
    // hydration. Poll the gauge with a 10 s deadline so we tolerate
    // that catch-up window — the BDD harness covers the existence-of-
    // metric path; the absolute count is timing-sensitive.
    let right = w
        .cluster
        .name_index_state
        .get("split_right_shard_id")
        .cloned()
        .expect("SplitShard step must run first");
    // Sanity — make sure we actually got a new shard id back from
    // the admin RPC (not, say, an empty string).
    assert!(!right.is_empty(), "split must produce a new shard id");

    let ports: Vec<(u64, u16)> = {
        let g = cluster(w);
        g.nodes().map(|n| (n.node_id, n.ports.metrics)).collect()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut missing: Vec<u64> = Vec::new();
    loop {
        missing.clear();
        for (node_id, metrics_port) in &ports {
            let url = format!("http://127.0.0.1:{metrics_port}/metrics");
            let value = match reqwest::get(&url).await {
                Ok(r) => r
                    .text()
                    .await
                    .map(|body| parse_registry_size(&body))
                    .unwrap_or(0.0),
                Err(_) => 0.0,
            };
            if value < 3.0 {
                missing.push(*node_id);
            }
        }
        if missing.is_empty() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!(
        "ADR-033 §4 cluster-wide split: nodes {missing:?} did not register the \
         new shard's Raft group locally within 10 s — apply hook either did \
         not fire or RaftShardStore::create_shard returned without \
         registration. Expected `kiseki_raft_transport_registry_size >= 3` on \
         every node, got <3 on {missing:?}. (Note: the cluster harness is a \
         process-level singleton; if a prior `@leader-change` scenario killed \
         a node, the restarted node's listener registry takes a few seconds \
         to re-populate via control-plane hydration — the 10 s deadline \
         accommodates that. A persistent failure means the apply hook is \
         genuinely not firing.)",
    );
}

/// Parse `kiseki_raft_transport_registry_size` from a Prometheus
/// text-format scrape. Returns `0.0` when the metric is missing —
/// the scenario assertion treats that as a failure too.
fn parse_registry_size(body: &str) -> f64 {
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("kiseki_raft_transport_registry_size") {
            // The gauge has no labels; everything after the metric
            // name is `<whitespace><value>`.
            return rest.trim().parse::<f64>().unwrap_or(0.0);
        }
    }
    0.0
}

/// GH #99: drive the multi-shard `topology namespace-create` HTTP
/// admin endpoint against the real spawned cluster, then assert every
/// fresh shard elects a real raft leader. Pre-fix the namespace-create
/// handler submits the `CreateNamespace` control command (apply hook
/// registers each shard's group on every node) but never initializes
/// per-shard membership, so the shards sit at `leader_id=null` forever
/// and the `then` step times out.
#[when(regex = r"^the admin creates a (\d+)-shard namespace via admin HTTP$")]
async fn when_admin_create_sharded_ns(w: &mut KisekiWorld, shards: u32) {
    // Snapshot (node_id, metrics_port) without holding the borrow
    // across awaits. `submit` is leader-only (openraft `client_write`
    // errors on followers → HTTP 421), so try every node until one
    // accepts the create — robust against leadership having rotated in
    // an earlier @leader-change scenario on this singleton cluster.
    let ports: Vec<(u64, u16)> = {
        let g = cluster(w);
        g.nodes().map(|n| (n.node_id, n.ports.metrics)).collect()
    };
    let namespace_id = uuid::Uuid::new_v4().to_string();
    let tenant_id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "namespace_id": namespace_id,
        "tenant_id": tenant_id,
        "shards": shards,
    });
    let http = reqwest::Client::new();

    // `submit` is leader-only and the HTTP handler does NOT forward to
    // the leader (unlike the gRPC SplitShard path), so we hit every
    // node until the control-plane leader accepts. Retry on a deadline
    // because the control-plane group's own election can still be in
    // flight right after `Given a 3-node kiseki cluster` (that step
    // only waits on the bootstrap *shard* leader).
    let mut created: Option<(u16, Vec<String>)> = None;
    let mut last_err = String::new();
    let create_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    'outer: loop {
        for (node_id, port) in &ports {
            let url = format!("http://127.0.0.1:{port}/admin/topology/namespaces");
            match http.post(&url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let json: serde_json::Value = resp.json().await.unwrap_or_default();
                    if status.is_success() {
                        let shard_ids: Vec<String> = json
                            .get("shards")
                            .and_then(|s| s.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|s| {
                                        s.get("shard_id").and_then(|v| v.as_str()).map(String::from)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        assert_eq!(
                            shard_ids.len(),
                            shards as usize,
                            "namespace-create returned {} shard ids, expected {shards}",
                            shard_ids.len(),
                        );
                        created = Some((*port, shard_ids));
                        break 'outer;
                    }
                    last_err = format!("node-{node_id} HTTP {status}: {json}");
                }
                Err(e) => last_err = format!("node-{node_id} POST failed: {e}"),
            }
        }
        if std::time::Instant::now() >= create_deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    let (leader_port, shard_ids) = created.unwrap_or_else(|| {
        panic!("namespace-create was not accepted by any node within 30s (last: {last_err})")
    });

    w.cluster
        .name_index_state
        .insert("ns99_leader_port".into(), leader_port.to_string());
    w.cluster
        .name_index_state
        .insert("ns99_shard_ids".into(), shard_ids.join(","));
    w.last_error = None;
}

#[then(
    regex = r"^every shard of that namespace elects a raft leader distributed across the cluster within (\d+)s$"
)]
async fn then_every_shard_has_leader(w: &mut KisekiWorld, secs: u64) {
    let port: u16 = w
        .cluster
        .name_index_state
        .get("ns99_leader_port")
        .and_then(|s| s.parse().ok())
        .expect("namespace-create step must run first");
    let shard_ids: Vec<String> = w
        .cluster
        .name_index_state
        .get("ns99_shard_ids")
        .map(|s| s.split(',').map(String::from).collect())
        .expect("namespace-create step must run first");

    let http = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut without_leader: Vec<String> = Vec::new();
    // shard_id -> elected leader node id (GH #101 distribution check).
    let mut leaders: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    loop {
        without_leader.clear();
        leaders.clear();
        for shard_id in &shard_ids {
            let url = format!("http://127.0.0.1:{port}/cluster/shards/{shard_id}/leader");
            let leader: Option<u64> = match http.get(&url).send().await {
                Ok(resp) => resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|j| j.get("leader_id").and_then(serde_json::Value::as_u64)),
                Err(_) => None,
            };
            match leader {
                Some(id) => {
                    leaders.insert(shard_id.clone(), id);
                }
                None => without_leader.push(shard_id.clone()),
            }
        }
        if without_leader.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "GH #99: {} of {} shards never elected a raft leader within {secs}s \
                 (leader_id stayed null): {without_leader:?}. The control-plane apply \
                 hook registered each shard's per-shard Raft group on every node, but \
                 the assigned leader never called initialize_membership — so the groups \
                 sit as empty-membership learners that can never elect a leader or \
                 accept a write.",
                without_leader.len(),
                shard_ids.len(),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    // GH #101: leadership must distribute across the cluster, not pile
    // onto the control-plane leader. `compute_shard_ranges` assigns
    // leader_node round-robin, and each assigned leader initializes its
    // own shards, so a 6-shard namespace on a 3-node cluster should land
    // leaders on more than one node.
    let distinct: std::collections::HashSet<u64> = leaders.values().copied().collect();
    assert!(
        distinct.len() > 1,
        "GH #101: all {} shard leaders landed on a single node {distinct:?} — per-shard \
         leadership did not distribute across the cluster (expected the assigned \
         leader_node round-robin to spread leaders over multiple nodes). leaders={leaders:?}",
        shard_ids.len(),
    );
}

#[then("the MergeShards response merged_shard_id equals the left shard id")]
async fn then_merge_response_id_matches_left(w: &mut KisekiWorld) {
    let left = w
        .cluster
        .name_index_state
        .get("split_left_shard_id")
        .cloned()
        .expect("SplitShard step must run first");
    let merged = w
        .cluster
        .name_index_state
        .get("merged_shard_id")
        .cloned()
        .expect("MergeShards step must run first");
    // The proto convention `merge_shards.rs` documents: "merge right
    // into left → left becomes the surviving target". So the merged
    // id must equal the left input.
    assert_eq!(
        merged, left,
        "MergeShards merged_shard_id should equal the left input shard",
    );
}
