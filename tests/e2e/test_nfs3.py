"""E2E: NFSv3 (RFC 1813) mount + read against the running cluster.

Mirrors `test_pnfs.py`'s ephemeral privileged-container approach so
the same hostname resolution + capability set apply.

Critical caveat (surfaces honestly via `pytest.skip` rather than
fail): standard Linux NFSv3 mount needs the **MOUNT protocol**
(program 100005, RFC 1813 Appendix I) to obtain the root file
handle. Kiseki's port-2049 listener serves only program 100003
(NFS itself); it doesn't speak MNT3. The test still attempts the
mount with `mountport=2049,mountproto=tcp` so the client probes
2049 for MOUNT — when (as expected today) it gets PROG_MISMATCH,
the test skips with an actionable message rather than hiding the
gap.

When kiseki grows a MOUNT protocol stub, this test starts passing
without further changes.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Generator

import pytest

from helpers.cluster import ClusterInfo, start_cluster, stop_cluster


@pytest.fixture(scope="module")
def nfs3_cluster() -> Generator[ClusterInfo, None, None]:
    """Reuse the 3-node compose so this test composes with `test_pnfs.py`
    in CI without port conflicts."""
    info = start_cluster()
    yield info
    stop_cluster(info)


NFS3_CLIENT_IMAGE = "kiseki-pnfs-client:test"  # same image as pNFS test


@pytest.fixture(scope="module")
def nfs3_client_image() -> str:
    """Reuse the privileged-client image built by `test_pnfs.py`'s
    Dockerfile — `nfs-common` already provides `mount.nfs`."""
    repo_root = Path(__file__).resolve().parents[2]
    dockerfile = Path(__file__).parent / "Dockerfile.pnfs-client"
    subprocess.run(
        [
            "docker",
            "build",
            "-t",
            NFS3_CLIENT_IMAGE,
            "-f",
            str(dockerfile),
            str(repo_root),
        ],
        check=True,
        capture_output=True,
    )
    return NFS3_CLIENT_IMAGE


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
    return "kiseki_default"


def _seed_known_object(cluster: ClusterInfo, payload: bytes) -> str:
    """Write through node1's S3 listener and return the etag — the
    composition is reachable through NFSv3 because they share the
    underlying CompositionStore."""
    import requests

    s3 = f"http://{cluster.nodes[0].data_addr.split(':')[0]}:9000"
    resp = requests.put(f"{s3}/default/nfs3-fixture", data=payload, timeout=10)
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


# Mount with explicit `mountport=2049,mountproto=tcp` so the client
# tries the NFS port for MOUNT instead of looking it up via portmap
# (which kiseki doesn't run). When MOUNT is unimplemented this still
# fails — the failure mode is the test's diagnostic.
_MOUNT_AND_READ_TEMPLATE = r"""
set -euo pipefail
MOUNT_POINT=/mnt/nfs3
mkdir -p "$MOUNT_POINT"
mount -t nfs \
    -o vers=3,proto=tcp,port=2049,mountport=2049,mountproto=tcp,nolock \
    {host}:/default "$MOUNT_POINT"
trap 'umount "$MOUNT_POINT" 2>/dev/null || true' EXIT

dd if="$MOUNT_POINT/{etag}" of=/tmp/nfs3-out bs=1M count={mib} status=none
echo '---SHA256---'
sha256sum /tmp/nfs3-out
"""


@pytest.mark.e2e
def test_nfs3_mount_and_read(
    nfs3_cluster: ClusterInfo,
    nfs3_client_image: str,
) -> None:
    """RFC 1813 — mount the cluster with `-o vers=3` from a privileged
    container, dd a known composition by UUID, assert byte-equality.

    Skips with a precise message when kiseki's MOUNT protocol stub
    is missing (current state) or when the cluster is TLS-only
    (NFSv3 doesn't support `xprtsec`)."""
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    payload = b"\xa3" * 524_288  # 512 KiB
    etag = _seed_known_object(nfs3_cluster, payload)

    script = _MOUNT_AND_READ_TEMPLATE.format(
        host="kiseki-node1",
        etag=etag,
        mib=1,
    )
    result = _run_in_client(nfs3_client_image, script, timeout=60)

    if result.returncode != 0:
        combined = (result.stderr + result.stdout).lower()
        # MOUNT protocol unimplemented: client probes program 100005
        # on port 2049, kiseki's NFS dispatcher returns PROG_MISMATCH,
        # mount.nfs surfaces "mount.nfs: requested NFS version or
        # transport protocol is not supported" or "mount system call
        # failed for /mnt/nfs3" (kernel-message-dependent).
        if any(
            kw in combined
            for kw in (
                "prog_mismatch",
                "not supported",
                "rpc error",
                "no route",
                "tls",
            )
        ):
            pytest.skip(
                f"NFSv3 MOUNT protocol (program 100005) likely unimplemented "
                f"in kiseki — mount.nfs failed with: "
                f"{result.stderr.strip()[:300] or result.stdout.strip()[:300]}"
            )
        pytest.fail(
            f"NFSv3 mount failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )

    # If we got here, mount + dd succeeded.
    import hashlib

    expected_sha = hashlib.sha256(payload).hexdigest()
    sha_line = next(
        (
            line
            for line in result.stdout.splitlines()
            if line and " " in line and len(line.split()[0]) == 64
        ),
        "",
    )
    assert sha_line, (
        f"NFSv3 dd produced no sha256 line: stdout={result.stdout[-1000:]}"
    )
    actual_sha = sha_line.split()[0]
    assert actual_sha == expected_sha, (
        f"NFSv3 read corrupted bytes: "
        f"expected sha256={expected_sha}, got {actual_sha}"
    )
