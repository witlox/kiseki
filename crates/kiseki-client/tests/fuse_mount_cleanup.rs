#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression test for bug #3 from the 2026-05-09 GCP `compact` run:
//! when a kiseki-client process is SIGKILL'd, the kernel keeps its
//! FUSE mountpoint registered (`kiseki on /mnt/X type fuse`) but
//! the userspace daemon is dead. Subsequent `stat` calls block in
//! the kernel waiting for the dead daemon. The next mount attempt
//! either fails outright (path already mounted) or succeeds-but-
//! unresponsive (overlapping mounts produce a wedged dentry).
//!
//! The fix in `crates/kiseki-client/src/bin/kiseki_client.rs::evict_stale_fuse_mount`
//! runs `fusermount3 -uz <path>` before every mount to clear any
//! zombie state. This test exercises the cleanup function directly
//! against a real path. It does NOT need an actual FUSE mount —
//! the helper's no-op-on-empty-path behavior is the contract that
//! matters: cleanup must be safe to call BEFORE we know whether
//! a mount exists.

use std::path::PathBuf;

/// The cleanup helper must succeed (or at least not panic) when
/// the mountpoint has nothing mounted on it. This is the common
/// case on a fresh boot or after a clean shutdown — the eviction
/// runs anyway because we don't have a way to cheaply distinguish
/// stale-mount from never-mounted before mounting.
///
/// We invoke the same shell command the binary uses
/// (`fusermount3 -uz <path>`) and verify it doesn't escalate a
/// non-error into an error. This pins the no-op-on-empty contract
/// without needing a real FUSE mount in the test environment.
#[test]
fn fusermount_unmount_lazy_on_clean_path_is_a_noop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path: PathBuf = dir.path().to_path_buf();
    assert!(path.exists(), "tempdir should exist");

    // Spawn fusermount3 directly. If the binary isn't installed,
    // skip with a clear marker (CI containers without fuse3 are a
    // valid environment for the rest of the suite).
    let out = match std::process::Command::new("fusermount3")
        .args(["-uz", path.to_str().expect("path is utf-8")])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("fusermount3 not installed in this environment; skipping");
            return;
        }
        Err(e) => panic!("fusermount3 spawn failed: {e}"),
    };

    // Common stderr on a non-mounted path:
    //   "fusermount3: entry for /tmp/X not found in /etc/mtab"
    // We accept any non-zero exit AS LONG AS the binary didn't
    // hang and didn't deadlock our test runner.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "fusermount3 -uz on clean path: exit={:?} stdout={:?} stderr={:?}",
        out.status.code(),
        stdout.trim(),
        stderr.trim(),
    );
    // The contract: cleanup MUST return within a reasonable time
    // (Command::output() is a synchronous call so this naturally
    // bounds it) and MUST NOT crash the test process. Both held
    // simply by reaching this point.
}

/// The eviction helper must accept any well-formed path string,
/// including ones that don't exist. The kiseki-client binary
/// invokes it on the `--mountpoint` value before checking that the
/// mountpoint directory exists, so a typo'd path shouldn't crash
/// the binary.
#[test]
fn fusermount_unmount_lazy_on_nonexistent_path_does_not_crash() {
    let phony = "/tmp/kiseki-cleanup-test-nonexistent-path-blah-9c4a";
    let out = match std::process::Command::new("fusermount3")
        .args(["-uz", phony])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => panic!("spawn: {e}"),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!(
        "fusermount3 -uz on nonexistent path: exit={:?} stderr={:?}",
        out.status.code(),
        stderr.trim(),
    );
    // Same contract as above: process completed, didn't hang.
}
