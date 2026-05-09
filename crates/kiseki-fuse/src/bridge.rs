//! Async bridge — moves [`Reply*`](crate::reply) tokens across the
//! libfuse-session-thread / tokio-task boundary (I-FUSE-3 + I-FUSE-7).
//!
//! Reply tokens hold a raw `*mut sys::fuse_req` and are `!Send` by
//! construction (raw pointers don't auto-impl `Send`). The bridge is
//! the *only* way for a handler to finalize a reply on a tokio task,
//! and it does so via a typed [`BridgedReply`] envelope that wraps
//! the raw pointer with `unsafe impl Send` — narrow, audited, single-
//! purpose.
//!
//! # Bridge lifecycle for one request
//!
//! 1. The trampoline allocates a [`RequestId`]
//!    and registers a slot in the in-flight `DashMap`. If the cap
//!    `max_pending_ops` is hit, the trampoline replies `EAGAIN`
//!    immediately and returns (I-FUSE-7).
//! 2. The trampoline calls into the user's [`Filesystem`](crate::filesystem::Filesystem)
//!    impl synchronously. The handler either:
//!    - consumes the reply token directly (sync path: reply finalizes
//!      on the libfuse session thread, no bridge involvement), or
//!    - calls [`Bridge::spawn`] which takes the reply token + a future
//!      and finalizes via the bridge after the future completes.
//! 3. If `Bridge::spawn` is used, the future runs on the configured
//!    tokio runtime under a [`tokio::time::timeout`] (I-FUSE-3.1
//!    default 30 s). Cancellation drops the future, the wrapper
//!    sends `EINTR` (I-FUSE-4); timeout sends [`FuseError::Timeout`].
//! 4. On any terminal state (success / error / cancel / timeout) the
//!    in-flight slot is removed, freeing capacity for new ops.
//!
//! # Cap accounting (I-FUSE-7)
//!
//! `max_pending_ops` is enforced via an `AtomicU64` counter alongside
//! the `DashMap`. The trampoline checks-and-increments atomically; if
//! the post-increment value exceeds the cap, the trampoline rolls back
//! the increment and replies `EAGAIN` to libfuse. (Using the DashMap's
//! own `len()` for the check would race against concurrent inserts.)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use kiseki_fuse_sys as sys;
use tokio::runtime::Handle;

use crate::error::FuseError;
use crate::reply::ReplyToken;
use crate::request::RequestId;

/// `Send` envelope around a libfuse `*mut fuse_req` and the op name.
///
/// Used internally by [`Bridge`] to move a finalize-target across the
/// session-thread / tokio-task boundary without losing the I-FUSE-1
/// drop-without-consume protection. The envelope's `Drop` runs the
/// same EIO-and-counter logic as the reply tokens themselves.
///
/// # Safety
///
/// The wrapping `unsafe impl Send` is sound because libfuse documents
/// `fuse_reply_*` as thread-safe with respect to a single `fuse_req_t`
/// (each request handle is finalized exactly once, from any thread).
/// We hold the only Rust reference to the pointer between
/// `BridgedReply::new` and `BridgedReply::into_raw` (both private).
pub struct BridgedReply {
    /// `None` means the original reply was already in the consumed
    /// state when handed to the bridge — no-op finalize.
    raw: Option<RawBridged>,
}

struct RawBridged {
    req: *mut sys::fuse_req,
    op: &'static str,
}

// SAFETY: libfuse documents `fuse_reply_*` as thread-safe for a given
// request handle (one finalize per request, callable from any thread).
// The bridge holds the unique Rust reference to `req` between
// `BridgedReply::new` and `into_raw`, so concurrent finalize is
// impossible.
unsafe impl Send for RawBridged {}

impl BridgedReply {
    /// Build a bridged envelope from a raw libfuse request handle.
    ///
    /// # Safety
    ///
    /// Caller is the [`reply::ReplyToken::into_bridged`](crate::reply::ReplyToken::into_bridged)
    /// implementation. `req` must be the still-pending request the
    /// reply token wrapped, and must not be finalized via any other
    /// path until [`BridgedReply::into_raw`] is called by the bridge.
    pub(crate) unsafe fn new(req: *mut sys::fuse_req, op: &'static str) -> Self {
        Self {
            raw: Some(RawBridged { req, op }),
        }
    }

    /// Build a no-op envelope. Used when the reply token was already
    /// consumed before reaching the bridge.
    pub(crate) const fn dummy(_op: &'static str) -> Self {
        Self { raw: None }
    }

    /// Pull out the raw pointer + op name for finalization.
    ///
    /// `None` means the envelope was a dummy (already-consumed token);
    /// the caller should skip finalization.
    pub(crate) fn into_raw(mut self) -> Option<(*mut sys::fuse_req, &'static str)> {
        let parts = self.raw.take()?;
        // bypass our Drop — we'll finalize manually.
        std::mem::forget(self);
        Some((parts.req, parts.op))
    }
}

impl Drop for BridgedReply {
    fn drop(&mut self) {
        if let Some(parts) = self.raw.take() {
            // Bridged envelope dropped without finalize — same I-FUSE-2
            // shape as a Reply* token, redirected through the same
            // counter.
            // SAFETY: see RawBridged unsafe impl Send.
            unsafe {
                sys::fuse_reply_err(parts.req, libc::EIO);
            }
            tracing::warn!(
                op = parts.op,
                "kiseki-fuse: BridgedReply dropped without finalize; replied EIO"
            );
            #[cfg(debug_assertions)]
            panic!(
                "kiseki-fuse: BridgedReply for op={} dropped without finalize",
                parts.op
            );
        }
    }
}

/// Configuration the bridge needs from [`KisekiFuseConfig`](crate::mount::KisekiFuseConfig).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BridgeConfig {
    pub handler_timeout: Duration,
    pub max_pending_ops: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            handler_timeout: Duration::from_secs(30),
            max_pending_ops: 1024,
        }
    }
}

/// In-flight request handle stored in the bridge's DashMap.
///
/// Keeping the slot occupies bridge capacity; it is removed when the
/// finalize task completes. The handle is a unit type today — the
/// bridge does not hold per-request state beyond its presence in the
/// map — but the type exists so future work (cancel-on-key,
/// per-request observability) has a place to land without churning
/// the public API.
struct BridgeSlot;

/// The async bridge.
///
/// One bridge per mount; created by [`mount`](crate::mount::mount) and
/// passed into the [`Filesystem`](crate::filesystem::Filesystem) impl
/// via [`OpContext`](crate::filesystem::OpContext) on every op.
#[derive(Clone)]
pub struct Bridge {
    inner: Arc<BridgeInner>,
}

struct BridgeInner {
    in_flight: DashMap<RequestId, BridgeSlot>,
    pending_count: AtomicU64,
    runtime: Handle,
    config: BridgeConfig,
}

impl Bridge {
    /// Build a bridge from a tokio runtime handle + config.
    pub(crate) fn new(runtime: Handle, config: BridgeConfig) -> Self {
        Self {
            inner: Arc::new(BridgeInner {
                in_flight: DashMap::new(),
                pending_count: AtomicU64::new(0),
                runtime,
                config,
            }),
        }
    }

    /// Try to claim a slot for a new in-flight request. `Ok` means
    /// the slot is reserved (caller must call
    /// [`Bridge::release_slot`] when the request finalizes); `Err`
    /// means the cap is hit (caller should reply `EAGAIN`).
    ///
    /// I-FUSE-7. The check-and-increment is atomic via
    /// `compare_exchange_weak`-loop so concurrent trampolines cannot
    /// overshoot the cap.
    pub(crate) fn try_claim_slot(&self, id: RequestId) -> Result<(), FuseError> {
        let cap = self.inner.config.max_pending_ops;
        let mut current = self.inner.pending_count.load(Ordering::Acquire);
        loop {
            if current >= cap {
                return Err(FuseError::TryAgain);
            }
            match self.inner.pending_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.inner.in_flight.insert(id, BridgeSlot);
        Ok(())
    }

    /// Release a previously-claimed slot. Idempotent.
    pub(crate) fn release_slot(&self, id: RequestId) {
        if self.inner.in_flight.remove(&id).is_some() {
            self.inner.pending_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Current in-flight count — exposed for tests + observability.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.inner.pending_count.load(Ordering::Acquire)
    }

    /// Configured cap.
    #[must_use]
    pub fn max_pending_ops(&self) -> u64 {
        self.inner.config.max_pending_ops
    }

    // ----- test-only helpers (`#[doc(hidden)]`; not part of stable API) -----

    /// Test-only: build a bridge with an explicit cap + timeout.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(runtime: Handle, handler_timeout: Duration, max_pending_ops: u64) -> Self {
        Self::new(
            runtime,
            BridgeConfig {
                handler_timeout,
                max_pending_ops,
            },
        )
    }

    /// Test-only: claim a slot directly, bypassing `spawn`. Used by
    /// `tests/safety_contract.rs` to exercise I-FUSE-7 in isolation.
    #[doc(hidden)]
    pub fn try_claim_slot_for_test(&self, id: RequestId) -> Result<(), FuseError> {
        self.try_claim_slot(id)
    }

    /// Test-only: release a slot directly.
    #[doc(hidden)]
    pub fn release_slot_for_test(&self, id: RequestId) {
        self.release_slot(id);
    }

    /// Spawn an async handler. Takes ownership of a reply token,
    /// converts it to a `Send` envelope, and runs the future on the
    /// configured tokio runtime under a per-request timeout. Sends
    /// the appropriate finalize call to libfuse on completion.
    ///
    /// # Cancellation
    ///
    /// If the spawned tokio task's future is cancelled (the JoinHandle
    /// dropped, or `tokio::task::abort`), the bridge replies `EINTR`
    /// to libfuse — I-FUSE-4. The user's plaintext buffers should be
    /// wrapped in [`ZeroOnCancel`](crate::zeroize::ZeroOnCancel) so
    /// cancellation also zeroes any captured bytes.
    ///
    /// # Timeout
    ///
    /// If the future does not complete within
    /// [`KisekiFuseConfig::handler_timeout`](crate::mount::KisekiFuseConfig)
    /// (default 30 s), the bridge replies [`FuseError::Timeout`] →
    /// `EIO` — I-FUSE-3.1.
    pub fn spawn<R, F>(&self, request_id: RequestId, reply: R, fut: F)
    where
        R: ReplyToken + 'static,
        F: std::future::Future<Output = Result<R::Output, FuseError>> + Send + 'static,
    {
        // I-FUSE-7: enforce max_pending_ops by claim-or-EAGAIN. If
        // the cap is hit, finalize the reply with EAGAIN inline and
        // return — never schedule the future.
        if self.try_claim_slot(request_id).is_err() {
            let bridged = reply.into_bridged();
            if let Some((req, _op)) = bridged.into_raw() {
                // SAFETY: bridged owns `req` and we are about to
                // finalize it exactly once. EAGAIN is the I-FUSE-7
                // backpressure errno.
                unsafe {
                    sys::fuse_reply_err(req, libc::EAGAIN);
                }
            }
            return;
        }
        let bridged = reply.into_bridged();
        let timeout = self.inner.config.handler_timeout;
        let bridge = self.clone();
        self.inner.runtime.spawn(async move {
            // Run the user's future under a timeout. tokio's timeout
            // returns Err(_) on the deadline; we fall through to
            // FuseError::Timeout. Cancellation of THIS spawn's
            // JoinHandle drops the future (and any ZeroOnCancel
            // buffers it owns); the bridged envelope's Drop will
            // *not* run because we explicitly route through
            // finalize_bridged below — UNLESS the entire spawn is
            // dropped before this awaits, in which case `bridged`
            // drops with EIO. To get the explicit EINTR-on-cancel
            // semantics, we wrap the await in a guard: if the
            // future panics or is cancelled BEFORE timeout fires,
            // we finalize with EINTR via the guard's Drop.
            let guard = CancelGuard::new(bridged);
            let result = match tokio::time::timeout(timeout, fut).await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(FuseError::Timeout),
            };
            // Reaching here means the future completed (no cancel).
            // Disarm the guard and route through finalize.
            let bridged = guard.disarm();
            R::finalize_bridged(bridged, result);
            bridge.release_slot(request_id);
        });
    }
}

/// Drop-guard around a [`BridgedReply`] that converts task
/// cancellation into a FUSE `EINTR` finalize (I-FUSE-4).
///
/// While the task is running, the guard owns the bridged reply. On
/// successful completion the handler calls [`CancelGuard::disarm`],
/// which extracts the bridged reply and lets the regular finalize
/// path consume it. If `disarm` is *not* called (the future was
/// cancelled mid-await, or panicked), the guard's `Drop` runs and
/// finalizes with `EINTR`.
struct CancelGuard {
    bridged: Option<BridgedReply>,
}

impl CancelGuard {
    fn new(bridged: BridgedReply) -> Self {
        Self {
            bridged: Some(bridged),
        }
    }

    fn disarm(mut self) -> BridgedReply {
        self.bridged
            .take()
            .expect("kiseki-fuse: CancelGuard::disarm called twice (BUG)")
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(bridged) = self.bridged.take() {
            // Future was cancelled — finalize EINTR.
            if let Some((req, _op)) = bridged.into_raw() {
                // SAFETY: `req` is the bridge's owned request handle;
                // see RawBridged::Send. Single-finalize is preserved
                // because BridgedReply::into_raw consumes the option.
                unsafe {
                    sys::fuse_reply_err(req, libc::EINTR);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_handle() -> Handle {
        // tokio::runtime::Handle requires a live runtime. Build one
        // we own and leak — the bridge just needs `spawn` to work.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("kiseki-fuse: build tokio runtime in test");
        let h = rt.handle().clone();
        // Leak the runtime so it stays alive for the test process.
        std::mem::forget(rt);
        h
    }

    #[test]
    fn slot_cap_blocks_at_limit() {
        let bridge = Bridge::new(
            fake_handle(),
            BridgeConfig {
                handler_timeout: Duration::from_millis(100),
                max_pending_ops: 2,
            },
        );
        assert!(bridge.try_claim_slot(RequestId(1)).is_ok());
        assert!(bridge.try_claim_slot(RequestId(2)).is_ok());
        // Cap hit.
        assert!(matches!(
            bridge.try_claim_slot(RequestId(3)),
            Err(FuseError::TryAgain)
        ));
        // Releasing makes capacity again.
        bridge.release_slot(RequestId(1));
        assert!(bridge.try_claim_slot(RequestId(3)).is_ok());
    }

    #[test]
    fn release_is_idempotent() {
        let bridge = Bridge::new(
            fake_handle(),
            BridgeConfig {
                handler_timeout: Duration::from_millis(100),
                max_pending_ops: 4,
            },
        );
        bridge.try_claim_slot(RequestId(7)).expect("claim succeeds");
        bridge.release_slot(RequestId(7));
        bridge.release_slot(RequestId(7));
        assert_eq!(bridge.in_flight(), 0);
    }
}
