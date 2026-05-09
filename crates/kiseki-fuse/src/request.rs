//! Per-request context passed to every [`Filesystem`](crate::filesystem::Filesystem)
//! method.
//!
//! Mirrors libfuse's `fuse_ctx` (caller uid / gid / pid / umask) plus
//! a wrapper-assigned [`RequestId`] used as the bridge map key
//! (I-FUSE-3 + I-FUSE-7).

use std::sync::atomic::{AtomicU64, Ordering};

/// Wrapper-assigned monotonic request identifier.
///
/// Used by the [`Bridge`](crate::bridge::Bridge) as the
/// `DashMap<RequestId, BridgeHandle>` key. Not the same as the kernel's
/// FUSE request unique — libfuse hides that behind the opaque
/// `fuse_req_t`. Allocated in the trampoline before dispatch into the
/// trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

impl RequestId {
    /// Allocate the next id from the global monotonic counter.
    ///
    /// Wraps after 2^64 ids — practically unreachable; one billion
    /// requests/sec for 584 years to wrap.
    pub fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Caller context for a FUSE op, populated from `fuse_req_ctx`.
///
/// The handler can use `caller_uid` / `caller_gid` for tenant identity
/// propagation (per ADR-043 §D6 criterion 2 — the FUSE daemon is
/// single-tenant per mount, so these are advisory rather than the
/// authn surface).
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// Wrapper-assigned monotonic id (bridge map key).
    pub id: RequestId,
    /// Calling process's uid.
    pub caller_uid: u32,
    /// Calling process's gid.
    pub caller_gid: u32,
    /// Calling process's pid.
    pub caller_pid: u32,
    /// File-mode creation mask in effect for the call.
    pub umask: u32,
}
