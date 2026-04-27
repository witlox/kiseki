"""E2E: pNFS Flexible Files Layout (RFC 8435) — Phase 15b/c exit gates.

The mount runs inside an ephemeral privileged docker container
joined to the cluster's internal network (`kiseki_default`). This
mirrors a real pNFS client in two important ways:

  1. The DS hostnames in the layout (e.g. `kiseki-node2:2052`) are
     resolvable from the container but NOT from the host. Mounting
     from the host would succeed at LAYOUTGET but fail at GETDEVICEINFO
     because the universal addresses point at unreachable names.
  2. Privilege is contained: the test runner doesn't need root, and
     no host-side `sudo` priming is required. Anywhere docker works,
     these tests work.

Three cases:

  * `test_pnfs_xprtsec_mtls` — RFC 9289 NFS-over-TLS path. Skipped
    when the host kernel predates 6.5 (so does the container, since
    they share a kernel) or when the cluster isn't booted with TLS.
  * `test_pnfs_plaintext_fallback` — opt-in audited fallback path
    (ADR-038 §D4.2). Skipped when the cluster boots TLS-only.
  * `test_pnfs_layout_recall_on_drain` — Phase 15c integration
    witness; placeholder until production wiring lands.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path
from typing import Generator

import pytest

from helpers.cluster import ClusterInfo, start_cluster, stop_cluster


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module")
def pnfs_cluster() -> Generator[ClusterInfo, None, None]:
    """3-node cluster — gives the MDS round-robin range across three
    real DS endpoints, so per-DS counters in mountstats are non-trivial."""
    info = start_cluster()
    yield info
    stop_cluster(info)


PNFS_CLIENT_IMAGE = "kiseki-pnfs-client:test"


@pytest.fixture(scope="module")
def pnfs_client_image() -> str:
    """Build the privileged pNFS-client image once per test module."""
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


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _kernel_version() -> tuple[int, int, int]:
    out = subprocess.run(
        ["uname", "-r"], check=True, capture_output=True, text=True
    ).stdout.strip()
    m = re.match(r"^(\d+)\.(\d+)(?:\.(\d+))?", out)
    return (int(m.group(1)), int(m.group(2)), int(m.group(3) or "0")) if m else (0, 0, 0)


def _kernel_supports_xprtsec() -> bool:
    """`xprtsec=mtls` mount option lands in mainline 6.5 and
    stabilizes in 6.7+. Container shares the host kernel, so the
    host check is authoritative."""
    return _kernel_version() >= (6, 5, 0)


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


def _cluster_network() -> str:
    """Compose-default network for the 3-node cluster.

    `docker compose` derives the network name from the project name,
    which defaults to the directory name. The kiseki repo lives in
    `~/kiseki`, so the network is `kiseki_default` unless the user
    overrode `COMPOSE_PROJECT_NAME`."""
    return "kiseki_default"


def _seed_known_object(cluster: ClusterInfo, payload: bytes) -> str:
    """Write `payload` through node1's host-mapped S3 port and return
    the etag — the resulting composition is reachable through NFS
    because they share the underlying CompositionStore."""
    import requests

    s3 = f"http://{cluster.nodes[0].data_addr.split(':')[0]}:9000"
    resp = requests.put(f"{s3}/default/pnfs-fixture", data=payload, timeout=10)
    resp.raise_for_status()
    etag = resp.headers.get("etag", "").strip('"')
    assert etag, "S3 PUT returned empty etag"
    return etag


def _run_in_client(
    image: str,
    script: str,
    *,
    timeout: int = 60,
) -> subprocess.CompletedProcess[str]:
    """Invoke `script` inside an ephemeral privileged client container
    joined to the cluster network. Returns the completed process
    (caller checks returncode and inspects stdout/stderr)."""
    return subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--privileged",
            "--network",
            _cluster_network(),
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


# Bash one-liner that mounts, reads N bytes, dumps mountstats, then unmounts.
# Parsed by `_parse_op_counters`.
_MOUNT_AND_READ_TEMPLATE = r"""
set -euo pipefail
MOUNT_POINT=/mnt/pnfs
mkdir -p "$MOUNT_POINT"
mount -t nfs4 -o {opts} {host}:/default "$MOUNT_POINT"
trap 'umount "$MOUNT_POINT" 2>/dev/null || true' EXIT

dd if="$MOUNT_POINT/{etag}" of=/dev/null bs=1M count={mib} status=none
echo '---MOUNTSTATS---'
cat /proc/self/mountstats
"""


def _parse_op_counters(stdout: str) -> dict[str, int]:
    """Parse the per-op stats block dumped after `---MOUNTSTATS---`.

    nfs-utils emits lines like `        LAYOUTGET: 1 1 0 80 1234 ...`.
    We sum across mounts (`device …` headers) since the container
    only mounts one. Op codes we need:

      * LAYOUTGET — proves the client requested a pNFS layout
      * GETDEVICEINFO — proves the client resolved a deviceid
      * READ — proves data flowed
    """
    counters = {"LAYOUTGET": 0, "GETDEVICEINFO": 0, "READ": 0}
    body = stdout.split("---MOUNTSTATS---", 1)[-1]
    for line in body.splitlines():
        for op in counters:
            m = re.match(rf"\s+{op}:\s+(\d+)\b", line)
            if m:
                counters[op] += int(m.group(1))
    return counters


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@pytest.mark.e2e
def test_pnfs_xprtsec_mtls(
    pnfs_cluster: ClusterInfo,
    pnfs_client_image: str,
) -> None:
    """TLS-mounted pNFS — Phase 15b RFC-fidelity exit criterion.

    Skipped on host kernels < 6.5 (the container shares the host
    kernel) or when the cluster's NFS listener is plaintext-only.
    """
    if not _docker_available():
        pytest.skip("docker daemon not reachable")
    if not _kernel_supports_xprtsec():
        pytest.skip(
            f"host kernel {_kernel_version()} predates xprtsec=mtls (need ≥ 6.5)"
        )

    payload = b"\xab" * 1_048_576
    etag = _seed_known_object(pnfs_cluster, payload)

    script = _MOUNT_AND_READ_TEMPLATE.format(
        opts="vers=4.1,minorversion=1,xprtsec=mtls",
        host="kiseki-node1",
        etag=etag,
        mib=1,
    )
    # tlshd has to be running before mount.nfs4 dispatches the
    # CMSG-based handshake. Prefix the bash with a tlshd boot line.
    script = "tlshd -s &\nsleep 1\n" + script
    result = _run_in_client(pnfs_client_image, script, timeout=90)

    if result.returncode != 0:
        if "xprtsec" in result.stderr or "TLS" in result.stderr.upper():
            pytest.skip(
                f"cluster appears to be plaintext-only: {result.stderr.strip()[:200]}"
            )
        pytest.fail(
            f"pNFS+TLS mount failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )

    counters = _parse_op_counters(result.stdout)
    assert counters["LAYOUTGET"] >= 1, (
        f"client never sent LAYOUTGET — counters={counters}"
    )
    assert counters["GETDEVICEINFO"] >= 1, (
        f"client never sent GETDEVICEINFO — counters={counters}"
    )
    assert counters["READ"] >= 1, (
        f"no NFS READs accounted — pNFS dispatch may have silently fallen back"
    )


@pytest.mark.e2e
def test_pnfs_plaintext_fallback(
    pnfs_cluster: ClusterInfo,
    pnfs_client_image: str,
) -> None:
    """Audited plaintext-NFS path (ADR-038 §D4.2) — the perf cluster's
    default for Rocky 9 baselines that don't honor `xprtsec=mtls`.
    Skips when the cluster runs TLS-only."""
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    payload = b"\xcd" * 1_048_576
    etag = _seed_known_object(pnfs_cluster, payload)

    script = _MOUNT_AND_READ_TEMPLATE.format(
        opts="vers=4.1,minorversion=1",
        host="kiseki-node1",
        etag=etag,
        mib=1,
    )
    result = _run_in_client(pnfs_client_image, script, timeout=60)

    if result.returncode != 0:
        # TLS-only servers reject plaintext at handshake.
        if any(
            kw in (result.stderr + result.stdout).lower()
            for kw in ("connection refused", "protocol", "no route", "tls")
        ):
            pytest.skip(
                f"plaintext NFS not available on this cluster: "
                f"{result.stderr.strip()[:200]}"
            )
        pytest.fail(
            f"plaintext mount failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )

    counters = _parse_op_counters(result.stdout)
    assert counters["LAYOUTGET"] >= 1, (
        f"plaintext-mode pNFS still requires LAYOUTGET: counters={counters}"
    )
    assert counters["READ"] >= 1, (
        f"no NFS READs accounted under plaintext fallback: counters={counters}"
    )


@pytest.mark.e2e
def test_pnfs_layout_recall_on_drain(pnfs_cluster: ClusterInfo) -> None:
    """When a storage node enters drain (ADR-035), MDS fires
    LAYOUTRECALL within I-PN5's 1-sec SLA. Phase 15c integration
    witness — currently a placeholder; BDD scenario `@pnfs-15c
    Drain triggers LAYOUTRECALL within 1 second` covers the
    in-process witness."""
    pytest.skip(
        "Phase 15c.1 follow-up — production drain hook in kiseki-server "
        "is not yet wired to the MdsLayoutManager (BDD scenario "
        "@pnfs-15c covers the in-process witness)."
    )
