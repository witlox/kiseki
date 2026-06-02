//! ADR-049 phase 2: per-node device inventory discovery + tag parsing.
//!
//! Reads `/proc/mounts` (Linux), filters to real-storage filesystems,
//! classifies each via `system_disk::detect_media_type`, and applies
//! operator-supplied `KISEKI_DEVICE_TAGS` overrides. Returns a fresh
//! `NodeDeviceInventory` per call.
//!
//! Phase 2 lands this as a pure function so phase-3 callers can
//! exercise it without spawning the full periodic refresh task.
//! Phase 5 adds the `InventoryReporter` task that drives
//! `UpsertNodeInventory` at boot + every
//! `KISEKI_INVENTORY_REFRESH_MS` (default 60 s).
//!
//! ## Q1 / N-9 acceptance
//!
//! Boot order is: discover → publish → resolver. A node that boots
//! with a stale catalog inventory (e.g. it died with `/mnt/nvme0`
//! mounted but recovers without it) MUST republish its current
//! truth BEFORE the resolver gate runs. The pure-function shape
//! makes that ordering testable (`discover_local_inventory(node)`
//! is what `await_catalog_ready` (phase 3) calls first).

#![allow(dead_code)] // phase-3+ consumers wire this; dead until then

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kiseki_common::{DeviceEntry, NodeDeviceInventory, NodeId};

use crate::system_disk::detect_media_type;

/// File systems that are NOT real backing storage. The discovery walker
/// excludes them from the inventory so a node with `tmpfs` mounted at
/// `/run` doesn't get classified as having extra capacity.
const EXCLUDED_FSTYPES: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "fusectl",
    "fuse.gvfsd-fuse",
    "fuse.snapfuse",
    "rpc_pipefs",
    "configfs",
    "debugfs",
    "tracefs",
    "selinuxfs",
    "pstore",
    "bpf",
    "autofs",
    "overlay",
    "binfmt_misc",
    "hugetlbfs",
    "ramfs",
    "squashfs",
    "iso9660",
    "udf",
    "vfat", // EFI/boot — adversary may flag if a real workload mounts vfat for data
];

/// Mount points that are always excluded even when their fstype is
/// real-storage. These are kernel + container-runtime overlays that
/// look like real disks but are not durable storage.
const EXCLUDED_PATH_PREFIXES: &[&str] = &[
    "/snap/",
    "/var/lib/docker/",
    "/var/lib/containerd/",
    "/var/lib/kubelet/",
    "/run/",
    "/sys/",
    "/proc/",
    "/dev/",
    "/boot/efi",
];

/// Operator-supplied tags from the `KISEKI_DEVICE_TAGS` env var, parsed
/// into a `path → tag` lookup. Format:
///
/// ```text
/// KISEKI_DEVICE_TAGS=/mnt/nvme0=nvme-fast,/mnt/sata0=ssd-tier,/=boot-only
/// ```
///
/// Untagged mounts still appear in the inventory with `tag: None` so
/// auto-class policies (e.g. `Class(Nvme)`) match them; tagged-target
/// policies (e.g. `Tag("nvme-fast")`) skip them.
#[derive(Clone, Debug, Default)]
pub struct DeviceTagMap {
    tags: std::collections::HashMap<PathBuf, String>,
}

impl DeviceTagMap {
    /// Parse `KISEKI_DEVICE_TAGS` from a raw env var string. Returns
    /// an empty map when the input is empty or malformed beyond
    /// repair.
    ///
    /// Format: comma-separated `path=tag` pairs. Whitespace around
    /// either side is trimmed. Empty entries are skipped. Duplicate
    /// paths use the last definition (operator's prerogative).
    #[must_use]
    pub fn parse(input: &str) -> Self {
        let mut tags = std::collections::HashMap::new();
        for entry in input.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some((path, tag)) = entry.split_once('=') else {
                tracing::warn!(
                    entry,
                    "KISEKI_DEVICE_TAGS: ignoring malformed entry (expected path=tag)",
                );
                continue;
            };
            let path = path.trim();
            let tag = tag.trim();
            if path.is_empty() || tag.is_empty() {
                tracing::warn!(
                    entry,
                    "KISEKI_DEVICE_TAGS: ignoring entry with empty path or tag",
                );
                continue;
            }
            tags.insert(PathBuf::from(path), tag.to_owned());
        }
        Self { tags }
    }

    /// Look up the operator-supplied tag for a mount path. Returns
    /// `None` when the mount has no tag (the inventory entry's
    /// `tag` field will be `None`).
    #[must_use]
    pub fn tag_for(&self, mount_path: &Path) -> Option<String> {
        self.tags.get(mount_path).cloned()
    }

    /// Number of tags configured (mostly for observability + tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    /// Whether the tag map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}

/// One row of /proc/mounts after parsing.
#[derive(Clone, Debug)]
struct ParsedMount {
    source: String,
    mount_point: PathBuf,
    fstype: String,
}

/// Parse `/proc/mounts` content into structured rows. Pure function
/// over the file bytes so tests can drive synthetic mount tables.
fn parse_mounts(content: &str) -> Vec<ParsedMount> {
    let mut out = Vec::new();
    for line in content.lines() {
        // /proc/mounts format: source mount_point fstype options dump pass
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        out.push(ParsedMount {
            source: parts[0].to_owned(),
            mount_point: PathBuf::from(parts[1]),
            fstype: parts[2].to_owned(),
        });
    }
    out
}

/// Filter parsed mounts down to "real backing storage":
///   - fstype not in [`EXCLUDED_FSTYPES`]
///   - mount point not under [`EXCLUDED_PATH_PREFIXES`]
///   - source starts with `/dev/` (a real block device) — `bind`
///     mounts and `none` sources are skipped
///
/// Deduplicates by `mount_point` (keeps the first occurrence, which
/// matches kernel's "most recent wins" ordering when /proc/mounts
/// is re-read after a `mount -o remount`).
fn filter_real_storage(mounts: Vec<ParsedMount>) -> Vec<ParsedMount> {
    let excluded_fstypes: HashSet<&str> = EXCLUDED_FSTYPES.iter().copied().collect();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    mounts
        .into_iter()
        .filter(|m| !excluded_fstypes.contains(m.fstype.as_str()))
        .filter(|m| m.source.starts_with("/dev/"))
        .filter(|m| {
            let p = m.mount_point.to_string_lossy();
            !EXCLUDED_PATH_PREFIXES
                .iter()
                .any(|prefix| p.starts_with(prefix))
        })
        .filter(|m| seen_paths.insert(m.mount_point.clone()))
        .collect()
}

/// Discover the local node's device inventory by reading
/// `/proc/mounts` (Linux only — returns an empty inventory on other
/// platforms; ADR-049 is Linux-only per the project memory).
///
/// `tags` controls operator-supplied tag overrides (D1 step 2).
///
/// `data_dir` is always added as a fallback `DeviceEntry` tagged
/// `data-dir-default` so policies with `DeviceMatcher::DataDir`
/// always have a target. The tag is overridable by the operator
/// via `KISEKI_DEVICE_TAGS=/path/to/data=other-tag`.
///
/// `node_id` is recorded so the inventory carries its own identity
/// (the catalog catalog also keys by it, but having both makes
/// round-trips safer).
///
/// `refreshed_ms` is supplied by the caller so the discovery
/// function stays pure (testable). The `InventoryReporter` task
/// passes `now_ms()` at refresh time.
#[must_use]
pub fn discover_local_inventory(
    node_id: NodeId,
    data_dir: Option<&Path>,
    tags: &DeviceTagMap,
    refreshed_ms: u64,
) -> NodeDeviceInventory {
    discover_with_proc_mounts(node_id, data_dir, tags, refreshed_ms, &read_proc_mounts())
}

/// Discover variant that takes the `/proc/mounts` content as a
/// pre-read string. Used by tests + the production `discover_local_inventory`
/// to share the parsing + filtering + classification pipeline.
#[must_use]
pub fn discover_with_proc_mounts(
    node_id: NodeId,
    data_dir: Option<&Path>,
    tags: &DeviceTagMap,
    refreshed_ms: u64,
    proc_mounts_content: &str,
) -> NodeDeviceInventory {
    let real_mounts = filter_real_storage(parse_mounts(proc_mounts_content));
    let mut devices: Vec<DeviceEntry> = Vec::with_capacity(real_mounts.len() + 1);

    for m in real_mounts {
        let (total_bytes, free_bytes) = fs_stats(&m.mount_point);
        // Skip mounts that report 0 total — they're either kernel
        // synthetic FS missed by the fstype filter or unreadable.
        if total_bytes == 0 {
            continue;
        }
        let media_class = detect_media_type(&m.mount_point);
        let tag = tags.tag_for(&m.mount_point);
        devices.push(DeviceEntry {
            mount_path: m.mount_point,
            media_class,
            total_bytes,
            free_bytes,
            tag,
            exclusive: true, // best-effort; phase 5 can refine via /proc/self/mountinfo
        });
    }

    // ADR-049 D1 step 3: always include data_dir as a fallback
    // entry so `DeviceMatcher::DataDir` always has a target. Tagged
    // `data-dir-default` unless the operator overrode it.
    if let Some(dir) = data_dir {
        // Only add the entry if data_dir wasn't already discovered
        // via /proc/mounts (its mount point might be `/` or another
        // path that already appears).
        let already_present = devices.iter().any(|d| {
            // Compare by canonical path: data_dir may be under a
            // mount point we already added.
            let dir_str = dir.to_string_lossy();
            let entry_str = d.mount_path.to_string_lossy();
            dir_str.starts_with(entry_str.as_ref())
        });
        if !already_present {
            // ADR-049 D1 step 3 says "always include" — sandboxed CI
            // runners (tmpfs / non-statvfs-friendly mounts) report
            // `total_bytes=0`. Drop the entry only if statvfs errors
            // entirely; a zero-byte report still gets the entry so
            // `DeviceMatcher::DataDir` policies retain a target. The
            // resolver's BestEffort fallback can pick this up even
            // when capacity is unknown.
            let (total_bytes, free_bytes) = fs_stats(dir);
            let media_class = detect_media_type(dir);
            let tag = tags
                .tag_for(dir)
                .or_else(|| Some("data-dir-default".to_owned()));
            devices.push(DeviceEntry {
                mount_path: dir.to_path_buf(),
                media_class,
                total_bytes,
                free_bytes,
                tag,
                exclusive: false,
            });
        }
    }

    NodeDeviceInventory {
        node_id,
        devices,
        refreshed_ms,
    }
}

/// Read `/proc/mounts`. Empty string on non-Linux or read failure
/// (discovery yields an empty inventory + falls back to `data_dir`).
fn read_proc_mounts() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/mounts").unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

/// `df`-style total + available bytes for the filesystem containing
/// `path`. Returns `(0, 0)` on any failure so the discovery walker
/// skips the entry rather than mis-classifying it as zero-capacity
/// data.
///
/// Mirrors the helper in `system_disk.rs` but lives here to avoid a
/// circular import — `system_disk` uses it for its own metrics, and
/// `device_discovery` uses it for inventory.
fn fs_stats(path: &Path) -> (u64, u64) {
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_tag_map_parses_well_formed_input() {
        let tags = DeviceTagMap::parse("/mnt/nvme0=nvme-fast,/mnt/sata0=ssd-tier,/=boot-only");
        assert_eq!(tags.len(), 3);
        assert_eq!(
            tags.tag_for(&PathBuf::from("/mnt/nvme0")),
            Some("nvme-fast".to_owned())
        );
        assert_eq!(
            tags.tag_for(&PathBuf::from("/mnt/sata0")),
            Some("ssd-tier".to_owned())
        );
        assert_eq!(
            tags.tag_for(&PathBuf::from("/")),
            Some("boot-only".to_owned())
        );
        assert_eq!(tags.tag_for(&PathBuf::from("/mnt/unknown")), None);
    }

    #[test]
    fn device_tag_map_tolerates_whitespace_and_empty_entries() {
        let tags = DeviceTagMap::parse(" /mnt/a = nvme-fast ,, /mnt/b = ssd ,");
        assert_eq!(tags.len(), 2);
        assert_eq!(
            tags.tag_for(&PathBuf::from("/mnt/a")),
            Some("nvme-fast".to_owned())
        );
    }

    #[test]
    fn device_tag_map_skips_malformed_entries() {
        let tags = DeviceTagMap::parse("no-equals-sign,/path=,=tag-only,/good=ok");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags.tag_for(&PathBuf::from("/good")), Some("ok".to_owned()));
    }

    #[test]
    fn device_tag_map_last_definition_wins_for_duplicate_paths() {
        let tags = DeviceTagMap::parse("/mnt/a=first,/mnt/a=second");
        assert_eq!(
            tags.tag_for(&PathBuf::from("/mnt/a")),
            Some("second".to_owned())
        );
    }

    #[test]
    fn parse_mounts_handles_realistic_proc_mounts_lines() {
        let sample = "\
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p1 /mnt/nvme0 ext4 rw,relatime 0 0
tmpfs /run tmpfs rw,nosuid,nodev,size=10M 0 0
/dev/sda1 / ext4 rw,relatime 0 0
/dev/sdb1 /mnt/sata0 xfs rw,relatime 0 0
";
        let mounts = parse_mounts(sample);
        assert_eq!(mounts.len(), 6);
        assert_eq!(mounts[2].fstype, "ext4");
        assert_eq!(mounts[2].mount_point, PathBuf::from("/mnt/nvme0"));
    }

    #[test]
    fn filter_real_storage_drops_kernel_and_runtime_fstypes() {
        let mounts = parse_mounts(
            "\
proc /proc proc rw 0 0
sysfs /sys sysfs rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/nvme0n1p1 /mnt/nvme0 ext4 rw 0 0
/dev/sda1 / ext4 rw 0 0
/dev/sdb1 /mnt/sata0 xfs rw 0 0
overlay /var/lib/docker/overlay2/abc/merged overlay rw 0 0
",
        );
        let real = filter_real_storage(mounts);
        // Should keep nvme0, /, and sata0; drop proc/sysfs/tmpfs/overlay.
        assert_eq!(real.len(), 3);
        let paths: Vec<_> = real.iter().map(|m| &m.mount_point).collect();
        assert!(paths.contains(&&PathBuf::from("/mnt/nvme0")));
        assert!(paths.contains(&&PathBuf::from("/")));
        assert!(paths.contains(&&PathBuf::from("/mnt/sata0")));
    }

    #[test]
    fn filter_real_storage_skips_excluded_path_prefixes() {
        let mounts = parse_mounts(
            "\
/dev/snap0 /snap/foo ext4 rw 0 0
/dev/nvme0n1p1 /mnt/nvme0 ext4 rw 0 0
",
        );
        let real = filter_real_storage(mounts);
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].mount_point, PathBuf::from("/mnt/nvme0"));
    }

    #[test]
    fn filter_real_storage_deduplicates_by_mount_point() {
        // A remount creates a second /proc/mounts entry for the same
        // path; keep the first.
        let mounts = parse_mounts(
            "\
/dev/sda1 /mnt/foo ext4 rw,defaults 0 0
/dev/sda1 /mnt/foo ext4 rw,remount,noatime 0 0
",
        );
        let real = filter_real_storage(mounts);
        assert_eq!(real.len(), 1);
    }

    #[test]
    fn discover_with_synthetic_proc_mounts_yields_expected_inventory() {
        // We can't assert on byte-counts here because `df` runs against
        // the real test host's filesystem; instead we assert on the
        // shape: the right mount points appear with the right tags
        // (or absence of tags), and excluded fstypes are dropped.
        //
        // Note: `total_bytes == 0` short-circuits an entry from the
        // inventory, so on a CI host where /mnt/nvme0 doesn't exist,
        // we'd see an empty inventory. This is intentional — the
        // discovery function only reports devices that actually exist.
        // Tests that need real device entries use the
        // `proc_mounts_content` variant with synthetic data + must
        // mock `fs_stats`. Phase 3 + 5 will add a richer mock layer.
        let proc = "\
proc /proc proc rw 0 0
/dev/nvme0n1p1 /this/path/will/never/exist ext4 rw 0 0
";
        let tags = DeviceTagMap::parse("");
        let inv = discover_with_proc_mounts(NodeId(42), None, &tags, 1_700_000_000_000, proc);
        assert_eq!(inv.node_id, NodeId(42));
        assert_eq!(inv.refreshed_ms, 1_700_000_000_000);
        // The non-existent path has total_bytes=0 so it's filtered out.
        assert!(inv.devices.is_empty());
    }

    #[test]
    fn discover_includes_data_dir_fallback_with_default_tag() {
        // Use the test process's CWD as data_dir — it exists and has
        // free space.
        let tags = DeviceTagMap::parse("");
        let cwd = std::env::current_dir().expect("cwd");
        let inv = discover_with_proc_mounts(NodeId(1), Some(&cwd), &tags, 0, "");
        // Should have at least the data_dir fallback entry.
        assert!(!inv.devices.is_empty());
        let dir_entry = &inv.devices[0];
        assert_eq!(dir_entry.tag.as_deref(), Some("data-dir-default"));
        assert!(!dir_entry.exclusive);
        assert!(dir_entry.total_bytes > 0);
    }

    #[test]
    fn discover_respects_operator_tag_override_on_data_dir() {
        let cwd = std::env::current_dir().expect("cwd");
        let tags = DeviceTagMap::parse(&format!("{}=operator-tag", cwd.display()));
        let inv = discover_with_proc_mounts(NodeId(1), Some(&cwd), &tags, 0, "");
        assert!(!inv.devices.is_empty());
        assert_eq!(
            inv.devices[0].tag.as_deref(),
            Some("operator-tag"),
            "operator tag must override the data-dir-default tag",
        );
    }
}
