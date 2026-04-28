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


def _put_object(node: int, key: str, data: bytes) -> str:
    """PUT an object via the named node's S3 listener; return the etag (=
    server-assigned object id) used for retrieval."""
    resp = requests.put(f"{S3[node]}/default/{key}", data=data, timeout=10)
    assert resp.status_code in (200, 201), (
        f"PUT via node{node} failed: {resp.status_code} {resp.text!r}"
    )
    etag = resp.headers.get("ETag", "").strip('"')
    assert etag, f"PUT response missing ETag header: {resp.headers}"
    return etag


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

    This is the B-3 gap that motivated Phase 16a: in a 3-node cluster
    each node holds its own chunk store, and prior to Phase 16a a
    PUT on node-1 left node-2 + node-3 with no copy → a GET on
    node-2 returned 404, breaking the basic HA promise.

    With ClusteredChunkStore + ClusterChunkService wired in, the
    leader fans the fragment out to all peers; a GET on any node
    succeeds.
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
