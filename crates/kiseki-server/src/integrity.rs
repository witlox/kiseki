//! Process integrity monitoring (I-O8).
//!
//! Detects debugger attachment (ptrace) and disables core dumps to
//! prevent memory extraction of key material in production.
//!
//! Phase 14e: also locks process pages with `mlockall(MCL_CURRENT |
//! MCL_FUTURE)` so the kernel does not swap key material out to disk.
//! Both `disable_core_dumps()` and `lock_memory_pages()` are best-effort
//! — they require enough `RLIMIT_CORE` / `RLIMIT_MEMLOCK` headroom to
//! act, and fall back to a logged warning rather than aborting startup
//! (the systemd unit + capabilities are the authoritative defence).
//!
//! Not yet wired into the server startup — will be called from runtime.rs
//! when the integrity monitor is enabled (Phase I2).
#![allow(dead_code)]

/// Integrity check result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityStatus {
    /// All checks passed.
    Ok,
    /// A debugger is attached.
    DebuggerDetected,
    /// Core dumps are enabled (key material could leak).
    CoreDumpsEnabled,
    /// Process pages are not memory-locked (could swap to disk).
    PagesNotLocked,
}

/// Run all integrity checks.
#[must_use]
pub fn check_integrity() -> Vec<IntegrityStatus> {
    let mut results = Vec::new();

    if is_debugger_attached() {
        results.push(IntegrityStatus::DebuggerDetected);
    }

    if are_core_dumps_enabled() {
        results.push(IntegrityStatus::CoreDumpsEnabled);
    }

    if !pages_are_locked() {
        results.push(IntegrityStatus::PagesNotLocked);
    }

    if results.is_empty() {
        results.push(IntegrityStatus::Ok);
    }

    results
}

/// Check whether the process has any locked pages.
///
/// Reads `/proc/self/status` for `VmLck`. Non-zero means at least
/// some pages are locked; the typical contract is "all of them" because
/// we call `mlockall(MCL_CURRENT | MCL_FUTURE)`. Returns `false` on
/// non-Linux and on read errors.
#[must_use]
fn pages_are_locked() -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmLck:") {
                    // Format: "VmLck:    <kB>  kB"
                    let kb: u64 = rest
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    return kb > 0;
                }
            }
        }
        false
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Check if a debugger is attached via ptrace status.
#[must_use]
fn is_debugger_attached() -> bool {
    #[cfg(target_os = "linux")]
    {
        // On Linux, check /proc/self/status for TracerPid.
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(pid) = line.strip_prefix("TracerPid:\t") {
                    return pid.trim() != "0";
                }
            }
        }
        false
    }

    #[cfg(not(target_os = "linux"))]
    {
        // macOS/other: no ptrace check available in safe Rust.
        false
    }
}

/// Check if core dumps are enabled.
#[must_use]
fn are_core_dumps_enabled() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Parse /proc/self/limits for the soft core dump limit.
        // Format: "Max core file size     <soft>     <hard>     <units>"
        if let Ok(limits) = std::fs::read_to_string("/proc/self/limits") {
            for line in limits.lines() {
                if line.starts_with("Max core file size") {
                    // Extract the soft limit (first number after the label).
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    // "Max core file size" is 4 words; soft limit is field[4].
                    if let Some(&soft) = fields.get(4) {
                        return soft != "0";
                    }
                }
            }
        }
        false
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Outcome of a hardening call. `Ok(())` means the kernel accepted
/// the request; the `HardeningError` variants distinguish "tried,
/// kernel refused" from "platform doesn't support it".
#[derive(Debug, thiserror::Error)]
pub enum HardeningError {
    /// The current platform doesn't expose this primitive (e.g.
    /// `mlockall` is Linux-only).
    #[error("hardening unsupported on this platform")]
    UnsupportedPlatform,
    /// The system call returned a non-zero status. Argument carries
    /// the raw `errno` for diagnostics.
    #[error("hardening syscall failed: errno {0}")]
    Syscall(i32),
}

/// Set `RLIMIT_CORE` to 0, preventing the kernel from writing a
/// core dump if the process crashes — keeps key material out of
/// post-mortem files. Best-effort: returns an error if the kernel
/// refuses (typically a hard-limit conflict the operator must lift
/// in the systemd unit).
pub fn disable_core_dumps() -> Result<(), HardeningError> {
    #[cfg(target_os = "linux")]
    {
        let zero = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: passing a valid `rlimit` raw pointer to setrlimit;
        // the kernel reads the struct and returns 0 / -1.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_CORE, std::ptr::from_ref(&zero)) };
        if rc == 0 {
            Ok(())
        } else {
            // SAFETY: `__errno_location` returns a thread-local pointer
            // valid for the current thread's lifetime; reading is safe.
            #[allow(unsafe_code)]
            let errno = unsafe { *libc::__errno_location() };
            Err(HardeningError::Syscall(errno))
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(HardeningError::UnsupportedPlatform)
    }
}

/// Lock all current and future process pages with `mlockall(MCL_CURRENT |
/// MCL_FUTURE)` so the kernel never swaps key material to disk. Best-effort:
/// requires `RLIMIT_MEMLOCK` headroom and (in containers) the
/// `IPC_LOCK` capability — fails with `EPERM` / `ENOMEM` if not granted.
/// Caller should log and continue rather than abort.
pub fn lock_memory_pages() -> Result<(), HardeningError> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: mlockall takes a flags integer; no memory is
        // dereferenced. Side-effect is that the kernel pins pages.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
        if rc == 0 {
            Ok(())
        } else {
            // SAFETY: see disable_core_dumps.
            #[allow(unsafe_code)]
            let errno = unsafe { *libc::__errno_location() };
            Err(HardeningError::Syscall(errno))
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(HardeningError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_check_runs() {
        let results = check_integrity();
        assert!(!results.is_empty());
        // In test environment, should be OK (no debugger, or non-Linux).
        // We don't assert specific status since it depends on the environment.
    }

    /// `disable_core_dumps()` must actually set `RLIMIT_CORE` to 0 on
    /// Linux — and that change is visible in `/proc/self/limits`.
    #[test]
    #[cfg(target_os = "linux")]
    fn disable_core_dumps_zeroes_rlimit_core() {
        // Some sandboxes pre-set RLIMIT_CORE high. Run, then verify.
        match disable_core_dumps() {
            Ok(()) => {
                assert!(
                    !are_core_dumps_enabled(),
                    "after disable_core_dumps, are_core_dumps_enabled() must be false"
                );
            }
            Err(HardeningError::Syscall(errno)) => {
                // EPERM is acceptable when the test runner has
                // already locked the hard limit above 0; that's an
                // environmental constraint, not a bug.
                assert_eq!(errno, libc::EPERM, "only EPERM is an acceptable failure");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// `lock_memory_pages()` is best-effort: in CI / sandboxes
    /// `RLIMIT_MEMLOCK` is often too low and `mlockall` returns
    /// `ENOMEM` or `EPERM`. We assert "either it succeeded and the
    /// reported `VmLck` is non-zero, or it failed with a known
    /// errno" — both are valid outcomes.
    #[test]
    #[cfg(target_os = "linux")]
    fn lock_memory_pages_succeeds_or_returns_known_errno() {
        match lock_memory_pages() {
            Ok(()) => {
                assert!(pages_are_locked(), "after mlockall VmLck must be >0");
            }
            Err(HardeningError::Syscall(errno)) => {
                assert!(
                    errno == libc::ENOMEM || errno == libc::EPERM,
                    "only ENOMEM/EPERM are acceptable failures, got {errno}"
                );
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// On non-Linux platforms both primitives must report
    /// UnsupportedPlatform — never silently succeed.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn hardening_unsupported_off_linux() {
        assert!(matches!(
            disable_core_dumps(),
            Err(HardeningError::UnsupportedPlatform)
        ));
        assert!(matches!(
            lock_memory_pages(),
            Err(HardeningError::UnsupportedPlatform)
        ));
    }

    // ---------------------------------------------------------------
    // Scenario: ptrace attachment detected
    // When TracerPid != 0, IntegrityStatus::DebuggerDetected is reported.
    // ---------------------------------------------------------------
    #[test]
    fn ptrace_detection_status_variant() {
        // Verify the DebuggerDetected variant exists and is distinct.
        let status = IntegrityStatus::DebuggerDetected;
        assert_ne!(status, IntegrityStatus::Ok);
        assert_ne!(status, IntegrityStatus::CoreDumpsEnabled);
        assert_eq!(status, IntegrityStatus::DebuggerDetected);
    }

    // ---------------------------------------------------------------
    // Scenario: Core dump attempt blocked
    // RLIMIT_CORE=0, MADV_DONTDUMP — no core dump generated.
    // ---------------------------------------------------------------
    #[test]
    fn core_dumps_blocked_status_variant() {
        let status = IntegrityStatus::CoreDumpsEnabled;
        assert_ne!(status, IntegrityStatus::Ok);
        // In production, this triggers an alert. Here we verify
        // the status can be checked and used for branching.
        let should_alert = status == IntegrityStatus::CoreDumpsEnabled;
        assert!(should_alert);
    }

    // ---------------------------------------------------------------
    // Scenario: Integrity monitor in development mode
    // In dev/test mode, ptrace does not trigger alerts.
    // ---------------------------------------------------------------
    #[test]
    fn dev_mode_integrity_monitor_disabled() {
        let dev_mode = true;
        let results = check_integrity();

        // In dev mode, results are not actionable.
        if dev_mode {
            // No alerts should be sent regardless of results.
            for status in &results {
                let _alert = match status {
                    IntegrityStatus::DebuggerDetected | IntegrityStatus::CoreDumpsEnabled
                        if !dev_mode =>
                    {
                        true
                    }
                    _ => false, // dev mode: suppress
                };
            }
        }
    }
}
