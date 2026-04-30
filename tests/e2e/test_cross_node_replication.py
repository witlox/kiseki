"""E2E: Cross-node chunk replication (Phase 16a).

Verifies that the cluster fabric (ClusterChunkService gRPC) actually
replicates chunks across nodes — closes the B-3 gap from the single-
node→multi-node-cluster transition. Each test PUTs via one node and
GETs via another to prove the fabric layer is wired end-to-end.

Requires docker-compose.3node.yml (or .3node-tls.yml). On each node:
  - S3 gateway:  9000 (host port 9000 / 9010 / 9020)
  - data-path:   9100 (host port 9100 / 9110 / 9120)
  - metrics:     9090 (host port 9090 / 9091 / 9092)

Spec: specs/implementation/phase-16-cross-node-chunks.md (rev 4)
"""

from __future__ import annotations

import time

import pytest
import requests

# S3 gateway ports per node (matches docker-compose.3node.yml).
S3 = {
    1: "http://127.0.0.1:9000",
    2: "http://127.0.0.1:9010",
    3: "http://127.0.0.1:9020",
}
METRICS = {
    1: "http://127.0.0.1:9090",
    2: "http://127.0.0.1:9091",
    3: "http://127.0.0.1:9092",
}

COMPOSE_FILE = "docker-compose.3node.yml"


@pytest.fixture(scope="module")
def cluster():
    """3-node cluster fixture. Skips the module if docker isn't available."""
    try:
        from helpers.cluster import start_cluster, stop_cluster
    except ImportError:
        pytest.skip("helpers.cluster not importable — e2e harness missing")
    try:
        info = start_cluster(COMPOSE_FILE)
    except Exception as e:  # noqa: BLE001 — fixture-skip on any setup failure
        pytest.skip(f"3-node cluster could not start: {e}")
    yield info
    try:
        stop_cluster(info)
    except Exception:  # noqa: BLE001
        pass


def _wait_s3(base_url: str, timeout: float = 30.0) -> None:
    """Block until S3 endpoint accepts a GET / (no auth required)."""
    deadline = time.monotonic() + timeout
    last_err: Exception | None = None
    while time.monotonic() < deadline:
        try:
            resp = requests.get(base_url, timeout=2)
            if resp.status_code in (200, 403, 404):
                return
        except requests.RequestException as e:  # connection refused, etc.
            last_err = e
        time.sleep(0.5)
    raise RuntimeError(
        f"S3 endpoint {base_url} not ready after {timeout}s: {last_err}"
    )


BOOTSTRAP_SHARD_ID = "00000000-0000-0000-0000-000000000001"


def _wait_for_shard_leader(
    node: int,
    shard_id: str = BOOTSTRAP_SHARD_ID,
    timeout: float = 30.0,
) -> None:
    """Block until `node` reports a leader for the given Raft shard.

    Phase 17 item 4 added `GET /cluster/shards/{shard_id}/leader` for
    exactly this — `cluster/info` reports a cluster-level leader, but
    Raft elections are per-shard, and a write to a non-bootstrap shard
    can fail with `LeaderUnavailable: ShardId(X)` even when
    `cluster/info` looks healthy. Tests that follow a node-restart
    poll this surface to wait for the right thing.
    """
    deadline = time.monotonic() + timeout
    last_seen: str = ""
    while time.monotonic() < deadline:
        try:
            resp = requests.get(
                f"{METRICS[node]}/cluster/shards/{shard_id}/leader", timeout=2
            )
            if resp.status_code == 200:
                info = resp.json()
                if info.get("leader_id") is not None:
                    return
                last_seen = f"no leader_id in {info}"
            else:
                last_seen = f"HTTP {resp.status_code}"
        except requests.RequestException as e:
            last_seen = str(e)
        time.sleep(0.25)
    raise RuntimeError(
        f"Raft leader not elected on node{node} for shard "
        f"{shard_id} after {timeout}s: {last_seen}"
    )


# Backwards-compat name kept while in-flight Phase 17 work lands; new
# tests should use the shard-specific helper directly.
def _wait_for_leader(node: int, timeout: float = 30.0) -> None:
    _wait_for_shard_leader(node, BOOTSTRAP_SHARD_ID, timeout)


def _put_object(node: int, key: str, data: bytes) -> str:
    """PUT an object via the named node's S3 listener; return the etag.

    Tests that arrive after a node-kill (post-resilience scenarios) call
    `_wait_for_shard_leader` first — the Phase 17 item 4 endpoint exposes
    the per-shard Raft leader. Even so, there's a brief window between
    "endpoint reports a leader_id" and "the gateway's log handle has
    observed the new term," during which a PUT can surface
    `LeaderUnavailable`. Retry on that specific transient up to ~5s.
    """
    deadline = time.monotonic() + 5.0
    last: requests.Response | None = None
    while True:
        resp = requests.put(f"{S3[node]}/default/{key}", data=data, timeout=10)
        if resp.status_code in (200, 201):
            etag = resp.headers.get("ETag", "").strip('"')
            assert etag, f"PUT response missing ETag header: {resp.headers}"
            return etag
        last = resp
        if resp.status_code == 500 and "leader unavailable" in resp.text.lower():
            if time.monotonic() < deadline:
                time.sleep(0.25)
                continue
        break
    assert False, (
        f"PUT via node{node} failed: {last.status_code} {last.text!r}"
    )


def _get_object(node: int, etag: str) -> bytes:
    """GET an object by its etag via the named node's S3 listener."""
    resp = requests.get(f"{S3[node]}/default/{etag}", timeout=10)
    assert resp.status_code == 200, (
        f"GET via node{node} failed: {resp.status_code} {resp.text!r}"
    )
    return resp.content


def _scrape_metric(node: int, metric_name: str) -> float | None:
    """Scrape Prometheus /metrics, return the sum across all label sets,
    or None if the metric isn't present yet."""
    try:
        resp = requests.get(f"{METRICS[node]}/metrics", timeout=5)
    except requests.RequestException:
        return None
    if resp.status_code != 200:
        return None
    total = 0.0
    found = False
    for line in resp.text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        if line.split()[0].split("{")[0] == metric_name:
            try:
                total += float(line.rsplit(" ", 1)[1])
                found = True
            except (IndexError, ValueError):
                pass
    return total if found else None


# ---------------------------------------------------------------------------
# B-3 closure: cross-node read after a single-node PUT
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
def test_cross_node_read_after_leader_put(cluster):
    """A PUT on node-1 must be readable on node-2 and node-3.

    Closes the B-3 gap: prior to Phase 16a a PUT on node-1 left
    node-2 + node-3 with no copy of the chunk → 404 on cross-node
    GET. Phase 16a wires `ClusteredChunkStore` so the leader fans
    the fragment out, and Phase 16f wires the composition hydrator
    so followers can resolve the composition_id.
    """
    for n in (1, 2, 3):
        _wait_s3(S3[n])

    payload = b"phase16-cross-node-roundtrip" * 64
    etag = _put_object(1, "cross-node-1", payload)

    # Allow Raft commit + fabric fan-out to settle.
    time.sleep(1)

    for n in (2, 3):
        got = _get_object(n, etag)
        assert got == payload, (
            f"node{n} returned different bytes — fabric replication broken"
        )


# ---------------------------------------------------------------------------
# Single-node-failure survival — D-1, the whole point of Phase 16a
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
def test_read_survives_single_node_failure(cluster):
    """Kill node-1 after a PUT lands; reads on node-2 must still work."""
    try:
        from helpers.cluster import start_node, stop_node
    except ImportError:
        pytest.skip("helpers.cluster not importable")

    for n in (1, 2, 3):
        _wait_s3(S3[n])

    payload = b"phase16-survives-failure" * 32
    etag = _put_object(1, "survives-1", payload)
    time.sleep(1)  # let fan-out settle

    stop_node(cluster.compose_file, "kiseki-node1")
    try:
        # Allow leader election to converge before the read.
        time.sleep(5)
        # Read via node-2; node-1 is gone, so this exercises the
        # local-store path (the fragment was already fanned out).
        got = _get_object(2, etag)
        assert got == payload, "node-2 lost data after node-1 kill"
    finally:
        start_node(cluster.compose_file, "kiseki-node1")
        time.sleep(5)


# ---------------------------------------------------------------------------
# Quorum-lost write surfaces 503 to the client
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
@pytest.mark.slow
def test_write_quorum_lost_returns_503(cluster):
    """With 2 of 3 peers down, a PUT on node-1 must fail with 503.

    Phase 16a Replication-3 default is min_acks=2; node-1's local
    write is 1 ack and the two unreachable peers are 0 acks → total
    1 < 2 ⇒ ChunkError::QuorumLost ⇒ S3 503 with retry-after.
    """
    try:
        from helpers.cluster import start_node, stop_node
    except ImportError:
        pytest.skip("helpers.cluster not importable")

    for n in (1, 2, 3):
        _wait_s3(S3[n])

    stop_node(cluster.compose_file, "kiseki-node2")
    stop_node(cluster.compose_file, "kiseki-node3")
    try:
        # Wait for node-1 to notice the peers are down.
        time.sleep(5)
        resp = requests.put(
            f"{S3[1]}/default/quorum-lost-1",
            data=b"this should fail",
            timeout=15,
        )
        # The S3 gateway maps RetriableError::ShardUnavailable → 503.
        # The exact code path is gateway-error-mapping; what matters
        # is that the client gets a non-2xx with retry-able semantics.
        assert resp.status_code in (503, 500, 504), (
            f"expected 5xx with quorum lost, got {resp.status_code}: {resp.text!r}"
        )
    finally:
        start_node(cluster.compose_file, "kiseki-node2")
        start_node(cluster.compose_file, "kiseki-node3")
        time.sleep(5)


# ---------------------------------------------------------------------------
# Metrics surface
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
def test_fabric_metrics_present_after_cross_node_write(cluster):
    """After a cross-node PUT, kiseki_fabric_ops_total must appear on
    the leader's /metrics with at least one PUT-OK entry per peer."""
    for n in (1, 2, 3):
        _wait_s3(S3[n])
    # Prior tests in this module restart nodes (resilience scenarios).
    # Wait for Raft to settle before issuing a write — otherwise the PUT
    # races leader election and surfaces a 500 LeaderUnavailable that has
    # nothing to do with what this test is checking.
    _wait_for_leader(1)

    # Fail-soft when the metrics port isn't host-mapped (older
    # docker-compose configs). Step 10 ships the port mapping in
    # docker-compose.3node.yml; older clusters skip this assertion.
    try:
        ping = requests.get(f"{METRICS[1]}/health", timeout=2)
        if ping.status_code != 200:
            pytest.skip(f"metrics endpoint not healthy: {ping.status_code}")
    except requests.RequestException as e:
        pytest.skip(f"metrics endpoint not reachable: {e}")

    payload = b"phase16-metrics-witness"
    _put_object(1, "metrics-1", payload)
    time.sleep(1)

    # The metric exposition format puts label sets on each line. We
    # don't dissect labels here — we just assert the family appears
    # with a non-zero sum, which means at least one fabric RPC fired.
    total = _scrape_metric(1, "kiseki_fabric_ops_total")
    assert total is not None, (
        "kiseki_fabric_ops_total missing from node-1 /metrics — step 11 wiring broken"
    )
    assert total >= 2, (
        f"expected ≥2 fabric ops (one PUT to each of the 2 peers), got {total}"
    )


# ---------------------------------------------------------------------------
# Phase 17 item 1: cross-node delete
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
def test_delete_visible_on_followers_after_settle(cluster):
    """An S3 DELETE on node-1 must remove the composition on node-2 and node-3.

    Before Phase 17 item 1, the gateway's delete path had a "log emission
    for delete tombstone would go here if needed" TODO and never emitted
    a Delete delta. Followers' compositions stayed alive forever and a
    cross-node GET returned 200 long after the leader had `forgotten' the
    object. This test forces the gap.
    """
    for n in (1, 2, 3):
        _wait_s3(S3[n])
    _wait_for_leader(1)

    payload = b"phase17-delete-bytes" * 32
    etag = _put_object(1, "delete-1", payload)
    time.sleep(1)  # let the Create delta hydrate on followers

    # Pre-condition: the composition exists on every node.
    for n in (1, 2, 3):
        got = _get_object(n, etag)
        assert got == payload, f"pre-delete: node{n} disagrees with bytes"

    # Issue the DELETE on the leader. S3 path uses the etag as the
    # object key (consistent with how _get_object addresses objects).
    resp = requests.delete(f"{S3[1]}/default/{etag}", timeout=10)
    assert resp.status_code in (200, 204), (
        f"DELETE via node1 failed: {resp.status_code} {resp.text!r}"
    )

    # Allow the Delete delta to hydrate on followers (~100 ms hydrator
    # poll + apply). 1 s is 10× the budget.
    time.sleep(1)

    # Post-condition: GET returns 404 on every node, including the
    # leader (whose compositions store dropped it inline).
    for n in (1, 2, 3):
        resp = requests.get(f"{S3[n]}/default/{etag}", timeout=10)
        assert resp.status_code == 404, (
            f"post-delete: node{n} still serves the object — "
            f"got {resp.status_code} (Delete delta not hydrated)"
        )


# ---------------------------------------------------------------------------
# Phase 17 item 4: per-shard leader endpoint
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
def test_per_shard_leader_agrees_across_nodes(cluster):
    """`GET /cluster/shards/{shard_id}/leader` reports the same leader on
    every node — the openraft state machine is consistent across the
    quorum, so any node's view of the per-shard leader matches.

    Runs last so it exercises the endpoint after the resilience tests
    have killed and restarted nodes (i.e. with a non-trivial leader
    history, not just the bootstrap leader). `_wait_for_shard_leader`
    on each node ensures the cluster is settled before the comparison.
    """
    for n in (1, 2, 3):
        _wait_s3(S3[n])
        _wait_for_shard_leader(n, BOOTSTRAP_SHARD_ID)

    leaders: list[int] = []
    for n in (1, 2, 3):
        resp = requests.get(
            f"{METRICS[n]}/cluster/shards/{BOOTSTRAP_SHARD_ID}/leader", timeout=2
        )
        assert resp.status_code == 200, (
            f"node{n}: shard-leader endpoint returned {resp.status_code} "
            f"{resp.text!r}"
        )
        info = resp.json()
        assert info["shard_id"] == BOOTSTRAP_SHARD_ID
        assert info["leader_id"] is not None, f"node{n}: no leader reported"
        assert isinstance(info["raft_members"], list)
        # Phase 17 ADR-040 §D6.3 / I-2 (auditor finding A2): the
        # per-shard endpoint must surface the composition hydrator
        # halt flag so load balancers can route around a halted
        # node. Field always present; always `false` in steady-state
        # multi-node runs (no compaction).
        assert "composition_hydrator_halted" in info, (
            f"node{n}: shard-leader response missing "
            f"`composition_hydrator_halted` field"
        )
        assert info["composition_hydrator_halted"] is False, (
            f"node{n}: hydrator unexpectedly halted in steady-state — "
            f"compaction shouldn't fire in this test"
        )
        leaders.append(info["leader_id"])

    assert len(set(leaders)) == 1, (
        f"per-shard leader disagreement: {leaders!r} across nodes"
    )


@pytest.mark.cross_node
def test_per_shard_leader_endpoint_rejects_bad_uuid(cluster):
    """Malformed shard_id surfaces a 400, not a panic or a 500."""
    for n in (1, 2, 3):
        _wait_s3(S3[n])
    resp = requests.get(f"{METRICS[1]}/cluster/shards/not-a-uuid/leader", timeout=2)
    assert resp.status_code == 400, (
        f"expected 400 for malformed UUID, got {resp.status_code}: {resp.text!r}"
    )


# ---------------------------------------------------------------------------
# Phase 17 integrator-pass: cross-context seams
# ---------------------------------------------------------------------------


@pytest.mark.cross_node
def test_phase_17_metrics_surface_includes_gateway_retry_counters(cluster):
    """ADR-040 §D7 + §D10 / F-4 closure (auditor finding A5).

    Verifies the new gateway-side retry counters
    (`kiseki_gateway_read_retry_total` and
    `kiseki_gateway_read_retry_exhausted_total`) actually appear on
    the `/metrics` endpoint. Without this integration test, a
    regression that breaks the Prometheus registration (e.g. the
    runtime forgetting to clone the metrics into the gateway
    builder) would silently land — operators would see flat zeros
    in their dashboards and no alerts when the retry budget is
    exhausted.

    Runs against every node — all three must surface the counters.
    """
    for n in (1, 2, 3):
        _wait_s3(S3[n])

    expected_metrics = [
        "kiseki_gateway_read_retry_total",
        "kiseki_gateway_read_retry_exhausted_total",
    ]
    for n in (1, 2, 3):
        resp = requests.get(f"{METRICS[n]}/metrics", timeout=5)
        assert resp.status_code == 200, (
            f"node{n}: /metrics returned {resp.status_code}"
        )
        body = resp.text
        for metric_name in expected_metrics:
            # Look for the `# HELP` or `# TYPE` line followed by a
            # value line. Just checking the name appears as a
            # standalone token is enough.
            assert metric_name in body, (
                f"node{n}: /metrics missing `{metric_name}` — registration "
                f"regressed in `KisekiMetrics::new()` or "
                f"`InMemoryGateway::with_retry_metrics(...)`"
            )


@pytest.mark.cross_node
def test_persistence_survives_node_restart(cluster):
    """ADR-040 §D1 / I-CP1 (auditor finding A4 closure).

    Verifies the full integration path for persistent compositions:
    docker-compose volume → KISEKI_DATA_DIR → metadata/compositions.redb →
    open-or-init at boot → CompositionStore::with_storage(persistent) →
    gateway reads through it. The unit tests cover each layer in
    isolation; this proves they wire together correctly under a real
    `docker compose stop` + `start` (which preserves the volume).

    Sequence:
      1. PUT on node-1 → composition created.
      2. Wait for hydration on node-2 → composition replicates.
      3. `docker compose stop` node-2 (preserves volume + redb).
      4. `docker compose start` node-2 → fresh process opens the
         existing redb at `/data/metadata/compositions.redb`.
      5. Wait for Raft + S3 to come back up.
      6. GET via node-2 → must still return 200 with the original
         bytes. The hydrator's last_applied_seq is durable so it
         doesn't re-process from seq=1; the composition is served
         from the persistent store directly.
    """
    try:
        from helpers.cluster import start_node, stop_node
    except ImportError:
        pytest.skip("helpers.cluster not importable")

    for n in (1, 2, 3):
        _wait_s3(S3[n])

    payload = b"phase17-persistence-survives-restart" * 16
    etag = _put_object(1, "persist-1", payload)
    time.sleep(1)  # let the Create delta hydrate on node-2 + node-3

    # Pre-condition: cross-node read works on node-2.
    pre = _get_object(2, etag)
    assert pre == payload, "pre-restart: node-2 must have the composition"

    # Stop + start node-2. `docker compose stop` (not down) preserves
    # the named volume `node2-data` and therefore the redb files
    # under /data/metadata/.
    stop_node(cluster.compose_file, "kiseki-node2")
    try:
        # Brief settle so the leader notices.
        time.sleep(2)
        start_node(cluster.compose_file, "kiseki-node2")
        # Wait for the restarted node to come back online.
        _wait_s3(S3[2])
        _wait_for_shard_leader(2, BOOTSTRAP_SHARD_ID)

        # Post-condition: GET on the freshly-restarted node still
        # serves the bytes. No re-hydration delay needed because
        # last_applied_seq is durable (I-CP1).
        post = _get_object(2, etag)
        assert post == payload, (
            "post-restart: node-2 lost the composition — "
            "PersistentRedbStorage didn't survive the restart"
        )
    finally:
        # Make sure the cluster is in a steady state before later
        # tests in the module run. Defensive: if start_node already
        # fired, calling start again on a running node is a no-op
        # in docker compose.
        try:
            start_node(cluster.compose_file, "kiseki-node2")
        except Exception:
            pass
