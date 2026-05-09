//! Error type carried across the bridge boundary.
//!
//! [`FuseError`] is what async handlers send via the bridge's oneshot
//! sender; the bridge converts it to an errno via
//! [`FuseError::to_errno`] and finalizes via `fuse_reply_err`. This
//! keeps callers from having to import `libc` errno constants directly.

use thiserror::Error;

/// Errors that can flow back from a handler through the bridge.
///
/// Most variants map 1:1 to a POSIX errno; [`FuseError::Errno`] is the
/// catch-all for "I already have a numeric errno from the gateway."
///
/// On the libfuse side everything becomes a single integer via
/// [`FuseError::to_errno`]. The structured shape exists because
/// `kiseki-client::fuse_daemon` already speaks in terms of typed
/// gateway errors and benefits from not having to flatten to `i32`
/// at the call site.
#[derive(Debug, Error, Clone, Copy)]
pub enum FuseError {
    /// Resource not found — `ENOENT`.
    #[error("ENOENT (no such file or directory)")]
    NotFound,

    /// I/O error — `EIO`.
    ///
    /// Used as the default for "something went wrong but we don't have a
    /// more specific errno." Also what the bridge reports on timeout
    /// (configurable via [`crate::mount::KisekiFuseConfig::handler_timeout`]).
    #[error("EIO (input/output error)")]
    Io,

    /// Permission denied — `EACCES`.
    #[error("EACCES (permission denied)")]
    AccessDenied,

    /// Operation not supported — `ENOSYS`.
    ///
    /// Reserved for ops the trait declines via the trait's
    /// default impl. ADR-013 §"Supported (full)" ops never reply
    /// with this; it's the libfuse-default for ops outside the
    /// supported matrix (`ioctl`, `bmap`, `poll`, `fallocate`, etc.).
    #[error("ENOSYS (function not implemented)")]
    NotImplemented,

    /// Resource temporarily unavailable — `EAGAIN`.
    ///
    /// Emitted by the wrapper itself (NOT by user handlers) when the
    /// bridge's `max_pending_ops` cap is hit (I-FUSE-7).
    #[error("EAGAIN (resource temporarily unavailable)")]
    TryAgain,

    /// Async handler cancelled — `EINTR`.
    ///
    /// The bridge replies this when its tokio task's future is
    /// dropped before completion (I-FUSE-4). User code does not
    /// produce this variant directly.
    #[error("EINTR (interrupted system call)")]
    Cancelled,

    /// Async handler timed out — `EIO`.
    ///
    /// The bridge replies this when the per-request handler timeout
    /// (default 30 s, [`crate::mount::KisekiFuseConfig::handler_timeout`])
    /// fires before the future completes. The errno is `EIO` rather
    /// than `EINTR` so the kernel doesn't retry indefinitely.
    #[error("EIO (handler timed out)")]
    Timeout,

    /// Pre-mapped errno from the gateway / lower layers.
    ///
    /// Use this when the caller already has a `libc::E*` constant
    /// (typical: `kiseki-client::fuse_fs::gateway_err_to_errno`).
    #[error("errno {0}")]
    Errno(i32),
}

impl FuseError {
    /// Convert to the integer errno passed to `fuse_reply_err`.
    ///
    /// `libc::E*` constants are positive; libfuse expects positive
    /// errno values too (it negates internally for kernel return).
    #[must_use]
    pub const fn to_errno(self) -> i32 {
        match self {
            Self::NotFound => libc::ENOENT,
            Self::Io | Self::Timeout => libc::EIO,
            Self::AccessDenied => libc::EACCES,
            Self::NotImplemented => libc::ENOSYS,
            Self::TryAgain => libc::EAGAIN,
            Self::Cancelled => libc::EINTR,
            Self::Errno(e) => e,
        }
    }
}

impl From<i32> for FuseError {
    fn from(errno: i32) -> Self {
        Self::Errno(errno)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_conversions_round_trip() {
        assert_eq!(FuseError::NotFound.to_errno(), libc::ENOENT);
        assert_eq!(FuseError::Io.to_errno(), libc::EIO);
        assert_eq!(FuseError::Cancelled.to_errno(), libc::EINTR);
        assert_eq!(FuseError::TryAgain.to_errno(), libc::EAGAIN);
        assert_eq!(FuseError::Timeout.to_errno(), libc::EIO);
        assert_eq!(FuseError::Errno(42).to_errno(), 42);
    }
}
