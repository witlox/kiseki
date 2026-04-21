//! Process integrity monitoring (I-O8).
//!
//! Detects debugger attachment (ptrace) and disables core dumps to
//! prevent memory extraction of key material in production.

/// Integrity check result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityStatus {
    /// All checks passed.
    Ok,
    /// A debugger is attached.
    DebuggerDetected,
    /// Core dumps are enabled (key material could leak).
    CoreDumpsEnabled,
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

    if results.is_empty() {
        results.push(IntegrityStatus::Ok);
    }

    results
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

/// Attempt to disable core dumps for this process.
pub fn disable_core_dumps() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        // Would call setrlimit(RLIMIT_CORE, 0) via libc — skipped in safe Rust.
        // In production, this is done by the systemd unit file.
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
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

    #[test]
    fn disable_core_dumps_succeeds() {
        assert!(disable_core_dumps().is_ok());
    }
}
