//! Safety-contract tests for `kiseki-fuse` (Acceptance criterion 8 of
//! `specs/implementation/libfuse-swap.md`; I-FUSE-1..I-FUSE-8 in
//! `specs/invariants.md`).
//!
//! These tests exercise the wrapper's own type-system + bookkeeping
//! contracts — they do NOT require `/dev/fuse` or a real mount, so
//! they run on every CI lane (Tier 1).
//!
//! No unsafe code: the I-FUSE-4 zeroize proof uses a counter-based
//! custom `Zeroize` impl (see `CountingZeroize` below) rather than
//! post-drop memory probing, which is racy under any allocator that
//! reuses freed pages.
//!
//! The compile-fail aspect of I-FUSE-3 + I-FUSE-6 (reply tokens are
//! `!Send`) is checked by the `compile_fail` module below — gated
//! behind `#[cfg(any())]`, never compiled, but uncommenting any
//! block must still cause `cargo build -p kiseki-fuse --tests` to
//! reject (`*mut fuse_req` does not auto-impl `Send`). A
//! `trybuild`-driven runner would automate this; that's a Phase 1c
//! follow-up.

use std::time::Duration;

use kiseki_fuse::{Bridge, FuseError, KisekiFuseConfig, ZeroOnCancel};

// =============================================================================
// I-FUSE-4 — ZeroOnCancel calls inner.zeroize() on drop.
//
// We use a `Zeroize` impl that increments a counter rather than
// probing freed allocator pages — the latter is racy.
// =============================================================================

use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingZeroize {
    counter: &'static AtomicUsize,
}

impl zeroize::Zeroize for CountingZeroize {
    fn zeroize(&mut self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn zero_on_cancel_invokes_zeroize_on_drop() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let z = ZeroOnCancel::new(CountingZeroize { counter: &COUNT });
    assert_eq!(COUNT.load(Ordering::SeqCst), 0);
    drop(z);
    assert_eq!(
        COUNT.load(Ordering::SeqCst),
        1,
        "zeroize not called on drop"
    );
}

#[test]
fn zero_on_cancel_into_inner_preserves_value_without_zeroize() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let z = ZeroOnCancel::new(CountingZeroize { counter: &COUNT });
    let _inner = z.into_inner();
    assert_eq!(
        COUNT.load(Ordering::SeqCst),
        0,
        "into_inner must not zeroize (caller owns inner now)"
    );
}

// =============================================================================
// I-FUSE-7 — max_pending_ops cap rejects with EAGAIN.
// =============================================================================

#[test]
fn max_pending_ops_cap_returns_try_again() {
    use kiseki_fuse::RequestId;

    let _rt = build_runtime();
    let bridge = test_bridge_with_cap(2);
    assert!(bridge.try_claim_slot_for_test(RequestId(1)).is_ok());
    assert!(bridge.try_claim_slot_for_test(RequestId(2)).is_ok());
    let result = bridge.try_claim_slot_for_test(RequestId(3));
    assert!(matches!(result, Err(FuseError::TryAgain)));
    assert_eq!(bridge.in_flight(), 2);

    bridge.release_slot_for_test(RequestId(1));
    assert!(bridge.try_claim_slot_for_test(RequestId(3)).is_ok());
    assert_eq!(bridge.in_flight(), 2);
}

// =============================================================================
// I-FUSE-8 — session-thread crash translates to FuseError on PropagateError.
//
// The Abort default path cannot be tested without spawning a child
// process (it calls `std::process::abort()`); we exercise
// PropagateError, which surfaces the underlying join result for tests.
// =============================================================================

#[test]
fn session_thread_panic_propagates_as_io_error() {
    use kiseki_fuse::session::{CrashBehavior, SessionThread};

    // Simulate a session thread that panics. We can't actually drive
    // `fuse_session_loop_mt_31` without /dev/fuse, but the watchdog
    // is library code that wraps an arbitrary JoinHandle, so we
    // exercise it via a synthetic spawn.
    let session_thread = SessionThread::synthetic_for_test(
        std::thread::Builder::new()
            .spawn(|| {
                panic!("synthetic session-thread panic for I-FUSE-8 test");
            })
            .expect("spawn synthetic thread"),
    );
    let watchdog = session_thread.with_watchdog(CrashBehavior::PropagateError);
    let result = watchdog.join();
    assert!(matches!(result, Err(FuseError::Io)));
}

// =============================================================================
// Smoke: KisekiFuseConfig defaults + auto_remount=true is rejected.
// =============================================================================

#[test]
fn auto_remount_true_is_not_yet_implemented() {
    use std::path::PathBuf;
    use std::sync::Arc;

    use kiseki_fuse::{mount, Filesystem};

    struct NopFs;
    impl Filesystem for NopFs {}

    let _rt = build_runtime();
    let config = KisekiFuseConfig {
        auto_remount: true,
        ..KisekiFuseConfig::default()
    };
    let fs: Arc<dyn Filesystem> = Arc::new(NopFs);
    let result = mount(fs, &PathBuf::from("/tmp/never-mounted"), config);
    assert!(matches!(result, Err(FuseError::NotImplemented)));
}

#[test]
fn config_defaults_match_invariants() {
    let cfg = KisekiFuseConfig::default();
    assert_eq!(cfg.handler_timeout, Duration::from_secs(30));
    assert_eq!(cfg.max_pending_ops, 1024);
    assert!(!cfg.auto_remount);
}

// =============================================================================
// Helpers — build a tokio runtime + a bridge with a small cap. Bridge
// has no public constructor; we add a test-only shim under
// `kiseki_fuse::Bridge` (see lib.rs).
// =============================================================================

fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn test_bridge_with_cap(cap: u64) -> Bridge {
    let rt = build_runtime();
    let handle = rt.handle().clone();
    // Leak the runtime so the bridge's spawn() (if used) has a live
    // executor. Test-only.
    std::mem::forget(rt);
    Bridge::for_test(handle, Duration::from_millis(100), cap)
}

// =============================================================================
// I-FUSE-3 + I-FUSE-6 — reply tokens are NOT Send / Sync.
//
// This is a compile-time check rather than a runtime test: the
// `static_assertions::assert_not_impl_all` macro would do it cleanly,
// but adding a dev-dep was outside Phase 1b's scope. The blocks
// below use Rust's own type-system to assert: if a block compiles,
// the contract is BROKEN. Each block is wrapped in `#[cfg(any())]`
// so it's never actually compiled — the test is "uncomment the block
// and cargo build must reject."
// =============================================================================

#[cfg(any())]
mod compile_fail {
    // COMPILE-FAIL: ReplyAttr is !Send. Sending it across `tokio::spawn`
    // must not compile. (Verifying I-FUSE-3 + I-FUSE-6.)
    fn reply_attr_must_not_be_send() {
        use kiseki_fuse::reply::ReplyAttr;
        fn assert_send<T: Send>(_: T) {}
        // SAFETY: never executed; we only assert compile-time rejection.
        let r: ReplyAttr = unsafe { std::mem::zeroed() };
        assert_send(r); // expected: error[E0277] `*mut fuse_req` cannot be sent
    }
}
