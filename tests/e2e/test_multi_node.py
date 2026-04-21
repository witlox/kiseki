"""E2E: 3-node Raft cluster — startup, connectivity, and failover.

Tests multi-node Docker compose deployment. Each node runs with a
unique KISEKI_NODE_ID and shared KISEKI_RAFT_PEERS configuration.

Requires docker compose. Run with:
    pytest tests/e2e/test_multi_node.py -v
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Generator

import grpc
import pytest

sys.path.insert(0, str(Path(__file__).parent / "proto"))

from conftest import BOOTSTRAP_SHARD_UUID, BOOTSTRAP_TENANT_UUID
from helpers.cluster import (
    ClusterInfo,
    _wait_for_ready,
    start_cluster,
    stop_cluster,
    stop_node,
    start_node,
)
from kiseki.v1 import common_pb2, log_pb2, log_pb2_grpc


COMPOSE_FILE = "docker-compose.3node.yml"


@pytest.fixture(scope="module")
def cluster() -> Generator[ClusterInfo, None, None]:
    """Boot the 3-node cluster and yield connection info."""
    info = start_cluster(COMPOSE_FILE)
    yield info
    stop_cluster(info)


def _make_timestamp(ms: int = 5000) -> common_pb2.DeltaTimestamp:
    return common_pb2.DeltaTimestamp(
        hlc=common_pb2.HybridLogicalClock(physical_ms=ms, logical=0, node_id=1),
        wall=common_pb2.WallTime(millis_since_epoch=ms, timezone="UTC"),
        quality=1,
    )


def _append_delta(addr: str, payload: bytes) -> int:
    """Append a delta and return the sequence number."""
    channel = grpc.insecure_channel(addr)
    stub = log_pb2_grpc.LogServiceStub(channel)
    resp = stub.AppendDelta(
        log_pb2.AppendDeltaRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            tenant_id=common_pb2.OrgId(value=BOOTSTRAP_TENANT_UUID),
            operation=1,
            timestamp=_make_timestamp(),
            hashed_key=bytes([0xAA] * 32),
            payload=payload,
            has_inline_data=True,
        )
    )
    channel.close()
    return resp.sequence


def _read_delta(addr: str, seq: int) -> bytes:
    """Read a delta by sequence number and return its payload."""
    channel = grpc.insecure_channel(addr)
    stub = log_pb2_grpc.LogServiceStub(channel)
    resp = stub.ReadDeltas(
        log_pb2.ReadDeltasRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            **{"from": seq, "to": seq},
        )
    )
    channel.close()
    assert len(resp.deltas) >= 1, f"no delta at seq {seq}"
    return resp.deltas[0].payload.ciphertext


# --- Tests ---


@pytest.mark.e2e
def test_all_nodes_healthy(cluster: ClusterInfo) -> None:
    """All 3 nodes accept gRPC connections and report healthy bootstrap shard."""
    for i, node in enumerate(cluster.nodes):
        channel = grpc.insecure_channel(node.data_addr)
        stub = log_pb2_grpc.LogServiceStub(channel)
        resp = stub.ShardHealth(
            log_pb2.ShardHealthRequest(
                shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            )
        )
        assert resp.info is not None, f"node {i+1} shard health returned None"
        assert resp.info.state == 1, f"node {i+1} shard not healthy: state={resp.info.state}"
        channel.close()


@pytest.mark.e2e
def test_write_to_each_node(cluster: ClusterInfo) -> None:
    """Each node can independently accept writes to the bootstrap shard."""
    for i, node in enumerate(cluster.nodes):
        payload = f"node{i+1}-write".encode()
        seq = _append_delta(node.data_addr, payload)
        assert seq >= 1, f"node {i+1} returned invalid sequence {seq}"

        # Read back from same node.
        data = _read_delta(node.data_addr, seq)
        assert data == payload, f"node {i+1} payload mismatch"


@pytest.mark.e2e
def test_node_failure_others_survive(cluster: ClusterInfo) -> None:
    """Stopping one node does not affect the others.

    Write to node 1, stop node 1, verify nodes 2 and 3 still accept
    writes and reads. Then restart node 1 and verify it recovers.
    """
    # Write to node 1 before stopping it.
    seq1 = _append_delta(cluster.nodes[0].data_addr, b"before-failure")
    assert seq1 >= 1

    # Stop node 1.
    stop_node(COMPOSE_FILE, "kiseki-node1")

    # Nodes 2 and 3 should still be healthy.
    for node in cluster.nodes[1:]:
        seq = _append_delta(node.data_addr, b"during-failure")
        assert seq >= 1, f"node at {node.data_addr} failed during node1 outage"

    # Restart node 1.
    start_node(COMPOSE_FILE, "kiseki-node1")
    _wait_for_ready(cluster.nodes[0].data_addr)

    # Node 1 should be back and accepting writes.
    seq_after = _append_delta(cluster.nodes[0].data_addr, b"after-recovery")
    assert seq_after >= 1, "node 1 failed to accept write after recovery"


@pytest.mark.e2e
def test_persistence_across_restart(cluster: ClusterInfo) -> None:
    """Data on a node survives container restart (redb persistence)."""
    # Write to node 2.
    payload = b"persist-multi-node-test"
    seq = _append_delta(cluster.nodes[1].data_addr, payload)
    assert seq >= 1

    # Restart node 2.
    root = Path(__file__).resolve().parents[2]
    subprocess.run(
        ["/usr/local/bin/docker", "compose", "-f", COMPOSE_FILE, "restart", "kiseki-node2"],
        cwd=root,
        check=True,
        capture_output=True,
    )
    _wait_for_ready(cluster.nodes[1].data_addr)

    # Read back — should survive restart via redb.
    data = _read_delta(cluster.nodes[1].data_addr, seq)
    assert data == payload, "data did not survive restart on node 2"
