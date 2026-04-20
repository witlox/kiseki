"""E2E: persistence — data survives server restart via Docker."""

from __future__ import annotations

import subprocess
import time
from pathlib import Path

import grpc
import pytest

from conftest import BOOTSTRAP_SHARD_UUID, BOOTSTRAP_TENANT_UUID
from helpers.cluster import ServerInfo, _wait_for_ready
from kiseki.v1 import common_pb2, log_pb2, log_pb2_grpc


WORKSPACE = Path(__file__).resolve().parents[2]


def _make_timestamp() -> common_pb2.DeltaTimestamp:
    return common_pb2.DeltaTimestamp(
        hlc=common_pb2.HybridLogicalClock(physical_ms=3000, logical=0, node_id=1),
        wall=common_pb2.WallTime(millis_since_epoch=3000, timezone="UTC"),
        quality=1,
    )


@pytest.mark.e2e
def test_delta_survives_restart(kiseki_server: ServerInfo) -> None:
    """Write a delta, restart the server, read the delta back."""
    channel = grpc.insecure_channel(kiseki_server.data_addr)
    stub = log_pb2_grpc.LogServiceStub(channel)

    # Write a delta with a unique payload.
    payload = b"persistence-test-unique-payload"
    resp = stub.AppendDelta(
        log_pb2.AppendDeltaRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            tenant_id=common_pb2.OrgId(value=BOOTSTRAP_TENANT_UUID),
            operation=1,
            timestamp=_make_timestamp(),
            hashed_key=bytes([0xDD] * 32),
            payload=payload,
            has_inline_data=True,
        )
    )
    seq = resp.sequence
    assert seq >= 1
    channel.close()

    # Restart the server container.
    subprocess.run(
        ["/usr/local/bin/docker", "compose", "restart", "kiseki-server"],
        cwd=WORKSPACE,
        check=True,
        capture_output=True,
    )

    # Wait for server to come back up.
    _wait_for_ready(kiseki_server.data_addr)

    # Read the delta back — should survive restart.
    channel2 = grpc.insecure_channel(kiseki_server.data_addr)
    stub2 = log_pb2_grpc.LogServiceStub(channel2)

    read_resp = stub2.ReadDeltas(
        log_pb2.ReadDeltasRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            **{"from": seq, "to": seq},
        )
    )

    # With persistence enabled (KISEKI_DATA_DIR=/data), the delta should survive.
    # With in-memory only, this will fail — which is expected without KISEKI_DATA_DIR.
    assert len(read_resp.deltas) >= 1, (
        f"delta at seq {seq} not found after restart — "
        "persistence may not be enabled (KISEKI_DATA_DIR required)"
    )
    assert read_resp.deltas[0].payload.ciphertext == payload
    channel2.close()
