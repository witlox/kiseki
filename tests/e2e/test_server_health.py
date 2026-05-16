"""E2E: verify the server boots and responds to health checks.

These were tautological pre-2026-05-16 — `assert resp.healthy is True`
and `assert resp.info.tip >= 0` both pass for ANY decoded protobuf
response (the fields default to those values). The current versions
exercise the *progression* of state across calls and the binding
between key epoch and shard tip.
"""

from __future__ import annotations

import grpc
import pytest

from conftest import BOOTSTRAP_SHARD_UUID
from kiseki.v1 import (
    common_pb2,
    key_pb2,
    key_pb2_grpc,
    log_pb2,
    log_pb2_grpc,
)


def _make_timestamp() -> log_pb2.DeltaTimestamp:
    # Match test_log_roundtrip's helper — a minimal valid HLC.
    return log_pb2.DeltaTimestamp(
        hlc=log_pb2.HybridLogicalClock(physical_ms=0, logical=0, node_id=1),
        wall=log_pb2.WallTime(millis_since_epoch=0, timezone="UTC"),
        quality=1,  # NTP
    )


@pytest.mark.e2e
def test_keymanager_health(grpc_channel: grpc.Channel) -> None:
    """Health endpoint: `healthy` flag MUST be True AND `current_epoch`
    MUST be the same across calls within a stable server lifetime
    (it only changes via explicit rotation, which we don't trigger).

    Pre-fix: `assert resp.healthy is True` passes for any decoded
    proto; `assert resp.current_epoch.value >= 1` passes if the field
    is set at all (default value satisfies `>= 1` after first boot).
    """
    stub = key_pb2_grpc.KeyManagerServiceStub(grpc_channel)
    resp1 = stub.Health(key_pb2.KeyManagerHealthRequest())
    resp2 = stub.Health(key_pb2.KeyManagerHealthRequest())

    assert resp1.healthy is True, f"server reports unhealthy: {resp1}"
    assert resp1.current_epoch.value >= 1, (
        f"current_epoch={resp1.current_epoch.value}; expected ≥1 after boot"
    )
    # Stability: the epoch does NOT advance between adjacent reads.
    # If it does, either a rotation is firing on its own (broken under
    # test) or the response is non-deterministic.
    assert resp1.current_epoch.value == resp2.current_epoch.value, (
        f"key epoch changed between two adjacent Health calls "
        f"({resp1.current_epoch.value} → {resp2.current_epoch.value}); "
        "no rotation should be in flight under test"
    )


@pytest.mark.e2e
def test_shard_health(grpc_channel: grpc.Channel) -> None:
    """ShardHealth: tip MUST advance after a write goes through, and
    the post-write tip MUST be exactly one more than the pre-write tip
    (single-shard, single-write).

    Pre-fix: `assert resp.info.tip >= 0` passes vacuously (u64 ≥ 0
    always).
    """
    log_stub = log_pb2_grpc.LogServiceStub(grpc_channel)
    shard_req = log_pb2.ShardHealthRequest(
        shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
    )

    before = log_stub.ShardHealth(shard_req)
    assert before.info.state == 1, (
        f"shard state={before.info.state}; expected 1 (SHARD_STATE_HEALTHY)"
    )
    tip_before = before.info.tip

    # Append one delta and assert the tip moves by exactly one.
    append_resp = log_stub.AppendDelta(
        log_pb2.AppendDeltaRequest(
            shard_id=common_pb2.ShardId(value=BOOTSTRAP_SHARD_UUID),
            tenant_id=common_pb2.OrgId(value=BOOTSTRAP_SHARD_UUID),
            operation=1,  # Create
            timestamp=_make_timestamp(),
            hashed_key=bytes(32),
            payload=b"shard-health-tip-probe",
            has_inline_data=True,
        )
    )
    assert append_resp.sequence >= 1, (
        f"AppendDelta returned sequence={append_resp.sequence}; expected ≥1"
    )

    after = log_stub.ShardHealth(shard_req)
    assert after.info.state == 1, "shard left HEALTHY state after a single append"
    assert after.info.tip == tip_before + 1, (
        f"tip did not advance by exactly 1: before={tip_before}, "
        f"after={after.info.tip} (delta={after.info.tip - tip_before}). "
        "Either ShardHealth lags AppendDelta's commit, or AppendDelta "
        "fanned out multiple log records for one logical delta."
    )
