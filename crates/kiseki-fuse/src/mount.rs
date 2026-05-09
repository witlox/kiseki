//! Public mount entry — the surface `kiseki-client::fuse_daemon`
//! (Phase 2) calls.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::bridge::{Bridge, BridgeConfig};
use crate::error::FuseError;
use crate::filesystem::Filesystem;
use crate::session::{CrashBehavior, Session, WatchdogHandle};

/// Mount-time configuration.
///
/// Carries safety-contract knobs (`handler_timeout`, `max_pending_ops`,
/// `auto_remount`) and libfuse mount options (`mount_options`,
/// `clone_fd`).
#[derive(Debug, Clone)]
pub struct KisekiFuseConfig {
    /// Per-request bridged-handler timeout (I-FUSE-3.1). Default 30 s.
    pub handler_timeout: Duration,

    /// Max in-flight bridged ops (I-FUSE-7). Default 1024.
    pub max_pending_ops: u64,

    /// On session-thread crash (I-FUSE-8), abort the process (default,
    /// fail-fast) or attempt a single re-mount before falling back
    /// to abort. **Auto-remount is not yet wired in Phase 1b**;
    /// setting `true` returns `FuseError::NotImplemented` from
    /// [`mount`]. Track in `specs/implementation/libfuse-swap.md`
    /// follow-up.
    pub auto_remount: bool,

    /// Behavior on session-thread crash. [`CrashBehavior::Abort`]
    /// matches the pre-swap process-crash shape; only override for
    /// tests + library consumers that want to inspect the error.
    pub crash_behavior: CrashBehavior,

    /// Mount options passed to libfuse (e.g. `["-o", "default_permissions"]`).
    pub mount_options: Vec<String>,
}

impl Default for KisekiFuseConfig {
    fn default() -> Self {
        Self {
            handler_timeout: Duration::from_secs(30),
            max_pending_ops: 1024,
            auto_remount: false,
            crash_behavior: CrashBehavior::Abort,
            mount_options: Vec::new(),
        }
    }
}

/// Mount a [`Filesystem`] at `mountpoint` using libfuse's low-level
/// API on a dedicated `kiseki-fuse-session` thread.
///
/// Returns a [`WatchdogHandle`] the caller can `.join()` to wait for
/// graceful unmount, OR drop to relinquish ownership (the watchdog
/// keeps running until the session thread exits).
///
/// # Errors
///
/// - `FuseError::NotImplemented` if `config.auto_remount = true`
///   (not yet wired).
/// - `FuseError::Errno(libc::EIO)` on `fuse_session_new_versioned`
///   or `fuse_session_mount` failure.
/// - `FuseError::Errno(libc::EAGAIN)` on session-thread spawn
///   failure (rare — process is out of thread slots).
pub fn mount(
    fs: Arc<dyn Filesystem>,
    mountpoint: &Path,
    config: KisekiFuseConfig,
) -> Result<WatchdogHandle, FuseError> {
    if config.auto_remount {
        return Err(FuseError::NotImplemented);
    }
    let runtime =
        tokio::runtime::Handle::try_current().map_err(|_| FuseError::Errno(libc::ENOENT))?;
    let bridge = Bridge::new(
        runtime,
        BridgeConfig {
            handler_timeout: config.handler_timeout,
            max_pending_ops: config.max_pending_ops,
        },
    );
    let session = Session::create(fs, bridge, mountpoint, &config.mount_options)?;
    let session_thread = session.run_on_dedicated_thread()?;
    Ok(session_thread.with_watchdog(config.crash_behavior))
}
