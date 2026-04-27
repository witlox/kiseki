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
    """Start via docker compose. Reuses the `kiseki-server:local`
    image pinned in compose if it already exists; rebuild it
    out-of-band with `docker build -t kiseki-server:local .`."""
    root = _workspace_root()
    subprocess.run(
        ["docker", "compose", "up", "-d"],
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
        # ADR-038 §D4.2 — local subprocess has no Cluster CA bundle,
        # so the audited plaintext-NFS fallback is required for the
        # NFS path to come up at all. Production deployments wire
        # real certs.
        "KISEKI_ALLOW_PLAINTEXT_NFS": "true",
        "KISEKI_INSECURE_NFS": "true",
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


@retry(stop=stop_after_delay(180), wait=wait_exponential(multiplier=0.5, max=5))
def _wait_for_ready(addr: str) -> None:
    """Wait until the gRPC server is accepting connections.

    180s deadline tolerates GitHub-runner-class machines: docker
    compose restart + redb open + Phase 14e at-rest key derivation
    on a shared 2-vCPU runner can sit at ~60-90 s. Locally on
    NVMe + 16-core this returns in ~3 s.
    """
    channel = grpc.insecure_channel(addr)
    try:
        grpc.channel_ready_future(channel).result(timeout=2)
    finally:
        channel.close()


@retry(stop=stop_after_delay(60), wait=wait_exponential(multiplier=0.3, max=3))
def _wait_for_s3(host: str, port: int = 9000) -> None:
    """Wait until the S3 listener actually serves a real PUT.

    The kiseki-server starts gRPC, S3, and NFS listeners in sequence;
    `_wait_for_ready` only blocks on gRPC. e2e tests that PUT objects
    via S3 immediately after start_cluster() race the namespace
    bootstrap — the 9000 socket is up and accepting connections, but
    early PUTs return 500 because the namespace isn't materialized
    yet. We poll with a real PUT (a discardable health-probe key) and
    retry until we get a 2xx; that proves the gateway is fully wired.
    """
    import requests

    resp = requests.put(
        f"http://{host}:{port}/default/__cluster_ready_probe__",
        data=b"ready",
        timeout=3,
    )
    resp.raise_for_status()


@retry(stop=stop_after_delay(60), wait=wait_exponential(multiplier=0.3, max=3))
def _wait_for_nfs(host: str, port: int = 2049) -> None:
    """Wait until the NFS listener accepts a TCP connection.

    The NFSv4/v3/MOUNT3 dispatcher binds last in the kiseki-server
    runtime startup sequence (after gRPC and S3). e2e tests that
    `mount -t nfs4` immediately after `_wait_for_ready` can race the
    NFS spawn and surface as `Connection refused`. A simple TCP
    connect is sufficient — the dispatcher accepts before reading
    the first RPC, and any malformed first message just closes the
    connection without state leaks.
    """
    import socket

    with socket.create_connection((host, port), timeout=2):
        pass


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
    # Stop any single-node compose that may be running (port conflicts).
    subprocess.run(
        ["docker", "compose", "down", "-v"],
        cwd=root,
        capture_output=True,
    )
    result = subprocess.run(
        ["docker", "compose", "-f", compose_file, "up", "-d"],
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

    # Phase 15c.5/B-4 — block on the user-facing data planes of node1
    # so a fresh `start_cluster()` is safe to PUT and `mount.nfs`
    # against immediately on return. _wait_for_ready only checks the
    # gRPC port; S3 and NFS bind later in the startup sequence and
    # have their own readiness criteria (S3 needs the bootstrap
    # namespace materialized; NFS just needs the dispatcher bound).
    host = nodes[0].data_addr.split(":")[0]
    _wait_for_s3(host, 9000)
    _wait_for_nfs(host, 2049)

    return ClusterInfo(nodes=nodes, compose_file=compose_file, mode="docker")


def stop_cluster(info: ClusterInfo) -> None:
    """Stop a multi-node cluster — idempotent.

    `docker compose down -v` occasionally fails with
    `Network ... Resource is still in use` if a privileged client
    container from the prior test is mid-teardown. We ignore the
    return code (the volumes-remove step has already started) and
    fall through to a `network rm` cleanup so the next start_cluster
    can recreate it without a port collision.
    """
    root = _workspace_root()
    subprocess.run(
        ["docker", "compose", "-f", info.compose_file, "down", "-v"],
        cwd=root,
        capture_output=True,
        check=False,
    )
    # Best-effort orphan-network cleanup. compose creates
    # `<project>_default` (e.g. `kiseki_default`); when a transient
    # container holds it open, the network survives `down -v`.
    subprocess.run(
        ["docker", "network", "rm", "kiseki_default"],
        capture_output=True,
        check=False,
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
