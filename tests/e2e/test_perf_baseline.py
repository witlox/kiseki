"""B-5 perf baseline — fio numbers for the 4-surface client matrix.

This is a *baseline*, not a regression gate. The goal is honest
quantitative numbers we can report next to the protocol-correctness
matrix:

   * S3 PUT/GET throughput (single-stream, single-node)
   * NFSv4.1 plain mount + sequential read (vs the same payload via S3)
   * NFSv3 mount + sequential read
   * FUSE → cluster (RemoteHttpGateway) sequential read

NOT a regression gate (no thresholds): perf depends on host hardware,
docker overhead, kernel version, and the cgroup limits applied to the
privileged container. The point is to make the numbers visible so we
catch order-of-magnitude regressions in code review and so the deferred
work (Phase 15c.5 pNFS Flex Files) has a "before" datapoint to
improve against.

The test marks itself with `slow` so default `pytest -m e2e` runs
skip it; opt in with `pytest -m perf`.
"""

from __future__ import annotations

import re
import subprocess
import time
from typing import Generator

import pytest
import requests

from helpers.cluster import ClusterInfo, start_cluster, stop_cluster


@pytest.fixture(scope="module")
def perf_cluster() -> Generator[ClusterInfo, None, None]:
    info = start_cluster()
    yield info
    stop_cluster(info)


PNFS_CLIENT_IMAGE = "kiseki-pnfs-client:test"


@pytest.fixture(scope="module")
def perf_client_image() -> str:
    """The standard pNFS-client image (already has fio after the
    Dockerfile.pnfs-client update for B-5)."""
    from pathlib import Path

    repo_root = Path(__file__).resolve().parents[2]
    dockerfile = Path(__file__).parent / "Dockerfile.pnfs-client"
    subprocess.run(
        [
            "docker",
            "build",
            "-t",
            PNFS_CLIENT_IMAGE,
            "-f",
            str(dockerfile),
            str(repo_root),
        ],
        check=True,
        capture_output=True,
    )
    return PNFS_CLIENT_IMAGE


def _docker_available() -> bool:
    return (
        subprocess.run(
            ["docker", "version", "--format", "{{.Server.Version}}"],
            check=False,
            capture_output=True,
            timeout=5,
        ).returncode
        == 0
    )


def _run_in_client(
    image: str,
    script: str,
    *,
    timeout: int = 120,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--privileged",
            "--network",
            "kiseki_default",
            "--cap-add",
            "SYS_ADMIN",
            "--cap-add",
            "DAC_READ_SEARCH",
            image,
            script,
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _seed_object(cluster: ClusterInfo, key: str, payload: bytes) -> str:
    s3 = f"http://{cluster.nodes[0].data_addr.split(':')[0]}:9000"
    resp = requests.put(f"{s3}/default/{key}", data=payload, timeout=15)
    resp.raise_for_status()
    etag = resp.headers.get("etag", "").strip('"')
    assert etag, "S3 PUT returned empty etag"
    return etag


def _parse_fio_bw(stdout: str) -> dict[str, float]:
    """Pull `bw=` (bandwidth) from a fio summary line.

    fio output formats vary; the canonical "Run status group" summary
    line is the most reliable (it's the steady-state mean across the
    whole run, not a per-sample bw=). Two example shapes:

       READ: bw=120MiB/s (126MB/s), 120MiB/s-120MiB/s ...
       READ: bw=64.2MiB/s (67.3MB/s), 64.2MiB/s-64.2MiB/s ...

    We prefer the parenthesized `(126MB/s)` form because it's already
    in MB/s (10^6 bytes/s, what fio calls "decimal" units) — matches
    standard storage marketing units and avoids MiB→MB conversion
    arithmetic.
    """
    out: dict[str, float] = {}
    for direction in ("READ", "WRITE"):
        # Try the parenthesized "(126MB/s)" form first.
        m = re.search(
            rf"{direction}:\s+bw=[^(]+\(([\d.]+)([KMG]B)/s\)",
            stdout,
        )
        if not m:
            # Fall back to the leading "bw=120MiB/s" form.
            m = re.search(rf"{direction}:\s+bw=([\d.]+)([KMG]i?B)/s", stdout)
        if m:
            n = float(m.group(1))
            unit = m.group(2)
            scale = {
                "KiB": 1 / 1024,
                "MiB": 1,
                "GiB": 1024,
                "KB": 1 / 1000,
                "MB": 1,
                "GB": 1000,
            }.get(unit, 1)
            out[direction.lower() + "_mbps"] = n * scale
    return out


# ---------------------------------------------------------------------------
# 1. S3 throughput — direct HTTP, no NFS in the loop.
# ---------------------------------------------------------------------------


@pytest.mark.e2e
@pytest.mark.perf
def test_perf_s3_put_get_throughput(perf_cluster: ClusterInfo) -> None:
    """S3 PUT + GET of an 8 MiB object measured wall-clock from the
    test runner. Reports MB/s in stdout for the next-tier perf
    monitoring system to scrape; does NOT assert any threshold."""
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    payload = b"\xb5" * (8 * 1024 * 1024)  # 8 MiB
    s3 = f"http://{perf_cluster.nodes[0].data_addr.split(':')[0]}:9000"

    t0 = time.monotonic()
    put = requests.put(f"{s3}/default/perf-fixture", data=payload, timeout=30)
    put.raise_for_status()
    put_dur = time.monotonic() - t0
    etag = put.headers.get("etag", "").strip('"')

    t0 = time.monotonic()
    get = requests.get(f"{s3}/default/{etag}", timeout=30)
    get.raise_for_status()
    get_dur = time.monotonic() - t0

    assert get.content == payload
    put_mbps = (len(payload) / 1_000_000) / put_dur if put_dur > 0 else 0.0
    get_mbps = (len(payload) / 1_000_000) / get_dur if get_dur > 0 else 0.0
    print(
        f"\n[B-5/S3] PUT {put_mbps:7.1f} MB/s ({put_dur*1000:.0f} ms)  "
        f"GET {get_mbps:7.1f} MB/s ({get_dur*1000:.0f} ms)"
    )


# ---------------------------------------------------------------------------
# 2. NFSv4.1 plain mount sequential read — fio.
# ---------------------------------------------------------------------------


@pytest.mark.e2e
@pytest.mark.perf
def test_perf_nfs41_seq_read(
    perf_cluster: ClusterInfo,
    perf_client_image: str,
) -> None:
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    # 8 MiB fixture — large enough to cover 8 NFS-bs=1M reads (kernel
    # readahead defaults to 4 MiB, so 8 MiB ensures fio's first
    # samples include a couple of cold misses + several cache hits).
    # 32 MiB stretched fio's 10s budget thin under cold-cache and
    # masked the steady-state throughput we actually want to measure.
    payload = b"\xa5" * (8 * 1024 * 1024)
    etag = _seed_object(perf_cluster, "perf-nfs41", payload)

    script = rf"""
set -euo pipefail
mkdir -p /mnt/pnfs
mount -t nfs4 -o vers=4.1,minorversion=1 kiseki-node1:/default /mnt/pnfs
trap 'umount /mnt/pnfs 2>/dev/null || true' EXIT
# Warm-up read populates the server-side decrypt cache (Phase 15c.5).
# Without this, fio's first sample includes cold-decrypt cost which
# dominates the time-based mean and obscures steady-state throughput.
dd if=/mnt/pnfs/{etag} of=/dev/null bs=1M status=none
fio --name=seq-read --rw=read --direct=0 --bs=1M --size=8M \
    --filename=/mnt/pnfs/{etag} --runtime=10 --time_based \
    --output-format=normal 2>&1 | tail -30
"""
    result = _run_in_client(perf_client_image, script, timeout=180)
    if result.returncode != 0:
        pytest.fail(
            f"fio NFSv4.1 seq-read failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )
    bw = _parse_fio_bw(result.stdout)
    print(
        f"\n[B-5/NFSv4.1] seq-read = {bw.get('read_mbps', 0):7.1f} MB/s"
    )


# ---------------------------------------------------------------------------
# 3. NFSv3 mount sequential read — fio.
# ---------------------------------------------------------------------------


@pytest.mark.e2e
@pytest.mark.perf
def test_perf_nfs3_seq_read(
    perf_cluster: ClusterInfo,
    perf_client_image: str,
) -> None:
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    payload = b"\xa6" * (8 * 1024 * 1024)
    etag = _seed_object(perf_cluster, "perf-nfs3", payload)

    script = rf"""
set -euo pipefail
mkdir -p /mnt/nfs3
mount -t nfs -o vers=3,proto=tcp,port=2049,mountport=2049,mountproto=tcp,nolock \
    kiseki-node1:/default /mnt/nfs3
trap 'umount /mnt/nfs3 2>/dev/null || true' EXIT
dd if=/mnt/nfs3/{etag} of=/dev/null bs=1M status=none
fio --name=seq-read --rw=read --direct=0 --bs=1M --size=8M \
    --filename=/mnt/nfs3/{etag} --runtime=10 --time_based \
    --output-format=normal 2>&1 | tail -30
"""
    result = _run_in_client(perf_client_image, script, timeout=180)
    if result.returncode != 0:
        pytest.fail(
            f"fio NFSv3 seq-read failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )
    bw = _parse_fio_bw(result.stdout)
    print(
        f"\n[B-5/NFSv3] seq-read = {bw.get('read_mbps', 0):7.1f} MB/s"
    )


# ---------------------------------------------------------------------------
# 4-6. WRITE perf — the symmetric case to the read tests above.
# ---------------------------------------------------------------------------
#
# fio --rw=write writes a fresh file (or overwrites an existing one)
# at the requested bs. For NFS this exercises the WRITE op + COMMIT.
# Linux 6.x derives wsize from FATTR4_MAXWRITE (NFSv4) / FSINFO wtmax
# (NFSv3); both are advertised at 1 MiB so a 1M block size lands in
# single-RPC writes.


@pytest.mark.e2e
@pytest.mark.perf
def test_perf_nfs41_seq_write(
    perf_cluster: ClusterInfo,
    perf_client_image: str,
) -> None:
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    script = r"""
set -euo pipefail
mkdir -p /mnt/pnfs
mount -t nfs4 -o vers=4.1,minorversion=1 kiseki-node1:/default /mnt/pnfs
trap 'umount /mnt/pnfs 2>/dev/null || true' EXIT
fio --name=seq-write --rw=write --direct=0 --bs=1M --size=8M \
    --filename=/mnt/pnfs/perf-write-nfs41 --runtime=10 --time_based \
    --output-format=normal 2>&1 | tail -30
"""
    result = _run_in_client(perf_client_image, script, timeout=180)
    if result.returncode != 0:
        pytest.fail(
            "fio NFSv4.1 seq-write failed "
            f"(rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )
    bw = _parse_fio_bw(result.stdout)
    print(f"\n[B-5/NFSv4.1] seq-write = {bw.get('write_mbps', 0):7.1f} MB/s")


@pytest.mark.e2e
@pytest.mark.perf
def test_perf_nfs3_seq_write(
    perf_cluster: ClusterInfo,
    perf_client_image: str,
) -> None:
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    script = r"""
set -euo pipefail
mkdir -p /mnt/nfs3
mount -t nfs -o vers=3,proto=tcp,port=2049,mountport=2049,mountproto=tcp,nolock \
    kiseki-node1:/default /mnt/nfs3
trap 'umount /mnt/nfs3 2>/dev/null || true' EXIT
fio --name=seq-write --rw=write --direct=0 --bs=1M --size=8M \
    --filename=/mnt/nfs3/perf-write-nfs3 --runtime=10 --time_based \
    --output-format=normal 2>&1 | tail -30
"""
    result = _run_in_client(perf_client_image, script, timeout=180)
    if result.returncode != 0:
        pytest.fail(
            "fio NFSv3 seq-write failed "
            f"(rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )
    bw = _parse_fio_bw(result.stdout)
    print(f"\n[B-5/NFSv3] seq-write = {bw.get('write_mbps', 0):7.1f} MB/s")

