//! Shared probe helpers for the RDMA bindings (ibverbs, libfabric).
//! ADR-042 §2.3 path-validation discipline (R2-M2).
//!
//! Every system-library probe (`libibverbs.so.1`, `libfabric.so.1`)
//! follows the same pattern:
//!
//! 1. Search a fixed list of distro/arch absolute paths for the
//!    library file.
//! 2. Validate the resolved file is owned by root (uid 0) and not
//!    group/world-writable.
//! 3. Audit-log the resolved absolute path so a path-injection
//!    attempt leaves a trace.
//! 4. `dlopen` the validated absolute path.
//!
//! Path-injection via `LD_LIBRARY_PATH` would expose a supply-chain
//! attack surface; the absolute-path + ownership/permissions check
//! eliminates it (closes round-1 M1).

use std::path::{Path, PathBuf};

/// Distro-aware search-list builder for a system library. Same shape
/// for libibverbs and libfabric — they only differ in the leaf
/// filename. The `${arch}` slot is filled from the build target.
///
/// Returns paths in the order ADR-042 §2.3 lists them: Debian/Ubuntu
/// (`/usr/lib/${arch}-linux-gnu/`), RHEL/SUSE/Rocky (`/usr/lib64/`),
/// then Alpine (`/usr/lib/`).
#[must_use]
pub fn system_library_search_paths(leaf: &str) -> Vec<PathBuf> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "powerpc64") {
        "powerpc64le"
    } else {
        // Fall back to a sentinel that won't resolve; the probe's
        // file-existence check will skip it cleanly.
        "unknown"
    };
    vec![
        PathBuf::from(format!("/usr/lib/{arch}-linux-gnu/{leaf}")),
        PathBuf::from(format!("/usr/lib64/{leaf}")),
        PathBuf::from(format!("/usr/lib/{leaf}")),
    ]
}

/// Outcome of a per-path probe attempt. The probe loop walks the
/// search list and returns the first `Found` result, or accumulates
/// rejection reasons for diagnostics.
#[derive(Debug, PartialEq, Eq)]
pub enum PathOutcome {
    /// Path resolved + passed every safety check.
    Found {
        /// The validated absolute path.
        path: PathBuf,
    },
    /// Path doesn't exist. Continue to next candidate.
    NotPresent {
        /// Path that wasn't there.
        path: PathBuf,
    },
    /// Path exists but failed validation. Refuse to dlopen.
    /// Continue to next candidate; audit the rejection.
    RejectedUnsafe {
        /// Path that was rejected.
        path: PathBuf,
        /// Why (root-ownership / write-bits / etc).
        reason: String,
    },
}

/// Validate one candidate path per ADR-042 §2.3 R2-M2.
///
/// Checks: file exists, owned by root (uid 0), not group/world-
/// writable.
#[cfg(target_os = "linux")]
#[must_use]
pub fn validate_candidate(path: &Path) -> PathOutcome {
    use std::os::linux::fs::MetadataExt;
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            return PathOutcome::NotPresent {
                path: path.to_path_buf(),
            };
        }
    };
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        // Direct files OR symlinks are acceptable — distros ship
        // libibverbs.so.1 as a symlink to libibverbs.so.1.x.y.
        return PathOutcome::RejectedUnsafe {
            path: path.to_path_buf(),
            reason: "not a regular file or symlink".into(),
        };
    }
    if metadata.st_uid() != 0 {
        return PathOutcome::RejectedUnsafe {
            path: path.to_path_buf(),
            reason: format!("owner uid {} != 0 (root)", metadata.st_uid()),
        };
    }
    let mode = metadata.st_mode();
    // Group-write bit: 0o020. Other-write bit: 0o002.
    if mode & 0o022 != 0 {
        return PathOutcome::RejectedUnsafe {
            path: path.to_path_buf(),
            reason: format!("group/world-writable (mode {:o})", mode & 0o7777),
        };
    }
    PathOutcome::Found {
        path: path.to_path_buf(),
    }
}

/// Non-Linux fallback — RDMA bindings are Linux-only per ADR-042
/// §2.3. The probe self-disqualifies on other OSes BEFORE invoking
/// path validation; this just keeps the build green.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn validate_candidate(path: &Path) -> PathOutcome {
    PathOutcome::NotPresent {
        path: path.to_path_buf(),
    }
}

/// Resolve the first usable path from a search list, optionally
/// honoring an env-var override. Returns `Ok(path)` on success or
/// `Err(reason)` aggregating the rejection causes for diagnostics.
///
/// `env_override`: env-var name like `KISEKI_NATIVE_IBVERBS_LIB`.
/// When set, that path is checked FIRST; if validation fails the
/// caller sees the operator-config error directly (no silent
/// fallback to defaults).
///
/// # Errors
/// Returns the rejection diagnostics string if no path is usable.
#[allow(clippy::missing_errors_doc)]
pub fn resolve_system_library(leaf: &str, env_override: &str) -> Result<PathBuf, String> {
    let mut rejections: Vec<String> = Vec::new();

    if let Ok(operator_path) = std::env::var(env_override) {
        let path = PathBuf::from(&operator_path);
        match validate_candidate(&path) {
            PathOutcome::Found { path } => return Ok(path),
            PathOutcome::NotPresent { path } => {
                return Err(format!(
                    "{env_override}={path:?} not present (operator override didn't resolve)",
                ));
            }
            PathOutcome::RejectedUnsafe { path, reason } => {
                return Err(format!("{env_override}={path:?} rejected: {reason}",));
            }
        }
    }

    for candidate in system_library_search_paths(leaf) {
        match validate_candidate(&candidate) {
            PathOutcome::Found { path } => return Ok(path),
            PathOutcome::NotPresent { .. } => continue,
            PathOutcome::RejectedUnsafe { path, reason } => {
                rejections.push(format!("{path:?} rejected: {reason}"));
            }
        }
    }
    if rejections.is_empty() {
        Err(format!("{leaf} not present in default paths"))
    } else {
        Err(format!(
            "{leaf} not present + rejected candidates: [{}]",
            rejections.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_paths_have_three_distro_variants() {
        let paths = system_library_search_paths("libfoo.so.1");
        assert_eq!(paths.len(), 3, "Debian + RHEL + Alpine");
        // Debian first.
        assert!(
            paths[0].to_string_lossy().contains("linux-gnu"),
            "first path should be the Debian/${{arch}}-linux-gnu shape: {paths:?}",
        );
        // RHEL second.
        assert_eq!(paths[1], PathBuf::from("/usr/lib64/libfoo.so.1"));
        // Alpine third.
        assert_eq!(paths[2], PathBuf::from("/usr/lib/libfoo.so.1"));
    }

    #[test]
    fn search_paths_substitute_arch_token() {
        let paths = system_library_search_paths("libibverbs.so.1");
        let first = paths[0].to_string_lossy().to_string();
        // The arch slot is filled from cfg!(target_arch). On any
        // tier-1/tier-2 Rust Linux target we should see x86_64,
        // aarch64, or powerpc64le; on others "unknown".
        assert!(
            first.contains("x86_64")
                || first.contains("aarch64")
                || first.contains("powerpc64le")
                || first.contains("unknown"),
            "first path should carry the arch token: {first}",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validate_candidate_returns_not_present_for_missing_path() {
        let outcome = validate_candidate(Path::new("/this/path/should/not/exist/libfake.so.1"));
        assert!(matches!(outcome, PathOutcome::NotPresent { .. }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validate_candidate_rejects_non_root_owned_file() {
        // The test process likely runs as a non-root user; create a
        // tempfile owned by that user and verify it gets rejected.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let outcome = validate_candidate(tmp.path());
        match outcome {
            PathOutcome::RejectedUnsafe { reason, .. } => {
                assert!(
                    reason.contains("owner uid") || reason.contains("not a regular file"),
                    "reason: {reason}",
                );
            }
            // If the test runner happens to be root (CI sometimes
            // is) the file passes ownership but may still be
            // group/world-writable depending on umask. Either
            // RejectedUnsafe path is acceptable.
            other => panic!("expected RejectedUnsafe (non-root owner), got: {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_returns_diagnostic_when_no_path_usable() {
        // Ask for a leaf name no system library uses. Every search-
        // list candidate will be NotPresent; the resolver returns
        // the no-default-paths reason.
        let err = resolve_system_library(
            "libkiseki-fake-probe-not-on-disk.so.1",
            "KISEKI_NATIVE_FAKE_LIB",
        )
        .expect_err("must fail");
        assert!(
            err.contains("not present"),
            "should mention not-present: {err}",
        );
    }
}
