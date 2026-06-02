//! System disk detection and metadata capacity budget (ADR-030).
//!
//! Detects media type (`NVMe`, SSD, HDD) of the system partition via
//! sysfs on Linux. Falls back to `Unknown` on other platforms.
//! Computes metadata budget from total capacity and configured limits.
//!
//! ADR-030 2026-05-31 amendment §"admin-driven metadata device role":
//! the result is pushed into the
//! `kiseki_node_metadata_capacity_bytes{kind=...}` gauge so the cluster
//! aggregator can derive `cluster_max_files` from
//! `Σ soft_limit / per-file metadata footprint`. See
//! `docs/performance/capacity-planning.md`.

use std::path::Path;

// ADR-049 phase 1: MediaType + PER_FILE_METADATA_FOOTPRINT_BYTES
// relocated to kiseki-common so every dependent crate (the control-
// plane state machine, admin RPC, resolver, fjall consumers) can
// import them. Re-exported here for back-compat with existing
// in-server callers (`runtime.rs:529`, `web/admin_extra.rs:1635`,
// the storage_admin_cli pool view, BDD test fixtures).
pub use kiseki_common::{MediaType, PER_FILE_METADATA_FOOTPRINT_BYTES};

/// Metadata capacity budget for the system disk.
#[derive(Clone, Debug)]
pub struct NodeMetadataCapacity {
    /// Total capacity of the system partition (bytes).
    pub total_bytes: u64,
    /// Current usage (bytes).
    pub used_bytes: u64,
    /// Soft limit for metadata usage (bytes).
    pub soft_limit_bytes: u64,
    /// Hard limit for metadata usage (bytes).
    pub hard_limit_bytes: u64,
    /// Detected media type.
    pub media_type: MediaType,
    /// Budget available for small-file inline content (bytes).
    pub small_file_budget_bytes: u64,
}

/// Detect the media type of the filesystem containing `path`.
#[must_use]
pub fn detect_media_type(path: &Path) -> MediaType {
    #[cfg(target_os = "linux")]
    {
        detect_media_type_linux(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        MediaType::Unknown
    }
}

#[cfg(target_os = "linux")]
fn detect_media_type_linux(path: &Path) -> MediaType {
    use std::fs;

    // Try to find the block device from /proc/mounts.
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mount_point = path.to_str().unwrap_or("");

    // Find the longest matching mount point.
    let mut best_dev = None;
    let mut best_len = 0;
    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && mount_point.starts_with(parts[1]) && parts[1].len() > best_len {
            best_dev = Some(parts[0].to_owned());
            best_len = parts[1].len();
        }
    }

    let Some(dev) = best_dev else {
        return MediaType::Unknown;
    };

    // Extract device name (e.g., /dev/nvme0n1p1 → nvme0n1).
    let dev_name = dev
        .strip_prefix("/dev/")
        .unwrap_or(&dev)
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('p');

    // Check rotational flag.
    let rotational_path = format!("/sys/block/{dev_name}/queue/rotational");
    if let Ok(val) = fs::read_to_string(&rotational_path) {
        if val.trim() == "1" {
            return MediaType::Hdd;
        }
        if dev_name.starts_with("nvme") {
            return MediaType::Nvme;
        }
        return MediaType::Ssd;
    }

    MediaType::Unknown
}

/// Compute metadata capacity budget for the system disk.
#[must_use]
pub fn compute_capacity(
    data_dir: &Path,
    soft_limit_pct: u8,
    hard_limit_pct: u8,
) -> NodeMetadataCapacity {
    let (total, available) = fs_stats(data_dir);
    let used = total.saturating_sub(available);
    let media_type = detect_media_type(data_dir);

    let soft_limit_bytes = total * u64::from(soft_limit_pct) / 100;
    let hard_limit_bytes = total * u64::from(hard_limit_pct) / 100;
    let small_file_budget_bytes = soft_limit_bytes.saturating_sub(used);

    NodeMetadataCapacity {
        total_bytes: total,
        used_bytes: used,
        soft_limit_bytes,
        hard_limit_bytes,
        media_type,
        small_file_budget_bytes,
    }
}

/// Get total and available bytes for the filesystem containing `path`.
///
/// Uses `std::process::Command` to call `df` as a safe alternative
/// to libc `statvfs` (avoids unsafe in the server binary).
fn fs_stats(path: &Path) -> (u64, u64) {
    // Use `df -k <path>` for portable filesystem stats.
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        // Parse the second line: Filesystem 1K-blocks Used Available ...
        if let Some(line) = text.lines().nth(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let total_kb: u64 = parts[1].parse().unwrap_or(0);
                let available_kb: u64 = parts[3].parse().unwrap_or(0);
                return (total_kb * 1024, available_kb * 1024);
            }
        }
    }
    (0, 0)
}

/// Emit a warning if the system disk is rotational (HDD).
pub fn warn_if_rotational(media_type: MediaType) {
    if media_type == MediaType::Hdd {
        tracing::warn!(
            "system disk is rotational (HDD). Raft fsync latency will be 5-10ms per commit. \
             Production deployments require NVMe or SSD for the metadata partition. See ADR-030."
        );
    }
}

/// ADR-030 2026-05-31 amendment — push the
/// `NodeMetadataCapacity` snapshot into the Prometheus gauges so the
/// cluster aggregator can derive `cluster_max_files` and surface
/// per-node media class. Idempotent and cheap; safe to call on a
/// periodic tick.
pub fn emit_capacity_metrics(metrics: &crate::metrics::KisekiMetrics, cap: &NodeMetadataCapacity) {
    // Per-kind capacity gauges. Each label resets cleanly on
    // re-application, so periodic refresh is correct.
    let kinds: [(&str, u64); 5] = [
        ("total", cap.total_bytes),
        ("used", cap.used_bytes),
        ("soft_limit", cap.soft_limit_bytes),
        ("hard_limit", cap.hard_limit_bytes),
        ("small_file_budget", cap.small_file_budget_bytes),
    ];
    for (kind, value) in kinds {
        // Cast saturates at i64::MAX — exabyte+ scale clamps but never panics.
        let v = i64::try_from(value).unwrap_or(i64::MAX);
        metrics
            .node_metadata_capacity_bytes
            .with_label_values(&[kind])
            .set(v);
    }

    // Media-type info gauge: one-hot. Zero out the others on every
    // refresh in case the active media class flipped (e.g. operator
    // remounted onto a different device).
    for &kind in &["nvme", "ssd", "hdd", "unknown"] {
        let v = i64::from(cap.media_type.as_label() == kind);
        metrics
            .node_metadata_media_type
            .with_label_values(&[kind])
            .set(v);
    }
}

// (PER_FILE_METADATA_FOOTPRINT_BYTES moved to kiseki-common::metadata
// per ADR-049 phase 1. Re-exported at the top of this file via the
// `pub use kiseki_common::*` line so existing callers continue to
// compile against `crate::system_disk::PER_FILE_METADATA_FOOTPRINT_BYTES`.)
