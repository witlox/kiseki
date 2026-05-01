//! Steps for `@integration @multi-node` scenarios.
//!
//! Cluster-aware scenarios acquire the process-level singleton from
//! `cluster_harness::acquire_cluster()`. Single-node scenarios stay on
//! the existing `world.server()` harness — they share these step files
//! to keep the 1-node baseline alongside the 3-node assertions.
//!
//! Every scenario gets a fresh bucket name (`bdd-{uuid}`) so we never
//! restart the cluster between runs.

use cucumber::{given, then, when};

use crate::steps::cluster_harness::{acquire_cluster, NodeHandle};
use crate::KisekiWorld;

/// 1 MiB — the size every "1MB" step in this file uses. Keeping the
/// constant lets us tweak the scenario boundary in one place if we
/// later split into "small" vs "large" object paths.
const ONE_MEBIBYTE: usize = 1024 * 1024;

fn megabyte_payload() -> Vec<u8> {
    // Pseudo-random but deterministic — rolling-hash friendly so a
    // partial-replication bug shows up as a body-mismatch on GET, not
    // as a length-only mismatch.
    let mut buf = Vec::with_capacity(ONE_MEBIBYTE);
    let mut x: u32 = 0x9E37_79B9; // golden ratio constant
    for _ in 0..ONE_MEBIBYTE {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        buf.push((x >> 16) as u8);
    }
    buf
}

fn unique_key() -> String {
    format!("bdd-{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// Single-node baseline (scenario "S3 PUT on single-node server …")
// ---------------------------------------------------------------------------

#[when("a client writes 1MB via S3 PUT")]
async fn when_client_writes_1mb_single(w: &mut KisekiWorld) {
    let body = megabyte_payload();
    let key = unique_key();
    let url = w.server().s3_url(&format!("default/{key}"));
    let resp = w
        .server()
        .http
        .put(&url)
        .body(body.clone())
        .send()
        .await
        .expect("HTTP PUT failed");
    assert!(
        resp.status().is_success(),
        "S3 PUT returned {}: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_owned())
        .expect("S3 PUT response must carry ETag");
    w.cluster.bucket = Some("default".to_owned());
    w.cluster.key = Some(key);
    w.cluster.last_etag = Some(etag);
    w.cluster.expected_body = Some(body);
}

#[then("S3 GET returns the same 1MB")]
async fn then_s3_get_returns_same_1mb_single(w: &mut KisekiWorld) {
    let bucket = w.cluster.bucket.as_ref().expect("PUT must run first");
    let etag = w.cluster.last_etag.as_ref().expect("PUT must capture ETag");
    let url = w.server().s3_url(&format!("{bucket}/{etag}"));
    let resp = w
        .server()
        .http
        .get(&url)
        .send()
        .await
        .expect("HTTP GET failed");
    assert!(
        resp.status().is_success(),
        "S3 GET returned {}",
        resp.status()
    );
    let body = resp.bytes().await.expect("read body").to_vec();
    let expected = w
        .cluster
        .expected_body
        .as_ref()
        .expect("PUT must record body");
    assert_eq!(
        body.len(),
        expected.len(),
        "S3 GET body length mismatch: got {} want {}",
        body.len(),
        expected.len(),
    );
    assert_eq!(
        body.as_slice(),
        expected.as_slice(),
        "S3 GET body bytes mismatch",
    );
}

#[then("the server did not report quorum errors")]
async fn then_no_quorum_errors_single(w: &mut KisekiWorld) {
    let url = format!("http://127.0.0.1:{}/metrics", w.server().ports.metrics);
    let resp = w
        .server()
        .http
        .get(&url)
        .send()
        .await
        .expect("metrics scrape failed");
    let text = resp.text().await.expect("metrics body");
    let count = parse_counter(&text, "kiseki_fabric_quorum_lost_total");
    assert_eq!(
        count, 0,
        "kiseki_fabric_quorum_lost_total should be 0, got {count}",
    );
}

// ---------------------------------------------------------------------------
// 3-node cluster (scenario "S3 PUT on 3-node cluster replicates to all nodes")
// ---------------------------------------------------------------------------

#[given("a 3-node kiseki cluster")]
async fn given_3_node_cluster(w: &mut KisekiWorld) {
    let cluster = acquire_cluster()
        .await
        .expect("failed to start 3-node cluster");
    // Sanity: confirm a leader exists before letting the scenario run.
    let guard = cluster.lock().await;
    let leader = guard.leader_id().await;
    assert!(leader.is_some(), "3-node cluster has no elected leader",);
    drop(guard);
    // Bucket "default" is the only namespace each node creates at
    // bootstrap; per-scenario isolation comes from the random key.
    w.cluster.bucket = Some("default".to_owned());
    w.cluster.key = Some(unique_key());
}

#[when("a client writes 1MB via S3 PUT to node-1")]
async fn when_client_writes_1mb_to_node1(w: &mut KisekiWorld) {
    let body = megabyte_payload();
    let cluster = acquire_cluster().await.expect("cluster handle");
    let bucket = w
        .cluster
        .bucket
        .clone()
        .expect("bucket must be set by Given step");
    let key = w
        .cluster
        .key
        .clone()
        .expect("key must be set by Given step");
    let etag = {
        let guard = cluster.lock().await;
        let node = guard.node(1);
        let url = format!("{}/{bucket}/{key}", node.s3_base);
        let resp = node
            .http
            .put(&url)
            .body(body.clone())
            .send()
            .await
            .expect("HTTP PUT to node-1 failed");
        assert!(
            resp.status().is_success(),
            "S3 PUT to node-1 returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        resp.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_owned())
            .expect("S3 PUT response must carry ETag")
    };
    w.cluster.last_etag = Some(etag);
    w.cluster.expected_body = Some(body);
}

#[then(regex = r"^S3 GET from node-(\d+) returns the same 1MB$")]
async fn then_s3_get_from_node(w: &mut KisekiWorld, node_id: u64) {
    let cluster = acquire_cluster().await.expect("cluster handle");
    let bucket = w.cluster.bucket.clone().expect("bucket missing");
    let etag = w.cluster.last_etag.clone().expect("etag missing");
    let expected = w
        .cluster
        .expected_body
        .clone()
        .expect("expected body missing");
    // Followers serve reads from their local CompositionStore, which
    // lags the leader via Raft delta hydration (ADR-040). Allow up to
    // 10s for the hydrator to catch up — a real client would retry the
    // same way after a routing-to-follower 404.
    let body = {
        let guard = cluster.lock().await;
        let node = guard.node(node_id);
        let url = format!("{}/{bucket}/{etag}", node.s3_base);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let resp = node
                .http
                .get(&url)
                .send()
                .await
                .unwrap_or_else(|e| panic!("HTTP GET from node-{node_id} failed: {e}"));
            let status = resp.status();
            if status.is_success() {
                break resp.bytes().await.expect("read body").to_vec();
            }
            if std::time::Instant::now() >= deadline {
                panic!("S3 GET from node-{node_id} kept failing: last status {status}",);
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };
    assert_eq!(
        body.len(),
        expected.len(),
        "node-{node_id} body length mismatch: got {} want {}",
        body.len(),
        expected.len(),
    );
    assert_eq!(
        body.as_slice(),
        expected.as_slice(),
        "node-{node_id} body bytes mismatch — replication did not converge",
    );
}

// ---------------------------------------------------------------------------
// Leader-failure scenario
// ---------------------------------------------------------------------------

#[when("the current leader is killed")]
async fn when_kill_leader(w: &mut KisekiWorld) {
    let cluster = acquire_cluster().await.expect("cluster handle");
    let mut guard = cluster.lock().await;
    let leader = guard
        .leader_id()
        .await
        .expect("cluster has no leader before kill — readiness probe should have failed earlier");
    guard
        .kill_node(leader)
        .await
        .unwrap_or_else(|e| panic!("kill leader (node-{leader}): {e}"));
    w.cluster.killed_leader = Some(leader);
}

#[then(regex = r"^a new leader is elected within (\d+) seconds$")]
async fn then_new_leader_elected(w: &mut KisekiWorld, secs: u64) {
    let cluster = acquire_cluster().await.expect("cluster handle");
    let killed = w
        .cluster
        .killed_leader
        .expect("kill-leader step must run first");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        let guard = cluster.lock().await;
        // Ask a known-alive node (any non-killed node).
        let alive = guard
            .nodes()
            .find(|n| n.node_id != killed)
            .expect("at least one alive node");
        let url = alive.admin_url("cluster/info");
        let leader = match alive.http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("leader_id").and_then(serde_json::Value::as_u64)),
            _ => None,
        };
        drop(guard);
        if let Some(l) = leader {
            if l != killed {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("no new leader elected within {secs}s; killed node-{killed} may have been re-elected");
}

#[when("a client writes 1MB via S3 PUT to the cluster")]
async fn when_write_to_cluster(w: &mut KisekiWorld) {
    let body = megabyte_payload();
    let key = unique_key();
    let bucket = w
        .cluster
        .bucket
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    let cluster = acquire_cluster().await.expect("cluster handle");
    let etag = {
        let guard = cluster.lock().await;
        let killed = w.cluster.killed_leader;
        // PUT to whichever node currently *thinks* it's leader — by
        // re-reading cluster_info before each attempt. The follower
        // gateways' per-shard leader cache lags Raft elections, so a
        // fixed-target PUT can stall for tens of seconds.
        let put_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            // Pick a discovery node that's known alive.
            let alive = guard
                .nodes()
                .find(|n| Some(n.node_id) != killed)
                .expect("at least one alive node");
            let leader_id = leader_id_via(alive).await;
            let target_id = leader_id
                .filter(|id| Some(*id) != killed)
                .unwrap_or(alive.node_id);
            let target = guard.node(target_id);
            let url = format!("{}/{bucket}/{key}", target.s3_base);
            let resp = target
                .http
                .put(&url)
                .body(body.clone())
                .send()
                .await
                .expect("HTTP PUT to cluster failed");
            if resp.status().is_success() {
                break resp
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.trim_matches('"').to_owned())
                    .expect("S3 PUT response must carry ETag");
            }
            if std::time::Instant::now() >= put_deadline {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                panic!(
                    "S3 PUT kept failing after election (target=node-{}): {status}: {body}",
                    target.node_id
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    };
    w.cluster.bucket = Some(bucket);
    w.cluster.key = Some(key);
    w.cluster.last_etag = Some(etag);
    w.cluster.expected_body = Some(body);
}

#[then("S3 GET from any surviving node returns the same 1MB")]
async fn then_get_from_any_surviving(w: &mut KisekiWorld) {
    let cluster = acquire_cluster().await.expect("cluster handle");
    let bucket = w.cluster.bucket.clone().expect("bucket missing");
    let etag = w.cluster.last_etag.clone().expect("etag missing");
    let expected = w.cluster.expected_body.clone().expect("body missing");
    let killed = w.cluster.killed_leader;

    let guard = cluster.lock().await;
    let surviving: Vec<u64> = guard
        .nodes()
        .filter(|n| Some(n.node_id) != killed)
        .map(|n| n.node_id)
        .collect();
    let mut last_err: Option<String> = None;
    for id in &surviving {
        let node = guard.node(*id);
        let url = format!("{}/{bucket}/{etag}", node.s3_base);
        match node.http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.bytes().await.expect("body").to_vec();
                if body.as_slice() == expected.as_slice() {
                    return;
                }
                last_err = Some(format!(
                    "node-{id} returned {} bytes, expected {}",
                    body.len(),
                    expected.len()
                ));
            }
            Ok(resp) => {
                last_err = Some(format!("node-{id} GET status {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(format!("node-{id} GET error: {e}"));
            }
        }
    }
    panic!(
        "no surviving node returned the written object: surviving={surviving:?}, last={last_err:?}"
    );
}

#[then("the killed node is restarted and rejoins the cluster")]
async fn then_restart_killed(w: &mut KisekiWorld) {
    let cluster = acquire_cluster().await.expect("cluster handle");
    let killed = w
        .cluster
        .killed_leader
        .take()
        .expect("kill-leader step must run before restart");
    let mut guard = cluster.lock().await;
    guard
        .restart_node(killed)
        .await
        .unwrap_or_else(|e| panic!("restart node-{killed} failed: {e}"));
    // wait_for_quorum already ran inside restart_node; re-check here
    // to surface a clear failure if the cluster is unstable.
    let leader = guard.leader_id().await;
    assert!(
        leader.is_some(),
        "cluster has no leader after node-{killed} rejoin",
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read `leader_id` from a specific node's `/cluster/info`. Returns
/// `None` if the call fails or the node hasn't yet seen a leader.
async fn leader_id_via(node: &NodeHandle) -> Option<u64> {
    let url = node.admin_url("cluster/info");
    let resp = node.http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("leader_id")?.as_u64()
}

/// Sum every line that matches `<name>{...} N` or `<name> N` in
/// Prometheus text-exposition format. Intentionally simple — we don't
/// care about labels for these zero-checks.
fn parse_counter(text: &str, name: &str) -> u64 {
    let mut total: u64 = 0;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let line = line.trim();
        if !line.starts_with(name) {
            continue;
        }
        // Accept either "name VALUE" or "name{labels} VALUE".
        let rest = if line.as_bytes().get(name.len()) == Some(&b'{') {
            line.split_once('}')
                .map(|(_, r)| r.trim_start())
                .unwrap_or(line)
        } else {
            line[name.len()..].trim_start()
        };
        if let Some(value_str) = rest.split_whitespace().next() {
            if let Ok(v) = value_str.parse::<f64>() {
                total = total.saturating_add(v as u64);
            }
        }
    }
    total
}
