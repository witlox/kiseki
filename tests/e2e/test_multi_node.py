"""E2E: 3-node Raft cluster — consensus, replication, and failover.

Tests multi-node Docker compose deployment with Raft consensus.
Each node runs with a unique KISEKI_NODE_ID and shared KISEKI_RAFT_PEERS.

Phase I2: validates cross-node replication, leader election, and
failover recovery.

Requires docker compose. Run with:
    pytest tests/e2e/test_multi_node.py -v
"""

from __future__ import annotations

import subprocess
import sys
import time
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

# NodeId → host:port mapping (matches docker-compose.3node.yml port
# forwards: node1=9100, node2=9110, node3=9120).
NODE_ADDRS = {
    1: "127.0.0.1:9100",
    2: "127.0.0.1:9110",
    3: "127.0.0.1:9120",
}

# NodeId → docker compose service name.
NODE_SERVICES = {
    1: "kiseki-node1",
    2: "kiseki-node2",
    3: "kiseki-node3",
}


@pytest.fixture(scope="module")
def cluster() -> Generator[ClusterInfo, None, None]:
    """Boot the 3-node cluster and yield connection info."""
    info = start_cluster(COMPOSE_FILE)
    # Give Raft time to elect a leader (election timeout is 1.5-3s).
    time.sleep(5)
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


def _try_append_delta(addr: str, payload: bytes) -> int | None:
    """Try to append a delta. Returns sequence number or None if unavailable."""
    try:
        return _append_delta(addr, payload)
    except grpc.RpcError as e:
        if e.code() == grpc.StatusCode.UNAVAILABLE:
            return None
        raise


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


def _read_delta_safe(addr: str, seq: int) -> bytes | None:
    """Read a delta, returning None if not found or unavailable."""
    try:
        channel = grpc.insecure_channel(addr)
        stub = log_pb2_grpc.LogServiceStub(channel)
        resp = stub.ReadDeltas(
            log_pb2.ReadDeltasRequest(
                shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
                **{"from": seq, "to": seq},
            )
        )
        channel.close()
        if len(resp.deltas) == 0:
            return None
        return resp.deltas[0].payload.ciphertext
    except grpc.RpcError:
        return None


def _find_leader(nodes: list) -> tuple[int, str]:
    """Discover the current Raft leader for the bootstrap shard.

    Returns (node_id, addr) of the leader. Retries for up to 15 seconds
    to handle ongoing elections.
    """
    for attempt in range(30):
        for node in nodes:
            try:
                channel = grpc.insecure_channel(node.data_addr)
                stub = log_pb2_grpc.LogServiceStub(channel)
                resp = stub.ShardHealth(
                    log_pb2.ShardHealthRequest(
                        shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
                    )
                )
                channel.close()
                if resp.info and resp.info.leader and resp.info.leader.value > 0:
                    leader_id = resp.info.leader.value
                    leader_addr = NODE_ADDRS.get(leader_id, node.data_addr)
                    return (leader_id, leader_addr)
            except grpc.RpcError:
                continue
        time.sleep(0.5)
    pytest.fail("no leader elected within 15 seconds")


def _find_leader_with_retry(nodes: list, exclude_id: int | None = None) -> tuple[int, str]:
    """Find leader, optionally excluding a known-dead node ID."""
    for attempt in range(30):
        for node in nodes:
            try:
                channel = grpc.insecure_channel(node.data_addr)
                stub = log_pb2_grpc.LogServiceStub(channel)
                resp = stub.ShardHealth(
                    log_pb2.ShardHealthRequest(
                        shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
                    )
                )
                channel.close()
                if resp.info and resp.info.leader and resp.info.leader.value > 0:
                    leader_id = resp.info.leader.value
                    if exclude_id and leader_id == exclude_id:
                        continue
                    leader_addr = NODE_ADDRS.get(leader_id)
                    if leader_addr:
                        return (leader_id, leader_addr)
            except grpc.RpcError:
                continue
        time.sleep(0.5)
    pytest.fail("no leader elected within 15 seconds")


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
def test_leader_elected(cluster: ClusterInfo) -> None:
    """A leader is elected within the election timeout."""
    leader_id, leader_addr = _find_leader(cluster.nodes)
    assert leader_id in (1, 2, 3), f"unexpected leader id: {leader_id}"
    assert leader_addr in NODE_ADDRS.values(), f"unexpected leader addr: {leader_addr}"


@pytest.mark.e2e
def test_write_to_leader(cluster: ClusterInfo) -> None:
    """The Raft leader accepts writes to the bootstrap shard."""
    leader_id, leader_addr = _find_leader(cluster.nodes)
    payload = b"leader-write-test"
    seq = _append_delta(leader_addr, payload)
    assert seq >= 1, f"leader (node {leader_id}) returned invalid sequence {seq}"

    # Read back from leader.
    data = _read_delta(leader_addr, seq)
    assert data == payload, "leader read-back payload mismatch"


@pytest.mark.e2e
def test_cross_node_replication(cluster: ClusterInfo) -> None:
    """Write to leader, read from followers — validates Raft replication.

    This is the core Phase I2 test: a delta committed on the leader
    must be readable from all follower nodes via their state machines.
    """
    leader_id, leader_addr = _find_leader(cluster.nodes)

    # Write through the leader.
    payload = b"replicated-delta-i2"
    seq = _append_delta(leader_addr, payload)
    assert seq >= 1

    # Give replication a moment to propagate.
    time.sleep(1)

    # Read from ALL nodes (leader + followers).
    for node_id, addr in NODE_ADDRS.items():
        data = _read_delta_safe(addr, seq)
        assert data is not None, (
            f"node {node_id} ({addr}) does not have delta at seq {seq} — "
            f"replication from leader {leader_id} failed"
        )
        assert data == payload, (
            f"node {node_id} payload mismatch: expected {payload!r}, got {data!r}"
        )


@pytest.mark.e2e
def test_follower_rejects_writes(cluster: ClusterInfo) -> None:
    """Non-leader nodes reject writes with UNAVAILABLE."""
    leader_id, _ = _find_leader(cluster.nodes)

    # Find a follower.
    for node_id, addr in NODE_ADDRS.items():
        if node_id != leader_id:
            result = _try_append_delta(addr, b"follower-write")
            assert result is None, (
                f"follower node {node_id} accepted a write (expected UNAVAILABLE)"
            )
            break


@pytest.mark.e2e
def test_leader_failover(cluster: ClusterInfo) -> None:
    """Stopping the leader triggers election; new leader accepts writes.

    Stop the current leader, wait for a new leader to be elected
    among the remaining 2 nodes, verify writes succeed on the new
    leader.
    """
    leader_id, leader_addr = _find_leader(cluster.nodes)

    # Write a delta before failover.
    payload_before = b"before-failover"
    seq_before = _append_delta(leader_addr, payload_before)
    assert seq_before >= 1

    # Give replication time.
    time.sleep(1)

    # Stop the leader.
    leader_service = NODE_SERVICES[leader_id]
    stop_node(COMPOSE_FILE, leader_service)

    # Wait for new leader election (timeout is 1.5-3s + some margin).
    time.sleep(5)

    # Find the new leader among surviving nodes.
    surviving_nodes = [n for n in cluster.nodes if n.data_addr != leader_addr]
    new_leader_id, new_leader_addr = _find_leader_with_retry(surviving_nodes, exclude_id=leader_id)
    assert new_leader_id != leader_id, "old leader should not be the new leader"

    # New leader should accept writes.
    payload_after = b"after-failover"
    seq_after = _append_delta(new_leader_addr, payload_after)
    assert seq_after >= 1, "new leader failed to accept write after failover"

    # Data written before failover should still be readable on surviving nodes.
    data = _read_delta_safe(new_leader_addr, seq_before)
    assert data == payload_before, (
        f"pre-failover data lost on new leader: expected {payload_before!r}, got {data!r}"
    )

    # Restart the old leader.
    start_node(COMPOSE_FILE, leader_service)
    _wait_for_ready(leader_addr)

    # Give the restarted node time to rejoin and catch up.
    time.sleep(5)

    # The restarted node should be healthy.
    channel = grpc.insecure_channel(leader_addr)
    stub = log_pb2_grpc.LogServiceStub(channel)
    resp = stub.ShardHealth(
        log_pb2.ShardHealthRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
        )
    )
    assert resp.info is not None, "restarted node shard health returned None"
    channel.close()
