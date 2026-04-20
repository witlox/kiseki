"""E2E: write a delta via LogService gRPC, read it back."""

from __future__ import annotations

import grpc
import pytest

from conftest import BOOTSTRAP_SHARD_UUID, BOOTSTRAP_TENANT_UUID
from kiseki.v1 import common_pb2, log_pb2, log_pb2_grpc


def _make_timestamp() -> common_pb2.DeltaTimestamp:
    return common_pb2.DeltaTimestamp(
        hlc=common_pb2.HybridLogicalClock(
            physical_ms=1000,
            logical=0,
            node_id=1,
        ),
        wall=common_pb2.WallTime(
            millis_since_epoch=1000,
            timezone="UTC",
        ),
        quality=1,  # NTP
    )


@pytest.mark.e2e
def test_append_and_read_delta(grpc_channel: grpc.Channel) -> None:
    """Write a delta through gRPC, read it back, verify payload roundtrip."""
    stub = log_pb2_grpc.LogServiceStub(grpc_channel)

    # Append a delta.
    append_resp = stub.AppendDelta(
        log_pb2.AppendDeltaRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            tenant_id=common_pb2.OrgId(value=BOOTSTRAP_TENANT_UUID),
            operation=1,  # Create
            timestamp=_make_timestamp(),
            hashed_key=bytes(32),
            payload=b"e2e-test-payload",
            has_inline_data=True,
        )
    )
    assert append_resp.sequence >= 1

    # Read it back.
    read_resp = stub.ReadDeltas(
        log_pb2.ReadDeltasRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            **{"from": append_resp.sequence, "to": append_resp.sequence},
        )
    )
    assert len(read_resp.deltas) == 1

    delta = read_resp.deltas[0]
    assert delta.header.sequence == append_resp.sequence
    assert delta.header.operation == 1
    assert delta.header.has_inline_data is True
    assert delta.payload.ciphertext == b"e2e-test-payload"


@pytest.mark.e2e
def test_maintenance_mode_rejects_writes(grpc_channel: grpc.Channel) -> None:
    """SetMaintenance blocks writes, clearing it allows writes again."""
    stub = log_pb2_grpc.LogServiceStub(grpc_channel)

    # Enable maintenance.
    stub.SetMaintenance(
        log_pb2.SetMaintenanceRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            enabled=True,
        )
    )

    # Write should fail with FAILED_PRECONDITION.
    with pytest.raises(grpc.RpcError) as exc_info:
        stub.AppendDelta(
            log_pb2.AppendDeltaRequest(
                shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
                tenant_id=common_pb2.OrgId(value=BOOTSTRAP_TENANT_UUID),
                operation=1,
                timestamp=_make_timestamp(),
                hashed_key=bytes(32),
                payload=b"should-fail",
                has_inline_data=False,
            )
        )
    assert exc_info.value.code() == grpc.StatusCode.FAILED_PRECONDITION

    # Clear maintenance.
    stub.SetMaintenance(
        log_pb2.SetMaintenanceRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            enabled=False,
        )
    )

    # Write should succeed now.
    resp = stub.AppendDelta(
        log_pb2.AppendDeltaRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            tenant_id=common_pb2.OrgId(value=BOOTSTRAP_TENANT_UUID),
            operation=2,  # Update
            timestamp=_make_timestamp(),
            hashed_key=bytes([0x01] * 32),
            payload=b"after-maintenance",
            has_inline_data=False,
        )
    )
    assert resp.sequence >= 1
