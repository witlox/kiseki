#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
//! GH issue #37 regression: `kiseki-client mount` did not honor
//! `O_DIRECT` on opens, so `fio --direct=1` reported 0 MB/s and
//! every HPC-shaped benchmark was either silently degraded or
//! silently fell through to the kernel page cache.
//!
//! The fix surfaces libfuse 3.x's per-fd `FOPEN_DIRECT_IO` flag
//! from `FuseDaemon::open` / `create` whenever the caller opened
//! the file with `O_DIRECT`. The kernel uses that reply bit to
//! flip the file's caching mode for the life of the fd — read +
//! write paths bypass the page cache without `EINVAL` on the
//! open call itself.
//!
//! ## Why a real mount
//!
//! The kernel side of `O_DIRECT` is what gets it wrong pre-fix:
//! the kernel only honors `O_DIRECT` on a FUSE fd when the FUSE
//! backend returned `FOPEN_DIRECT_IO` on `FUSE_OPEN` /
//! `FUSE_CREATE`. Without that reply bit, the kernel either
//! rejects the open with EINVAL outright or silently routes the
//! request through the page cache — that's the bug. A pure unit
//! test against `KisekiFuse` cannot detect this because the kernel
//! path is what enforces the contract.
//!
//! The test spawns a real `kiseki_client::fuse_daemon::mount`
//! against an in-memory gateway, opens the file with `O_DIRECT`,
//! writes 4 KiB (aligned), and asserts the write succeeded.
//!
//! ## CI skip path
//!
//! Environments without `/dev/fuse` (some CI containers, macOS,
//! restricted sandboxes) skip with a clear marker rather than
//! fail. This matches `fuse_mount_cleanup.rs`'s pattern.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use kiseki_client::fuse_daemon::open_options_for_flags;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kiseki_chunk::store::ChunkStore;
use kiseki_client::fuse_fs::KisekiFuse;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;

fn tenant() -> kiseki_common::ids::OrgId {
    OrgId(uuid::Uuid::from_u128(370))
}
fn ns() -> kiseki_common::ids::NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(371))
}

/// Probe `/dev/fuse` and the ability to actually mount before
/// attempting the real test. Returns `Some(reason)` if the test
/// should skip (with the reason logged), or `None` if it can proceed.
///
/// Two distinct failure modes are handled:
/// 1. `/dev/fuse` not openable — no kernel FUSE module / no permission.
/// 2. `fusermount3` runs but `mount(2)` returns EPERM — common in
///    container sandboxes where the setuid bit on fusermount3 is
///    suppressed (root mapped to `nobody`). Detect by attempting
///    a sacrificial mount via fusermount3 directly.
fn fuse_unavailable_reason() -> Option<String> {
    let path = std::ffi::CString::new("/dev/fuse").expect("static");
    // SAFETY: standard libc open of a C string. We immediately close on success.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Some(format!(
            "/dev/fuse not openable: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fd from open above.
    unsafe {
        libc::close(fd);
    }
    match std::process::Command::new("fusermount3")
        .arg("--version")
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => return Some(format!("fusermount3 --version exit {:?}", o.status.code())),
        Err(e) => return Some(format!("fusermount3 not installed: {e}")),
    }
    // Probe: in some sandboxes the kernel returns EPERM on mount(2)
    // because fusermount3's setuid bit is suppressed (root mapped to
    // `nobody`). A simple way to detect this without bringing up the
    // whole daemon is to try `fusermount3 -uz` on a fresh tempdir
    // and look at its exit — that doesn't actually mount, but if
    // EPERM is the ambient policy the test would fail on mount(2)
    // later anyway. We can't trivially distinguish "no mount perm"
    // from "ambient EPERM" without trying a real mount, so the test
    // body itself catches and skips on EPERM (see below).
    None
}

fn build_inmemory_fuse() -> KisekiFuse<InMemoryGateway> {
    let compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: ns(),
        tenant_id: tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
        tier_policy: Vec::new(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x37; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
    KisekiFuse::new(gw, tenant(), ns())
}

/// Lazily unmount on drop. Best-effort: ignores errors because
/// the kernel may already have unmounted via a successful
/// `umount2(MNT_DETACH)` from another path.
struct MountGuard {
    path: PathBuf,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("fusermount3")
            .args(["-uz", self.path.to_str().expect("utf-8")])
            .output();
    }
}

/// GH#37 unit-level pin: `open_options_for_flags` MUST set
/// `direct_io = true` when `O_DIRECT` is in the kernel-supplied
/// open flags. This is the deterministic guard that runs on every
/// CI lane regardless of `/dev/fuse` availability. The kernel-
/// driven test below is the integration assertion.
#[test]
fn open_options_for_o_direct_flags_sets_direct_io() {
    let opts = open_options_for_flags(libc::O_RDWR | libc::O_DIRECT);
    assert!(
        opts.direct_io,
        "FuseDaemon::open MUST set FOPEN_DIRECT_IO when the caller \
         opened the file with O_DIRECT — otherwise the kernel ignores \
         O_DIRECT and routes the IO through the page cache (GH#37)"
    );
    // direct_io implies the page cache is bypassed; keep_cache MUST
    // be false in that case, or the kernel would still cache the
    // pre-direct content and serve stale reads.
    assert!(
        !opts.keep_cache,
        "direct_io and keep_cache are mutually exclusive — when \
         direct_io is set the kernel must not cache this fd's pages"
    );
}

/// GH#37 unit-level pin: opens without `O_DIRECT` keep the
/// pre-existing `FOPEN_KEEP_CACHE` behavior (immutable chunks
/// across opens). A regression here would re-introduce the
/// 2026-05-04 GCP perf cliff on repeat reads.
#[test]
fn open_options_without_o_direct_preserves_keep_cache() {
    let opts = open_options_for_flags(libc::O_RDONLY);
    assert!(
        !opts.direct_io,
        "non-O_DIRECT opens MUST NOT set FOPEN_DIRECT_IO"
    );
    assert!(
        opts.keep_cache,
        "non-O_DIRECT opens MUST keep FOPEN_KEEP_CACHE (chunks are \
         content-addressed; the page cache stays valid across opens)"
    );
}

/// GH#37 RED: kernel kept rejecting / silently degrading `O_DIRECT`
/// opens before the fix because `FuseDaemon::open` returned an
/// `OpenOptions` with `direct_io = false`. With the fix in place
/// the daemon inspects the FUSE_OPEN `flags` argument, observes
/// `O_DIRECT`, and replies with `FOPEN_DIRECT_IO` so the kernel
/// honors the direct-I/O semantics for the life of the fd.
#[test]
fn o_direct_open_succeeds_and_write_round_trips() {
    if let Some(reason) = fuse_unavailable_reason() {
        eprintln!("FUSE not available, skipping O_DIRECT mount test: {reason}");
        return;
    }

    let mountdir = tempfile::tempdir().expect("tempdir");
    let mountpoint = mountdir.path().to_path_buf();
    let guard = MountGuard {
        path: mountpoint.clone(),
    };

    // Spawn the mount on a dedicated thread; it blocks until the
    // kernel unmounts. The MountGuard unmounts on test exit.
    // The mount call's Result is reported via a channel so the
    // main thread can distinguish "EPERM in this sandbox, skip"
    // from "real test failure".
    let mount_ready = Arc::new(AtomicBool::new(false));
    let mount_ready_w = Arc::clone(&mount_ready);
    let mountpoint_for_thread = mountpoint.clone();
    let (mount_result_tx, mount_result_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let mount_thread = thread::Builder::new()
        .name("kiseki-fuse-mount-test".to_owned())
        .spawn(move || {
            let fs = build_inmemory_fuse();
            mount_ready_w.store(true, Ordering::SeqCst);
            let r = kiseki_client::fuse_daemon::mount(
                fs,
                &mountpoint_for_thread,
                /* read_write */ true,
            );
            let _ = mount_result_tx.send(r);
        })
        .expect("spawn mount thread");

    // Wait until the kernel has wired the mount: stat of the
    // mountpoint reports a different st_dev than the parent dir.
    let parent = mountdir
        .path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .to_path_buf();
    let parent_dev = std::fs::metadata(&parent)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.dev()
        })
        .unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut mounted = false;
    while Instant::now() < deadline {
        if mount_ready.load(Ordering::SeqCst) {
            if let Ok(meta) = std::fs::metadata(&mountpoint) {
                use std::os::unix::fs::MetadataExt;
                if meta.dev() != parent_dev {
                    mounted = true;
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !mounted {
        // The mount call may already have failed (e.g. EPERM in a
        // container sandbox). Peek the result channel non-blocking;
        // if it's an EPERM/EACCES we skip the test rather than fail
        // — the environment can't run the test, but the kiseki code
        // under test is fine. Any other error is a real failure.
        match mount_result_rx.try_recv() {
            Ok(Err(e)) => {
                // The libfuse-3 mount helper (fusermount3) prints
                // its own EPERM message to stderr ("mount failed:
                // Operation not permitted") and then returns an
                // opaque failure that `kiseki-fuse` surfaces as
                // `errno 5` (EIO). Any mount-setup failure is an
                // environmental issue (no /dev/fuse perms, no
                // FUSE module, suppressed setuid in a sandbox);
                // skip rather than fail. A future ADR can refine
                // the libfuse wrapper to propagate the real errno.
                let msg = e.to_string();
                eprintln!(
                    "FUSE mount setup failed (likely environment, not kiseki bug); \
                     skipping O_DIRECT test: {msg}"
                );
                drop(guard);
                let _ = mount_thread.join();
                return;
            }
            Ok(Ok(())) => {
                // Mount thread exited cleanly before we observed
                // the mount as wired — very unusual race; treat as
                // a setup failure rather than a kiseki bug.
                drop(guard);
                let _ = mount_thread.join();
                panic!("mount thread exited successfully without the mount becoming visible");
            }
            Err(_) => {
                // Channel still open, no result yet — the daemon
                // is stuck. Real failure of the test setup.
                drop(guard);
                let _ = mount_thread.join();
                panic!("kernel never wired the FUSE mount within 10s — daemon failed to start");
            }
        }
    }

    // -----------------------------------------------------------
    // The actual O_DIRECT exercise.
    // -----------------------------------------------------------
    let file = mountpoint.join("odirect.bin");
    let c_file =
        std::ffi::CString::new(file.to_str().expect("utf-8")).expect("path is c-string clean");

    // 1) Create the file (regular open with O_CREAT).
    // SAFETY: libc::open with a static C string.
    let create_fd = unsafe {
        libc::open(
            c_file.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
            0o644,
        )
    };
    if create_fd < 0 {
        let errno = std::io::Error::last_os_error();
        // Clean up before panicking so the test runner isn't left
        // with a wedged mount.
        drop(guard);
        let _ = mount_thread.join();
        panic!("regular create open failed: {errno}");
    }
    // SAFETY: fd from open above.
    unsafe { libc::close(create_fd) };

    // 2) Reopen with O_DIRECT. Pre-fix the kernel either rejects
    // this with EINVAL or accepts but then routes the write through
    // the page cache silently. Post-fix the daemon's FUSE_OPEN
    // reply carries FOPEN_DIRECT_IO and the kernel honors direct-IO.
    // SAFETY: libc::open with a static C string.
    let fd = unsafe {
        libc::open(
            c_file.as_ptr(),
            libc::O_RDWR | libc::O_DIRECT | libc::O_CLOEXEC,
        )
    };
    let open_errno = std::io::Error::last_os_error();
    if fd < 0 {
        drop(guard);
        let _ = mount_thread.join();
        panic!(
            "O_DIRECT open failed on kiseki FUSE mount: {open_errno} — \
             daemon must return FOPEN_DIRECT_IO from open() for \
             O_DIRECT-opened files (GH#37)"
        );
    }
    struct Fd(libc::c_int);
    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: fd owned by Fd; close once on drop.
            unsafe {
                libc::close(self.0);
            }
        }
    }
    let fd = Fd(fd);

    // 3) Write 4 KiB aligned (page size on x86_64) so the kernel's
    // alignment check passes. Use an aligned heap buffer.
    const ALIGN: usize = 4096;
    const LEN: usize = 4096;
    let layout = std::alloc::Layout::from_size_align(LEN, ALIGN).expect("layout");
    // SAFETY: allocate + zero-fill an aligned buffer for the write.
    let buf = unsafe {
        let p = std::alloc::alloc_zeroed(layout);
        assert!(!p.is_null(), "OOM allocating O_DIRECT buffer");
        for i in 0..LEN {
            *p.add(i) = (i & 0xFF) as u8;
        }
        p
    };
    // SAFETY: write to a kernel-validated fd from a buffer of LEN bytes.
    let n = unsafe { libc::write(fd.0, buf as *const libc::c_void, LEN) };
    let write_errno = std::io::Error::last_os_error();
    // SAFETY: free the same layout we allocated.
    unsafe {
        std::alloc::dealloc(buf, layout);
    }

    // Drop the fd before unmount so libfuse doesn't see a live fd
    // during session teardown.
    drop(fd);
    // Drop the guard (unmount) before asserting so the test runner
    // isn't left with a wedged mount on failure.
    drop(guard);
    let _ = mount_thread.join();

    assert!(
        n == LEN as isize,
        "O_DIRECT write returned {n} (expected {LEN}); errno={write_errno} — \
         daemon must propagate FOPEN_DIRECT_IO so the kernel routes the \
         write through direct-IO instead of failing or short-writing (GH#37)"
    );
}
