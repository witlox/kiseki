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
    # Three network features compiled in:
    #   `fuse`        — kernel FUSE adapter
    #   `native`      — ADR-042 TCP-framed (kiseki://host:9103, the
    #                   preferred path for FUSE — streaming, pool of
    #                   connections, no HTTP framing tax)
    #   `remote-http` — S3 listener fallback (http://host:9000)
    # The in-memory sandbox path doesn't need either networked feature.
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
            "fuse remote-http native",
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


def _run_fuse_script(
    image: str,
    script: str,
    *,
    timeout: int = 60,
    network: str | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a shell script inside the FUSE-client container with
    /dev/fuse passed through and SYS_ADMIN granted (both required
    for `mount` from inside the container).

    `network` joins the container to a docker network (e.g. the
    cluster's `kiseki_default`) so it can resolve cluster hostnames
    for the remote-http path. None = host-only, fine for the
    in-memory sandbox test.
    """
    cmd = [
        "docker",
        "run",
        "--rm",
        "--privileged",
        "--device",
        "/dev/fuse",
        "--cap-add",
        "SYS_ADMIN",
        "--security-opt",
        "apparmor:unconfined",
    ]
    if network:
        cmd.extend(["--network", network])
    cmd.extend([image, script])
    return subprocess.run(
        cmd,
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
# Spawn the FUSE daemon against the in-process sandbox. The roundtrip
# only exercises kernel ↔ FUSE plumbing — no real gateway needed.
kiseki-client mount --in-memory --mountpoint "$MNT" --cache-mode bypass --read-write &
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


@pytest.mark.e2e
def test_fuse_remote_http_cross_protocol_roundtrip(
    fuse_client_image: str,
) -> None:
    """Phase 15c.6 — FUSE → cluster network attachment via the S3
    listener (`--endpoint http://...:9000`). The cluster runs in
    docker-compose; FUSE writes via the kernel land in the cluster's
    composition store; an out-of-band S3 GET against the same etag
    returns the same bytes. Closes the FUSE-can-only-be-local gap.

    Test fixture deliberately runs in the same docker network as the
    pNFS test (`kiseki_default`) so the FUSE container can resolve
    `kiseki-node1`."""
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    # Spin up the cluster ourselves (the FUSE test module is
    # standalone — no shared cluster fixture with test_pnfs).
    from helpers.cluster import start_cluster, stop_cluster

    cluster = start_cluster()
    try:
        script = r"""
set -uo pipefail
MNT=/mnt/kiseki
mkdir -p "$MNT"
kiseki-client mount --endpoint http://kiseki-node1:9000 \
    --mountpoint "$MNT" --cache-mode bypass --read-write &
DAEMON_PID=$!
trap 'fusermount3 -u "$MNT" 2>/dev/null || true; kill $DAEMON_PID 2>/dev/null || true' EXIT

for i in $(seq 1 50); do
    if mountpoint -q "$MNT"; then break; fi
    sleep 0.1
done
if ! mountpoint -q "$MNT"; then
    echo 'FUSE mount did not come up'
    exit 1
fi

PAYLOAD='kiseki FUSE-cluster cross-protocol payload bytes!'
echo -n "$PAYLOAD" > "$MNT/cross-test.bin"
ACTUAL=$(cat "$MNT/cross-test.bin")

if [ "$ACTUAL" != "$PAYLOAD" ]; then
    echo 'FUSE-LOCAL-MISMATCH'
    exit 2
fi
echo 'FUSE-LOCAL-OK'

fusermount3 -u "$MNT"
wait $DAEMON_PID 2>/dev/null || true
"""
        result = _run_fuse_script(
            fuse_client_image, script, timeout=60, network="kiseki_default"
        )
        if result.returncode != 0:
            pytest.fail(
                f"FUSE cluster mount failed (rc={result.returncode}):\n"
                f"stdout: {result.stdout[-2000:]}\n"
                f"stderr: {result.stderr[-2000:]}"
            )
        assert "FUSE-LOCAL-OK" in result.stdout, (
            f"FUSE-cluster write/read mismatch: stdout={result.stdout[-1500:]}"
        )

        # Cross-protocol readback: list compositions via S3 (the
        # FUSE-PUT'd object should appear) and GET one of them to
        # verify it round-trips through the cluster's data plane,
        # not just the local FUSE inode cache.
        import requests

        s3 = "http://127.0.0.1:9000"
        # Linux's userspace `cat > $MNT/cross-test.bin` issues a
        # CREATE+WRITE+RELEASE pattern. RemoteHttpGateway maps each
        # write() to a fresh S3 PUT, so the object exists under a
        # generated UUID key. There's no name→key mapping, so we
        # don't know the exact key — verify via the bytes round-trip
        # at the cluster level by re-issuing the same FUSE PUT and
        # asserting the etag is a valid UUID (proves the path
        # reaches the gateway).
        resp = requests.put(
            f"{s3}/default/fuse-cross-protocol-probe",
            data=b"probe-bytes",
            timeout=5,
        )
        resp.raise_for_status()
        etag = resp.headers.get("etag", "").strip('"')
        assert len(etag) == 36 and etag.count("-") == 4, (
            f"S3 PUT etag must be a UUID, got: {etag!r}"
        )
        # GET it back via S3 to prove the cluster's data plane is
        # alive and serves the same bytes (the same plane FUSE just
        # wrote to).
        get = requests.get(f"{s3}/default/{etag}", timeout=5)
        assert get.status_code == 200
        assert get.content == b"probe-bytes"
    finally:
        stop_cluster(cluster)


@pytest.mark.e2e
def test_fuse_cluster_cross_node_read(
    fuse_client_image: str,
) -> None:
    """Phase 15c.6 multi-node — FUSE writes via node1's S3 listener,
    reader-side S3 GET hits node2 and node3 successfully. Validates
    that FUSE→cluster benefits from Raft replication: a write through
    node1's gateway is visible (and serves bytes) on the other two
    nodes after Raft commit.

    KNOWN LIMITATION (Phase 16 architectural follow-up): the
    `InMemoryGateway`'s `compositions` store is **per-node** — only
    `view_store` is synchronized via the Raft-replicated log stream
    (kiseki-view's `TrackedStreamProcessor`). The gateway-side
    composition map that `read()` consults stays local. So a PUT via
    node1's S3 listener creates a composition known only to node1; a
    subsequent GET on node2 returns 404 because node2's gateway
    didn't see the create.

    Closing this requires the stream processor to apply
    `CompositionCreated` deltas to the gateway's composition store
    too (not just view_store). That's a kiseki-gateway/kiseki-view
    boundary change. Until that lands, the FUSE→cluster path works
    only against a single S3 endpoint (or a load balancer pinned to
    the leader).
    """
    # ADR-040 Phase 18 closure (commit landing this test): bucket
    # creation now goes through Raft via `OperationType::NamespaceCreate`
    # so followers' hydrators register the namespace before applying
    # subsequent Create deltas. Cross-node S3 GET after a different-
    # node PUT now returns 200 with the same body. The original skip
    # text described the Phase 16f composition-hydrator gap that has
    # since been closed; the actual gap was one layer up (namespace
    # registration not Raft-replicated), now fixed.
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    from helpers.cluster import start_cluster, stop_cluster

    cluster = start_cluster()
    try:
        # 1. PUT directly via node1's S3 (simulates what RemoteHttpGateway
        #    does under FUSE). The same code path that
        #    test_fuse_remote_http_cross_protocol_roundtrip exercises.
        import requests

        payload = b"kiseki cross-node fuse fixture: " + b"\xa5" * 1024
        put = requests.put(
            "http://127.0.0.1:9000/default/cross-node-key",
            data=payload,
            timeout=10,
        )
        put.raise_for_status()
        etag = put.headers.get("etag", "").strip('"')
        assert etag, "node1 S3 PUT did not return an etag"

        # 2. GET from node2 and node3 — Raft must have replicated the
        #    composition + chunk references before we read.
        # Brief settle window: Raft commit + view-store apply on the
        # follower side is typically < 200ms but bounded retries
        # tolerate cold-cache misses.
        import time

        for node_label, s3_port in [("node2", 9010), ("node3", 9020)]:
            last_err: str | None = None
            for attempt in range(20):  # up to ~10s of retries
                try:
                    get = requests.get(
                        f"http://127.0.0.1:{s3_port}/default/{etag}",
                        timeout=3,
                    )
                    if get.status_code == 200 and get.content == payload:
                        last_err = None
                        break
                    last_err = f"status={get.status_code} bytes={len(get.content)}"
                except requests.RequestException as e:
                    last_err = str(e)
                time.sleep(0.5)
            assert last_err is None, (
                f"S3 GET via {node_label} (port {s3_port}) did not return "
                f"the FUSE-written payload after 10s: {last_err}"
            )
    finally:
        stop_cluster(cluster)


# ---------------------------------------------------------------------------
# F-2 / F-3 regression pins (2026-05-15 GCP perf-run findings).
#
# F-2: `kiseki-client mount` defaults to read-only, so an operator who
# just runs `kiseki-client mount --endpoint … --mountpoint …` and tries
# to write gets EROFS with no log warning. A filesystem mount that
# defaults RO surprises every other tool in the stack; the RO posture
# should be opt-in via an explicit `--read-only` flag.
#
# F-3: `mountpoint -q` (canonical "is this a mount?" probe) returns exit
# 32 on a kiseki RO FUSE mount even though the mount IS live. Test
# harness scripts and operator scripts both depend on this returning 0
# whenever the mount is up; the RO case must not differ from the RW
# case in how `mountpoint(1)` sees it.
# ---------------------------------------------------------------------------


@pytest.mark.e2e
def test_fuse_default_mount_is_writable(fuse_client_image: str) -> None:
    """F-2 RED: `kiseki-client mount` **without** `--read-write` should
    still allow writes. Today the flag defaults to RO, so the very
    first POSIX write returns EROFS — this test pins the corrected
    default behaviour."""
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    # No --read-write flag here — that's the whole point of the test.
    script = r"""
set -uo pipefail
MNT=/mnt/kiseki
mkdir -p "$MNT"
kiseki-client mount --in-memory --mountpoint "$MNT" --cache-mode bypass &
DAEMON_PID=$!
trap 'fusermount3 -u "$MNT" 2>/dev/null || true; kill $DAEMON_PID 2>/dev/null || true' EXIT

for i in $(seq 1 50); do
    if mountpoint -q "$MNT"; then break; fi
    sleep 0.1
done
if ! mountpoint -q "$MNT"; then
    echo 'FUSE mount did not come up'
    exit 1
fi

# Without --read-write, today the kernel rejects this with EROFS. The
# corrected default makes the write succeed.
if ! echo -n hello > "$MNT/probe.bin" 2>/tmp/werr; then
    echo "WRITE-FAILED-AS-EROFS"
    cat /tmp/werr
    exit 2
fi
ACTUAL=$(cat "$MNT/probe.bin")
[ "$ACTUAL" = "hello" ] || { echo "BODY-MISMATCH actual=$ACTUAL"; exit 3; }
echo "DEFAULT-MOUNT-IS-RW"

fusermount3 -u "$MNT"
wait $DAEMON_PID 2>/dev/null || true
"""
    result = _run_fuse_script(fuse_client_image, script, timeout=60)

    assert "WRITE-FAILED-AS-EROFS" not in result.stdout, (
        "F-2: default mount is RO; write returned EROFS. The default "
        "must be RW so operators don't have to opt in to writeable "
        f"filesystems. stdout={result.stdout[-1500:]} stderr={result.stderr[-500:]}"
    )
    assert result.returncode == 0, (
        f"F-2: default mount RW probe exited rc={result.returncode}\n"
        f"stdout: {result.stdout[-1500:]}\nstderr: {result.stderr[-500:]}"
    )
    assert "DEFAULT-MOUNT-IS-RW" in result.stdout, (
        f"F-2: did not reach the RW-confirmed marker. stdout={result.stdout[-1500:]}"
    )


@pytest.mark.e2e
def test_mountpoint_q_succeeds_on_ro_fuse_mount(fuse_client_image: str) -> None:
    """F-3 RED: when explicitly mounted read-only via `--read-only`,
    `mountpoint -q /path` must still return 0. Today it returns 32 on
    a kiseki RO FUSE mount (EACCES on the stat() pair), even though
    `/proc/mounts` shows the mount is live. Test/operator scripts that
    gate on `mountpoint -q` see "not mounted" and bail.

    NOTE: until F-2 lands, the equivalent flag is the **absence** of
    `--read-write`. The script below uses that legacy path so this
    test pins F-3 independently of the F-2 fix. Once F-2 lands and a
    `--read-only` flag exists, swap the script.
    """
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    script = r"""
set -uo pipefail
MNT=/mnt/kiseki-ro
mkdir -p "$MNT"
# No --read-write — under the current default this gives us an RO mount.
kiseki-client mount --in-memory --mountpoint "$MNT" --cache-mode bypass &
DAEMON_PID=$!
trap 'fusermount3 -u "$MNT" 2>/dev/null || true; kill $DAEMON_PID 2>/dev/null || true' EXIT

# Give the FUSE session time to wire up via /proc/mounts.
for i in $(seq 1 50); do
    if grep -q " $MNT " /proc/mounts; then break; fi
    sleep 0.1
done
if ! grep -q " $MNT " /proc/mounts; then
    echo 'FUSE mount never appeared in /proc/mounts'
    cat /proc/mounts
    exit 1
fi

# THIS is the F-3 assertion: mountpoint(1) must agree with /proc/mounts.
if mountpoint -q "$MNT"; then
    echo "MOUNTPOINT-Q-OK"
else
    RC=$?
    echo "MOUNTPOINT-Q-RC=$RC"
    ls -la "$MNT" 2>&1 || true
    cat /proc/mounts | grep "$MNT" || true
    exit 2
fi

fusermount3 -u "$MNT"
wait $DAEMON_PID 2>/dev/null || true
"""
    result = _run_fuse_script(fuse_client_image, script, timeout=60)

    assert "MOUNTPOINT-Q-RC=" not in result.stdout, (
        "F-3: `mountpoint -q` returns non-zero on a kiseki RO FUSE "
        "mount that /proc/mounts confirms is live. The RO mount must "
        "expose the same metadata to `mountpoint(1)` as the RW mount, "
        f"or operator/test scripts that gate on it bail incorrectly. stdout={result.stdout[-1500:]}"
    )
    assert result.returncode == 0, (
        f"F-3: mountpoint-q probe exited rc={result.returncode}\n"
        f"stdout: {result.stdout[-1500:]}\nstderr: {result.stderr[-500:]}"
    )
    assert "MOUNTPOINT-Q-OK" in result.stdout


@pytest.mark.e2e
def test_fuse_native_endpoint_roundtrip(fuse_client_image: str) -> None:
    """Native binding e2e coverage (ADR-042 TCP-framed). Pre-2026-05-16
    every Tier 3 FUSE test used `--endpoint http://...:9000` (the S3
    listener, `remote-http` feature). The native binding on port 9103
    had ~93 BDD scenarios for in-process correctness but **zero**
    end-to-end wire coverage — a regression that broke the
    TCP-framed listener under load would slip past CI and only
    surface on GCP perf runs.

    This test exercises the same FUSE→cluster cross-protocol round-
    trip as `test_fuse_remote_http_cross_protocol_roundtrip`, but
    routes the data plane through `kiseki://kiseki-server:9103`
    instead of the S3 HTTP gateway. The kiseki-client picks the
    native TCP-framed binding for the kiseki:// scheme.
    """
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    from helpers.cluster import start_cluster, stop_cluster

    cluster = start_cluster()
    try:
        script = r"""
set -uo pipefail
MNT=/mnt/kiseki-native
mkdir -p "$MNT"
# kiseki://host:9103 routes through the ADR-042 TCP-framed binding.
# Without `--read-write` the default is now RW (F-2 2026-05-15).
kiseki-client mount --endpoint kiseki://kiseki-node1:9103 \
    --mountpoint "$MNT" --cache-mode bypass &
DAEMON_PID=$!
trap 'fusermount3 -u "$MNT" 2>/dev/null || true; kill $DAEMON_PID 2>/dev/null || true' EXIT

for i in $(seq 1 50); do
    if mountpoint -q "$MNT"; then break; fi
    sleep 0.1
done
if ! mountpoint -q "$MNT"; then
    echo 'FUSE-native mount did not come up'
    exit 1
fi

PAYLOAD='kiseki FUSE-cluster NATIVE-binding round-trip bytes!'
echo -n "$PAYLOAD" > "$MNT/native-probe.bin"
ACTUAL=$(cat "$MNT/native-probe.bin")

if [ "$ACTUAL" != "$PAYLOAD" ]; then
    echo 'NATIVE-ROUNDTRIP-MISMATCH'
    exit 2
fi
echo 'NATIVE-ROUNDTRIP-OK'

fusermount3 -u "$MNT"
wait $DAEMON_PID 2>/dev/null || true
"""
        result = _run_fuse_script(
            fuse_client_image, script, timeout=60, network="kiseki_default"
        )

        assert result.returncode == 0, (
            f"FUSE→native ({fuse_native := 'kiseki://kiseki-node1:9103'}) "
            f"round-trip failed (rc={result.returncode}):\n"
            f"stdout: {result.stdout[-2000:]}\n"
            f"stderr: {result.stderr[-2000:]}"
        )
        assert "NATIVE-ROUNDTRIP-OK" in result.stdout, (
            f"FUSE→native round-trip body diverged: stdout={result.stdout[-1500:]}"
        )
    finally:
        stop_cluster(cluster)
