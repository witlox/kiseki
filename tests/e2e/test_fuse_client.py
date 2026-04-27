"""E2E: FUSE client — `kiseki-client mount` POSIX round-trip.

CRITICAL CAVEAT (surfaced honestly here, not hidden in the harness):
the `kiseki-client mount` binary today wires a **local in-memory
gateway** (see `crates/kiseki-client/src/bin/kiseki_client.rs`'s
`handle_mount` — it constructs `InMemoryGateway` directly, no gRPC,
no network). So this test validates that the FUSE/POSIX adapter
itself works end-to-end through the kernel; it does **not** validate
a "FUSE client → kiseki cluster" network path because that path
does not exist in the codebase yet.

When kiseki-client grows a `GrpcGateway` impl that connects to
`kiseki-server:9100`, the assertion list expands to include reading
back via S3 / NFS to confirm the cross-protocol roundtrip — at that
point this test becomes a true e2e network test rather than a
local-mount validation.

Test mechanic: run inside a privileged docker container with
`/dev/fuse` exposed (the bwrap-style sandboxes pytest may run in
generally don't expose /dev/fuse). Spawn `kiseki-client mount`,
write+read a fixture file via plain POSIX I/O (kernel routes through
/dev/fuse to the daemon), assert byte-equality.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest


def _workspace_root() -> Path:
    return Path(__file__).resolve().parents[2]


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


FUSE_CLIENT_IMAGE = "kiseki-fuse-client:test"


@pytest.fixture(scope="module")
def fuse_client_image() -> str:
    """Build kiseki-client (release, --features fuse), then build the
    docker image that wraps it. The image is rebuilt every module run
    because the binary mtime changes; docker layer cache makes this
    fast after the first build."""
    root = _workspace_root()
    # 1. Build the binary on the host (matches glibc with Ubuntu 24.04).
    subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "kiseki-client",
            "--bin",
            "kiseki-client",
            "--features",
            "fuse",
        ],
        cwd=root,
        check=True,
        capture_output=True,
    )
    binary = root / "target" / "release" / "kiseki-client"
    assert binary.exists(), f"kiseki-client not built at {binary}"

    # 2. Build the wrapper image.
    dockerfile = Path(__file__).parent / "Dockerfile.fuse-client"
    subprocess.run(
        [
            "docker",
            "build",
            "-t",
            FUSE_CLIENT_IMAGE,
            "-f",
            str(dockerfile),
            str(root),
        ],
        cwd=root,
        check=True,
        capture_output=True,
    )
    return FUSE_CLIENT_IMAGE


def _run_fuse_script(image: str, script: str, *, timeout: int = 60) -> subprocess.CompletedProcess[str]:
    """Run a shell script inside the FUSE-client container with
    /dev/fuse passed through and SYS_ADMIN granted (both required
    for `mount` from inside the container)."""
    return subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--privileged",  # blanket — easier than the precise cap set
            "--device",
            "/dev/fuse",
            "--cap-add",
            "SYS_ADMIN",
            "--security-opt",
            "apparmor:unconfined",
            image,
            script,
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


@pytest.mark.e2e
def test_fuse_write_read_roundtrip(fuse_client_image: str) -> None:
    """Plain POSIX write+read through the FUSE mount inside docker.

    Validates: kernel→/dev/fuse→KisekiFuse op dispatch (CREATE +
    WRITE + RELEASE + LOOKUP + OPEN + READ + RELEASE), in-memory
    gateway round-trip, attribute consistency. Does NOT validate
    network attachment — see module docstring."""
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    script = r"""
set -uo pipefail
MNT=/mnt/kiseki
mkdir -p "$MNT"
# Spawn the FUSE daemon in the background. --endpoint is required
# by argv but ignored by the in-memory backend.
kiseki-client mount --endpoint 127.0.0.1:9100 --mountpoint "$MNT" --cache-mode bypass --read-write &
DAEMON_PID=$!
trap 'fusermount3 -u "$MNT" 2>/dev/null || true; kill $DAEMON_PID 2>/dev/null || true' EXIT

# Wait until the kernel has wired the FUSE socket. mountpoint(1)
# reports rc=0 only after the mount is live.
for i in $(seq 1 50); do
    if mountpoint -q "$MNT"; then break; fi
    sleep 0.1
done
if ! mountpoint -q "$MNT"; then
    echo 'FUSE mount did not come up'
    exit 1
fi

# Write+read+verify a fixture file.
PAYLOAD='kiseki FUSE e2e payload bytes 0123456789ABCDEF'
echo -n "$PAYLOAD" > "$MNT/fixture.bin"
ACTUAL=$(cat "$MNT/fixture.bin")

if [ "$ACTUAL" != "$PAYLOAD" ]; then
    echo 'MISMATCH'
    echo "expected: $PAYLOAD"
    echo "actual:   $ACTUAL"
    exit 2
fi
echo 'FUSE-ROUNDTRIP-OK'

# Verify readdir surfaces the file.
ls -la "$MNT/"

fusermount3 -u "$MNT"
wait $DAEMON_PID 2>/dev/null || true
"""
    result = _run_fuse_script(fuse_client_image, script, timeout=60)

    if result.returncode != 0:
        pytest.fail(
            f"FUSE roundtrip failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )

    assert "FUSE-ROUNDTRIP-OK" in result.stdout, (
        f"FUSE write/read mismatch: stdout={result.stdout[-1000:]}"
    )
    assert "fixture.bin" in result.stdout, (
        f"FUSE readdir missing file: stdout={result.stdout[-1000:]}"
    )
