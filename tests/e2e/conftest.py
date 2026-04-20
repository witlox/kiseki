"""Fixtures for kiseki e2e tests."""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Generator

import grpc
import pytest

# Add proto output to path.
sys.path.insert(0, str(Path(__file__).parent / "proto"))

from helpers.cluster import ServerInfo, start_server, stop_server  # noqa: E402


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
