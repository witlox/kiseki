"""Server lifecycle management for e2e tests.

Supports two modes:
- Docker Compose (default): `docker compose up --build -d`, connect on localhost ports
- Local subprocess: spawn kiseki-server directly (requires FIPS dylib on PATH)

Set KISEKI_E2E_MODE=local to use local subprocess mode.
"""

from __future__ import annotations

import os
import signal
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

import grpc
from tenacity import retry, stop_after_delay, wait_exponential


@dataclass
class ServerInfo:
    """Running server connection info."""

    data_addr: str
    advisory_addr: str
    control_addr: str
    mode: str  # "docker" or "local"
    _process: subprocess.Popen[bytes] | None = None


@dataclass
class ClusterInfo:
    """Running multi-node cluster connection info."""

    nodes: list[ServerInfo]
    compose_file: str
    mode: str  # "docker"


def _workspace_root() -> Path:
    return Path(__file__).resolve().parents[3]


def start_server(
    data_port: int = 9100,
    advisory_port: int = 9101,
    control_port: int = 9200,
) -> ServerInfo:
    """Start the server stack. Uses docker compose by default."""
    mode = os.environ.get("KISEKI_E2E_MODE", "docker")
    if mode == "local":
        return _start_local(data_port, advisory_port)
    return _start_docker(data_port, advisory_port, control_port)


def _start_docker(
    data_port: int, advisory_port: int, control_port: int
) -> ServerInfo:
    """Start via docker compose."""
    root = _workspace_root()
    subprocess.run(
        ["docker", "compose", "up", "--build", "-d"],
        cwd=root,
        check=True,
        capture_output=True,
    )

    info = ServerInfo(
        data_addr=f"127.0.0.1:{data_port}",
        advisory_addr=f"127.0.0.1:{advisory_port}",
        control_addr=f"127.0.0.1:{control_port}",
        mode="docker",
    )

    _wait_for_ready(info.data_addr)
    return info


def _start_local(data_port: int, advisory_port: int) -> ServerInfo:
    """Start a local kiseki-server subprocess."""
    root = _workspace_root()
    binary = root / "target" / "debug" / "kiseki-server"
    if not binary.exists():
        subprocess.run(
            ["cargo", "build", "-p", "kiseki-server"],
            cwd=root,
            check=True,
            capture_output=True,
        )

    data_addr = f"127.0.0.1:{data_port}"
    advisory_addr = f"127.0.0.1:{advisory_port}"

    env = {
        **os.environ,
        "KISEKI_DATA_ADDR": data_addr,
        "KISEKI_ADVISORY_ADDR": advisory_addr,
        "KISEKI_BOOTSTRAP": "true",
    }

    process = subprocess.Popen(
        [str(binary)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    info = ServerInfo(
        data_addr=data_addr,
        advisory_addr=advisory_addr,
        control_addr="",
        mode="local",
        _process=process,
    )

    _wait_for_ready(data_addr)
    return info


@retry(stop=stop_after_delay(60), wait=wait_exponential(multiplier=0.5, max=5))
def _wait_for_ready(addr: str) -> None:
    """Wait until the gRPC server is accepting connections."""
    channel = grpc.insecure_channel(addr)
    try:
        grpc.channel_ready_future(channel).result(timeout=2)
    finally:
        channel.close()


def stop_server(info: ServerInfo) -> None:
    """Stop the server stack."""
    if info.mode == "docker":
        root = _workspace_root()
        subprocess.run(
            ["docker", "compose", "down"],
            cwd=root,
            capture_output=True,
        )
    elif info._process is not None and info._process.poll() is None:
        info._process.send_signal(signal.SIGTERM)
        try:
            info._process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            info._process.kill()
            info._process.wait()


def start_cluster(compose_file: str = "docker-compose.3node.yml") -> ClusterInfo:
    """Start a multi-node cluster via docker compose."""
    root = _workspace_root()
    result = subprocess.run(
        ["docker", "compose", "-f", compose_file, "up", "--build", "-d"],
        cwd=root,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(f"docker compose build failed:\nstdout: {result.stdout[-2000:]}\nstderr: {result.stderr[-2000:]}")
        result.check_returncode()

    # Node port mappings: node1=9100, node2=9110, node3=9120.
    nodes = [
        ServerInfo(
            data_addr="127.0.0.1:9100",
            advisory_addr="127.0.0.1:9101",
            control_addr="",
            mode="docker",
        ),
        ServerInfo(
            data_addr="127.0.0.1:9110",
            advisory_addr="127.0.0.1:9111",
            control_addr="",
            mode="docker",
        ),
        ServerInfo(
            data_addr="127.0.0.1:9120",
            advisory_addr="127.0.0.1:9121",
            control_addr="",
            mode="docker",
        ),
    ]

    for node in nodes:
        _wait_for_ready(node.data_addr)

    return ClusterInfo(nodes=nodes, compose_file=compose_file, mode="docker")


def stop_cluster(info: ClusterInfo) -> None:
    """Stop a multi-node cluster."""
    root = _workspace_root()
    subprocess.run(
        ["docker", "compose", "-f", info.compose_file, "down", "-v"],
        cwd=root,
        capture_output=True,
    )


def stop_node(compose_file: str, service_name: str) -> None:
    """Stop a single node in the cluster."""
    root = _workspace_root()
    subprocess.run(
        ["docker", "compose", "-f", compose_file, "stop", service_name],
        cwd=root,
        check=True,
        capture_output=True,
    )


def start_node(compose_file: str, service_name: str) -> None:
    """Start a previously stopped node."""
    root = _workspace_root()
    subprocess.run(
        ["docker", "compose", "-f", compose_file, "start", service_name],
        cwd=root,
        check=True,
        capture_output=True,
    )
