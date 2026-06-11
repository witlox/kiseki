"""Fixtures for kiseki e2e tests."""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Generator

import grpc
import pytest

# Add proto output to path.
sys.path.insert(0, str(Path(__file__).parent / "proto"))

from helpers.budgets import (  # noqa: E402, F401 — re-exported for tests
    BUDGET_SCALE,
    GRPC_TIMEOUT,
    S3_LARGE_TIMEOUT,
    S3_TIMEOUT,
    scaled,
)
from helpers.cluster import ServerInfo, start_server, stop_server  # noqa: E402

# ---------------------------------------------------------------------------
# Runner-class gating (release run 27322282644)
#
# Some tests need infrastructure a standard 2-vCPU GitHub runner cannot
# provide reliably: FUSE daemons inside privileged docker containers and
# kernel NFS/pNFS mounts under a 3-node compose. Those are gated — NOT
# budget-scaled — because no timeout makes the infrastructure feasible
# there. release.yml sets E2E_RUNNER_CLASS=ci-small for its e2e job;
# dev boxes default to "dev" and run everything (the suite remains the
# project's ground truth on dev boxes).
# ---------------------------------------------------------------------------
E2E_RUNNER_CLASS = os.environ.get("E2E_RUNNER_CLASS", "dev")


def skip_on_ci_small(reason: str) -> pytest.MarkDecorator:
    """Explicit infrastructure gate for small CI runners.

    This is a runner-class gate, not a semantic skip: the test stays
    fully runnable (and required) on dev boxes.
    """
    return pytest.mark.skipif(
        E2E_RUNNER_CLASS == "ci-small",
        reason=f"{reason} — infeasible on small CI runners; dev-box "
        "ground truth (see release run 27322282644)",
    )


@pytest.fixture(scope="session")
def kiseki_server() -> Generator[ServerInfo, None, None]:
    """Boot the kiseki stack (docker compose or local) and yield connection info."""
    info = start_server()
    yield info
    stop_server(info)


@pytest.fixture(scope="session")
def grpc_channel(kiseki_server: ServerInfo) -> Generator[grpc.Channel, None, None]:
    """Shared gRPC channel to the data-path server."""
    channel = grpc.insecure_channel(kiseki_server.data_addr)
    yield channel
    channel.close()


# Well-known bootstrap IDs (must match runtime.rs bootstrap).
BOOTSTRAP_SHARD_UUID = "00000000-0000-0000-0000-000000000001"
BOOTSTRAP_TENANT_UUID = "00000000-0000-0000-0000-000000000001"
