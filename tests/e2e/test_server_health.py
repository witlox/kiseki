"""E2E: verify the server boots and responds to health checks."""

from __future__ import annotations

import grpc
import pytest

from conftest import BOOTSTRAP_SHARD_UUID
from kiseki.v1 import common_pb2, key_pb2, key_pb2_grpc, log_pb2, log_pb2_grpc


@pytest.mark.e2e
def test_keymanager_health(grpc_channel: grpc.Channel) -> None:
    """KeyManagerService.Health returns a valid epoch."""
    stub = key_pb2_grpc.KeyManagerServiceStub(grpc_channel)
    resp = stub.Health(key_pb2.KeyManagerHealthRequest())

    assert resp.healthy is True
    assert resp.current_epoch.value >= 1


@pytest.mark.e2e
def test_shard_health(grpc_channel: grpc.Channel) -> None:
    """LogService.ShardHealth returns the bootstrap shard as healthy."""
    stub = log_pb2_grpc.LogServiceStub(grpc_channel)
    resp = stub.ShardHealth(
        log_pb2.ShardHealthRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
        )
    )

    assert resp.info.state == 1  # SHARD_STATE_HEALTHY
    assert resp.info.tip >= 0  # tip advances as other tests write deltas
