#![allow(clippy::unwrap_used, clippy::expect_used)]
//! GH #255 — restart-under-volume catch-up recovery steps.
//!
//! Drives the production shape that wedged the 2026-06-11 GCP cluster:
//! sustained 4 KiB inline writes against a multi-shard namespace on the
//! 6-node singleton, one follower stopped mid-volume, more volume while
//! it is down (the leader's Raft log grows ahead), restart — then
//! asserts the follower genuinely catches up (per-shard committed-tip
//! convergence via `/cluster/shards/{id}/leader`) and that NO node
//! rejected an oversized Raft RPC
//! (`kiseki_raft_transport_rpc_total{outcome="parse_error"}` delta vs
//! the scenario-entry baseline; the singleton is shared across
//! scenarios so absolute counts would be polluted).

use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use futures::StreamExt;

use crate::steps::multi_node::{cluster, cluster_mut, scrape_metrics, sum_counter_matching_all};
use crate::KisekiWorld;

/// Concurrent in-flight PUTs for the volume steps. Deep enough to keep
/// the committer's drain batches fat (the GH #255 entry shape), small
/// enough not to starve CI runners.
const WRITE_CONCURRENCY: usize = 64;

/// Per-object retry deadline. Covers a shard-leader re-election when
/// the killed node led one of the namespace shards.
const PER_OBJECT_DEADLINE: Duration = Duration::from_secs(60);

/// A distinct 4096-byte payload — unique content per (seed, i) so
/// content-addressed dedup cannot collapse the volume into one chunk.
fn inline_payload(seed: u64, i: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(i.wrapping_mul(0xD1B5_4A32_D192_ED03))
        | 1;
    let mut buf = Vec::with_capacity(4096);
    while buf.len() < 4096 {
        // xorshift64* — cheap, full-period, distinct streams per seed.
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        buf.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    buf.truncate(4096);
    buf
}

/// Snapshot per-node `(node_id, s3_base, metrics_port, http)` without
/// holding the cluster borrow across awaits.
fn snapshot_nodes(w: &KisekiWorld) -> Vec<(u64, String, u16, reqwest::Client)> {
    cluster(w)
        .nodes()
        .map(|n| {
            (
                n.node_id,
                n.s3_base.clone(),
                n.ports.metrics,
                n.http.clone(),
            )
        })
        .collect()
}

/// The label-fragment for the receiver-side oversized/parse rejection
/// outcome on the multiplexed Raft transport.
const PARSE_ERROR_FRAGMENT: &str = r#"outcome="parse_error""#;

#[given("the oversized Raft RPC rejection baseline is recorded")]
async fn given_oversized_baseline(w: &mut KisekiWorld) {
    let nodes = snapshot_nodes(w);
    for (node_id, _s3, metrics_port, http) in nodes {
        let url = format!("http://127.0.0.1:{metrics_port}/metrics");
        let text = http
            .get(&url)
            .send()
            .await
            .expect("metrics scrape for baseline")
            .text()
            .await
            .expect("metrics body for baseline");
        let v = sum_counter_matching_all(
            &text,
            "kiseki_raft_transport_rpc_total",
            &[PARSE_ERROR_FRAGMENT],
        );
        w.cluster
            .metric_baselines
            .insert(format!("node-{node_id}/raft_parse_error"), v);
    }
}

#[when(regex = r"^(\d+) (?:more )?distinct 4KB objects are written via S3 across the cluster$")]
async fn when_volume_writes(w: &mut KisekiWorld, n: u64) {
    let bucket = w
        .cluster
        .name_index_state
        .get("fwd_bucket")
        .expect("the namespace step must run first")
        .clone();
    // Continue key numbering across the two volume phases so phase 2
    // never overwrites phase 1 (overwrites would dedup-shrink the log
    // growth the scenario depends on).
    let offset: u64 = w
        .cluster
        .name_index_state
        .get("v255_offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Unique content seed per scenario run — the 6-node singleton is
    // shared, and identical content across runs would dedup away.
    let seed: u64 = match w.cluster.name_index_state.get("v255_seed") {
        Some(s) => s.parse().expect("stored seed parses"),
        None => {
            let s = uuid::Uuid::new_v4().as_u128() as u64;
            w.cluster
                .name_index_state
                .insert("v255_seed".into(), s.to_string());
            s
        }
    };
    let killed = w.cluster.killed_leader;
    let targets: Vec<(String, reqwest::Client)> = snapshot_nodes(w)
        .into_iter()
        .filter(|(id, _, _, _)| Some(*id) != killed)
        .map(|(_, s3, _, http)| (s3, http))
        .collect();
    assert!(!targets.is_empty(), "no alive node to write to");

    let started = Instant::now();
    let results: Vec<Result<(), String>> = futures::stream::iter(0..n)
        .map(|i| {
            let key_idx = offset + i;
            // Round-robin ingress across alive nodes — the gateway
            // forwards to the owning shard leader (GH #111 path).
            let (s3_base, http) = targets[(i as usize) % targets.len()].clone();
            let bucket = bucket.clone();
            async move {
                let url = format!("{s3_base}/{bucket}/v255-{key_idx}");
                let body = inline_payload(seed, key_idx);
                let deadline = Instant::now() + PER_OBJECT_DEADLINE;
                loop {
                    let last = match http.put(&url).body(body.clone()).send().await {
                        Ok(r) if r.status().is_success() => return Ok(()),
                        Ok(r) => format!("HTTP {}", r.status()),
                        Err(e) => format!("{e}"),
                    };
                    if Instant::now() >= deadline {
                        return Err(format!("PUT v255-{key_idx} kept failing: {last}"));
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        })
        .buffer_unordered(WRITE_CONCURRENCY)
        .collect()
        .await;

    let failures: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        failures.is_empty(),
        "GH #255 volume phase: {}/{n} writes failed permanently \
         (first: {}); elapsed {:?}",
        failures.len(),
        failures[0],
        started.elapsed(),
    );
    w.cluster
        .name_index_state
        .insert("v255_offset".into(), (offset + n).to_string());
}

#[when("a follower member of the volume namespace is killed")]
async fn when_kill_volume_follower(w: &mut KisekiWorld) {
    let shard_ids: Vec<String> = w
        .cluster
        .name_index_state
        .get("fwd_shard_ids")
        .map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect()
        })
        .expect("the namespace step must run first");
    assert!(
        !shard_ids.is_empty(),
        "namespace step recorded no shard ids"
    );

    let nodes = snapshot_nodes(w);
    let http = reqwest::Client::new();

    // Control-plane leader (bootstrap /cluster/info) — avoid killing it
    // so admin probing keeps working while the node is down.
    let mut cp_leader: Option<u64> = None;
    for (_, _, port, _) in &nodes {
        let url = format!("http://127.0.0.1:{port}/cluster/info");
        if let Ok(r) = http.get(&url).send().await {
            if let Ok(j) = r.json::<serde_json::Value>().await {
                if let Some(l) = j.get("leader_id").and_then(serde_json::Value::as_u64) {
                    cp_leader = Some(l);
                    break;
                }
            }
        }
    }

    // Per-shard membership + leadership, from whichever node answers.
    let mut members: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut shard_leaders: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for sid in &shard_ids {
        for (_, _, port, _) in &nodes {
            let url = format!("http://127.0.0.1:{port}/cluster/shards/{sid}/leader");
            let Ok(r) = http.get(&url).send().await else {
                continue;
            };
            if !r.status().is_success() {
                continue;
            }
            let Ok(j) = r.json::<serde_json::Value>().await else {
                continue;
            };
            if let Some(arr) = j.get("raft_members").and_then(|m| m.as_array()) {
                for m in arr.iter().filter_map(serde_json::Value::as_u64) {
                    members.insert(m);
                }
            }
            if let Some(l) = j.get("leader_id").and_then(serde_json::Value::as_u64) {
                shard_leaders.insert(l);
            }
            break;
        }
    }
    assert!(
        !members.is_empty(),
        "no raft_members discovered for namespace shards {shard_ids:?}"
    );

    // Preference order: a member that leads nothing and isn't the
    // control-plane leader → a member that leads nothing → a member
    // that isn't the CP leader → any member. Highest id for
    // determinism (and to match the "node-6 is a follower" intuition).
    let pick = members
        .iter()
        .rev()
        .find(|id| !shard_leaders.contains(id) && Some(**id) != cp_leader)
        .or_else(|| members.iter().rev().find(|id| !shard_leaders.contains(id)))
        .or_else(|| members.iter().rev().find(|id| Some(**id) != cp_leader))
        .or_else(|| members.iter().next_back())
        .copied()
        .expect("non-empty members has a pick");

    cluster_mut(w)
        .kill_node(pick)
        .await
        .unwrap_or_else(|e| panic!("kill node-{pick}: {e}"));
    // `killed_leader` drives the alive-filter in the volume step and
    // the existing "the killed node is restarted and rejoins the
    // cluster" then-step (which `take()`s it) — stash a copy for the
    // catch-up assertion that runs after the restart.
    w.cluster.killed_leader = Some(pick);
    w.cluster
        .name_index_state
        .insert("v255_killed_node".into(), pick.to_string());
}

#[then(regex = r"^the restarted node catches up on every volume shard within (\d+)s$")]
async fn then_restarted_catches_up(w: &mut KisekiWorld, secs: u64) {
    let restarted: u64 = w
        .cluster
        .name_index_state
        .get("v255_killed_node")
        .and_then(|s| s.parse().ok())
        .expect("kill step must run first");
    let shard_ids: Vec<String> = w
        .cluster
        .name_index_state
        .get("fwd_shard_ids")
        .map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect()
        })
        .expect("namespace step must run first");

    let nodes = snapshot_nodes(w);
    let http = reqwest::Client::new();

    // Reference tips: for each shard, the max committed tip any OTHER
    // node reports, plus that shard's membership. Writes have stopped,
    // so the reference is stable; the restarted node must replicate up
    // to it (pre-#255 it could not — the catch-up batch was oversized
    // and the shard wedged forever).
    let mut reference: Vec<(String, u64)> = Vec::new(); // (shard_id, tip)
    for sid in &shard_ids {
        let mut max_tip: Option<u64> = None;
        let mut member = false;
        for (node_id, _, port, _) in &nodes {
            if *node_id == restarted {
                continue;
            }
            let url = format!("http://127.0.0.1:{port}/cluster/shards/{sid}/leader");
            let Ok(r) = http.get(&url).send().await else {
                continue;
            };
            if !r.status().is_success() {
                continue;
            }
            let Ok(j) = r.json::<serde_json::Value>().await else {
                continue;
            };
            if let Some(tip) = j
                .get("last_committed_seq")
                .and_then(serde_json::Value::as_u64)
            {
                max_tip = Some(max_tip.map_or(tip, |m: u64| m.max(tip)));
            }
            if j.get("raft_members")
                .and_then(|m| m.as_array())
                .is_some_and(|arr| {
                    arr.iter()
                        .filter_map(serde_json::Value::as_u64)
                        .any(|m| m == restarted)
                })
            {
                member = true;
            }
        }
        if member {
            reference.push((
                sid.clone(),
                max_tip.unwrap_or_else(|| panic!("no node reported a tip for shard {sid}")),
            ));
        }
    }
    assert!(
        !reference.is_empty(),
        "restarted node-{restarted} is a member of no volume shard — \
         the kill step should have picked a member"
    );

    let metrics_port = nodes
        .iter()
        .find(|(id, _, _, _)| *id == restarted)
        .map(|(_, _, port, _)| *port)
        .expect("restarted node in snapshot");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut lagging: Vec<String> = Vec::new();
    loop {
        lagging.clear();
        for (sid, want) in &reference {
            let url = format!("http://127.0.0.1:{metrics_port}/cluster/shards/{sid}/leader");
            let got: Option<u64> = match http.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    r.json::<serde_json::Value>().await.ok().and_then(|j| {
                        j.get("last_committed_seq")
                            .and_then(serde_json::Value::as_u64)
                    })
                }
                _ => None,
            };
            match got {
                Some(tip) if tip >= *want => {}
                got => lagging.push(format!("{sid}: have {got:?}, want >= {want}")),
            }
        }
        if lagging.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!(
        "GH #255: restarted node-{restarted} did NOT catch up within {secs}s on \
         {} of {} member shards: {lagging:?}. Pre-fix signature: the leader \
         retries an oversized append_entries batch forever (receiver logs \
         'Raft RPC oversized') and the follower's committed tip never moves.",
        lagging.len(),
        reference.len(),
    );
}

#[then("no node recorded an oversized Raft RPC rejection")]
async fn then_no_oversized_rejections(w: &mut KisekiWorld) {
    let nodes = snapshot_nodes(w);
    let mut offenders: Vec<String> = Vec::new();
    {
        let guard = cluster(w);
        for (node_id, _, _, _) in &nodes {
            let node = guard.node(*node_id);
            let text = scrape_metrics(node).await;
            let now = sum_counter_matching_all(
                &text,
                "kiseki_raft_transport_rpc_total",
                &[PARSE_ERROR_FRAGMENT],
            );
            let baseline = w
                .cluster
                .metric_baselines
                .get(&format!("node-{node_id}/raft_parse_error"))
                .copied()
                .unwrap_or(0.0);
            if now > baseline {
                offenders.push(format!(
                    "node-{node_id}: parse_error outcome {baseline} -> {now}"
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "GH #255: the Raft transport rejected frames during the volume + \
         restart cycle — with byte-budgeted replication batches \
         (KISEKI_RAFT_REPLICATION_BYTE_BUDGET, default frame cap / 4) no \
         oversized frame should ever be BUILT: {offenders:?}"
    );

    // Secondary witness when the harness captures child logs
    // (KISEKI_HARNESS_LOG_DIR set): the receiver-side warn line the
    // GCP incident was diagnosed from must not appear.
    if let Ok(dir) = std::env::var("KISEKI_HARNESS_LOG_DIR") {
        let mut hits: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("log") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let count = content.matches("Raft RPC oversized").count();
                    if count > 0 {
                        hits.push(format!("{}: {count} occurrences", path.display()));
                    }
                }
            }
        }
        assert!(
            hits.is_empty(),
            "GH #255: 'Raft RPC oversized' found in harness logs: {hits:?}"
        );
    }
}
