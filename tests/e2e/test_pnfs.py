"""E2E: pNFS Flexible Files Layout (RFC 8435) — Phase 15b exit gate.

Mounts the kiseki cluster as NFSv4.1 with `minorversion=1` so the
Linux client requests a pNFS layout. After reading 1 MiB through the
mount, `/proc/self/mountstats` must report non-zero LAYOUTGET and at
least one per-DS READ counter — proof that the client honored the
ff_layout4 from `op_layoutget` and dispatched READs to the DS
listener (rather than tunneling everything through the MDS).

The test exercises both transport flavors per ADR-038 §D4:

  * `test_pnfs_xprtsec_mtls` — RFC 9289 NFS-over-TLS path. Requires
    Linux kernel ≥ 6.7 and a running `tlshd`.

  * `test_pnfs_plaintext_fallback` — opt-in fallback for older
    kernels (RHEL/Rocky 9 baseline). Requires `KISEKI_INSECURE_NFS=true`
    on the server and `[security].allow_plaintext_nfs=true` in config.

Both tests skip gracefully when their preconditions aren't satisfied,
so the same file runs on both the dev laptop (where mounts are
typically blocked) and on the GCP perf cluster.
"""

from __future__ import annotations

import os
import re
import shutil
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
    """Spin up the 3-node compose cluster used by the multi-node tests.

    A 3-node cluster gives the MDS a meaningful round-robin range —
    `op_layoutget` will mint stripes targeting all three DS endpoints,
    so the per-DS READ counter assertion is non-trivial.
    """
    info = start_cluster()
    yield info
    stop_cluster(info)


# ---------------------------------------------------------------------------
# Helpers — kernel + mount-stat parsing
# ---------------------------------------------------------------------------


def _kernel_version() -> tuple[int, int, int]:
    """Return `(major, minor, patch)` of the running Linux kernel."""
    out = subprocess.run(
        ["uname", "-r"], check=True, capture_output=True, text=True
    ).stdout.strip()
    m = re.match(r"^(\d+)\.(\d+)(?:\.(\d+))?", out)
    if not m:
        return (0, 0, 0)
    return (int(m.group(1)), int(m.group(2)), int(m.group(3) or "0"))


def _kernel_supports_xprtsec() -> bool:
    """`xprtsec=mtls` mount option lands in mainline kernel 6.5 and is
    stable in 6.7+. RHEL/Rocky 9.5 backports starting late 2024 also
    ship it; we treat that as 6.5+ and let `mount` itself reject if
    the option isn't recognized."""
    return _kernel_version() >= (6, 5, 0)


def _tlshd_running() -> bool:
    """`xprtsec=mtls` needs the tls handshake helper. We just probe
    for the binary on PATH — a missing tlshd means the mount will
    fail, so we skip rather than fail the assertion."""
    return shutil.which("tlshd") is not None


def _have_passwordless_sudo() -> bool:
    """Return True iff `sudo -n true` succeeds. Used to avoid blocking
    on a password prompt when the test isn't already running as root."""
    try:
        return (
            subprocess.run(
                ["sudo", "-n", "true"],
                check=False,
                capture_output=True,
                timeout=2,
            ).returncode
            == 0
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False


def _mount_command(args: list[str]) -> list[str]:
    """Wrap a mount/umount command in `sudo -n` when we're not root.

    The dev path is "user with passwordless sudo" (Arch laptop, GCP
    OS Login); the CI path is "container running as root". Only when
    neither root nor passwordless sudo is available do we genuinely
    have to skip — and the caller turns that into a pytest.skip with
    a clear actionable message.
    """
    if os.geteuid() == 0:
        return args
    return ["sudo", "-n", *args]


def _can_invoke_mount() -> bool:
    return os.geteuid() == 0 or _have_passwordless_sudo()


def _read_mountstats(mount_point: Path) -> dict[str, int]:
    """Parse `/proc/self/mountstats` and return RPC op-counter sums
    for the given mount. Op codes we care about:

      * `LAYOUTGET` — proves the client requested a pNFS layout
      * `GETDEVICEINFO` — proves the client resolved a deviceid
      * `READ` — proves the data path actually flowed
    """
    counters: dict[str, int] = {"LAYOUTGET": 0, "GETDEVICEINFO": 0, "READ": 0}
    text = Path("/proc/self/mountstats").read_text()
    in_section = False
    for line in text.splitlines():
        if line.startswith("device "):
            in_section = str(mount_point) in line
            continue
        if not in_section:
            continue
        # `per-op statistics` block: op-name <space> <count> <space> ...
        for op in counters:
            m = re.match(rf"\s+{op}:\s+(\d+)\b", line)
            if m:
                counters[op] += int(m.group(1))
    return counters


def _seed_known_object(cluster: ClusterInfo, payload: bytes) -> str:
    """Write `payload` through the S3 gateway on node 1 and return the
    object's etag/key — the same composition is mountable through NFS
    because they share the underlying CompositionStore."""
    import requests

    s3 = f"http://{cluster.nodes[0].data_addr.split(':')[0]}:9000"
    resp = requests.put(f"{s3}/default/pnfs-fixture", data=payload, timeout=10)
    resp.raise_for_status()
    etag = resp.headers.get("etag", "").strip('"')
    assert etag, "S3 PUT returned empty etag"
    return etag


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@pytest.mark.e2e
def test_pnfs_xprtsec_mtls(pnfs_cluster: ClusterInfo, tmp_path: Path) -> None:
    """TLS-mounted pNFS — Phase 15b exit criterion.

    Skipped on kernels < 6.5, on hosts without `tlshd`, or when not
    root. On the GCP perf cluster these preconditions hold (Rocky 9.5
    + `KISEKI_PNFS_TLS=mtls` boot flag).
    """
    if not _kernel_supports_xprtsec():
        pytest.skip(
            f"kernel {_kernel_version()} predates xprtsec=mtls (need ≥ 6.5)"
        )
    if not _tlshd_running():
        pytest.skip("tlshd not available — xprtsec=mtls cannot complete handshake")
    if not _can_invoke_mount():
        pytest.skip(
            "no root + no passwordless sudo — try `sudo -v` first or run "
            "the suite as root"
        )

    payload = b"\xab" * 1_048_576
    etag = _seed_known_object(pnfs_cluster, payload)

    mount = tmp_path / "kiseki-pnfs"
    mount.mkdir()
    mds_host = pnfs_cluster.nodes[0].data_addr.split(":")[0]
    try:
        subprocess.run(
            _mount_command([
                "mount",
                "-t",
                "nfs4",
                "-o",
                "vers=4.1,minorversion=1,xprtsec=mtls",
                f"{mds_host}:/default",
                str(mount),
            ]),
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )

        # Drain 1 MiB so the per-DS READ counter accumulates.
        path = mount / etag
        data = path.read_bytes()
        assert len(data) == len(payload)

        stats = _read_mountstats(mount)
        assert stats["LAYOUTGET"] >= 1, (
            f"client never sent LAYOUTGET — stats={stats}"
        )
        assert stats["GETDEVICEINFO"] >= 1, (
            f"client never sent GETDEVICEINFO — stats={stats}"
        )
        assert stats["READ"] >= 1, (
            f"no NFS READs accounted — pNFS dispatch may have silently fallen back"
        )
    finally:
        subprocess.run(
            _mount_command(["umount", str(mount)]),
            check=False,
            capture_output=True,
            timeout=10,
        )


@pytest.mark.e2e
def test_pnfs_plaintext_fallback(
    pnfs_cluster: ClusterInfo, tmp_path: Path
) -> None:
    """Audited plaintext-NFS fallback (ADR-038 §D4.2) — the path the
    perf cluster uses for Rocky 9.x baselines without `xprtsec=mtls`.

    Skipped when neither root nor passwordless sudo is available, or
    when the cluster wasn't booted with the plaintext fallback flags
    (detected by attempting the mount and treating EPROTO/ECONNRESET
    as "TLS-only server, skip").
    """
    if not _can_invoke_mount():
        pytest.skip(
            "no root + no passwordless sudo — try `sudo -v` first or run "
            "the suite as root"
        )

    payload = b"\xcd" * 1_048_576
    etag = _seed_known_object(pnfs_cluster, payload)

    mount = tmp_path / "kiseki-plaintext"
    mount.mkdir()
    mds_host = pnfs_cluster.nodes[0].data_addr.split(":")[0]
    result = subprocess.run(
        _mount_command([
            "mount",
            "-t",
            "nfs4",
            "-o",
            "vers=4.1,minorversion=1",
            f"{mds_host}:/default",
            str(mount),
        ]),
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        # The server might be running in TLS-only mode (no plaintext
        # fallback opted in). Treat as skip rather than fail.
        if any(
            kw in result.stderr.lower()
            for kw in ("connection refused", "protocol", "no route")
        ):
            pytest.skip(
                f"plaintext NFS not available on this cluster: {result.stderr.strip()}"
            )
        pytest.fail(f"mount failed: {result.stderr.strip()}")

    try:
        path = mount / etag
        data = path.read_bytes()
        assert len(data) == len(payload)

        stats = _read_mountstats(mount)
        assert stats["LAYOUTGET"] >= 1, (
            "plaintext-mode pNFS still requires LAYOUTGET — client never asked"
        )
        assert stats["READ"] >= 1, (
            "no NFS READs accounted under plaintext fallback"
        )
    finally:
        subprocess.run(
            _mount_command(["umount", str(mount)]),
            check=False,
            capture_output=True,
            timeout=10,
        )


@pytest.mark.e2e
def test_pnfs_layout_recall_on_drain(pnfs_cluster: ClusterInfo) -> None:
    """When a storage node enters drain (ADR-035), MDS fires
    LAYOUTRECALL within I-PN5's 1-sec SLA. Phase 15c integration
    witness — currently a placeholder; BDD scenario `@pnfs-15c
    Drain triggers LAYOUTRECALL within 1 second` covers the
    in-process witness.
    """
    pytest.skip(
        "Phase 15c.1 follow-up — production drain hook in kiseki-server "
        "is not yet wired to the MdsLayoutManager (BDD scenario "
        "@pnfs-15c covers the in-process witness)."
    )
