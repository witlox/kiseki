//! Device characteristic probing — auto-detects device properties from sysfs.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Detected storage medium type.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DetectedMedium {
    /// `NVMe` SSD (non-rotational, `NVMe` transport).
    NvmeSsd,
    /// SATA/SAS SSD (non-rotational, not `NVMe`).
    SataSsd,
    /// Hard disk drive (rotational).
    Hdd,
    /// Virtual/emulated device (virtio, no SMART).
    Virtual,
    /// Unable to determine.
    Unknown,
}

/// I/O strategy derived from device characteristics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IoStrategy {
    /// `O_DIRECT` | `O_DSYNC` -- `NVMe`, SATA SSD.
    DirectAligned,
    /// Buffered I/O with fsync — HDD (readahead benefits).
    BufferedSequential,
    /// Regular file I/O — VM, dev, CI. Enforces 4K alignment.
    FileBacked,
}

/// Auto-detected device properties.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceCharacteristics {
    /// Detected medium type.
    pub medium: DetectedMedium,
    /// Physical block size in bytes (alignment unit).
    pub physical_block_size: u32,
    /// Device-preferred I/O size in bytes.
    pub optimal_io_size: u32,
    /// True if the device is rotational (HDD).
    pub rotational: bool,
    /// NUMA node affinity (None if unknown).
    pub numa_node: Option<u32>,
    /// Whether the device supports TRIM/UNMAP.
    pub supports_trim: bool,
    /// Whether SMART health monitoring is available.
    pub supports_smart: bool,
    /// Derived I/O strategy.
    pub io_strategy: IoStrategy,
}

impl DeviceCharacteristics {
    /// Probe a block device via sysfs.
    ///
    /// Reads `/sys/block/<dev>/queue/*` and `/sys/block/<dev>/device/*`
    /// to determine device properties. Falls back to sensible defaults
    /// if sysfs is not available (non-Linux, VM).
    #[must_use]
    pub fn probe(path: &Path) -> Self {
        // Try to extract the block device name from the path.
        let dev_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let sysfs_base = format!("/sys/block/{dev_name}");
        let sysfs_path = Path::new(&sysfs_base);

        if sysfs_path.exists() {
            Self::probe_sysfs(dev_name, sysfs_path)
        } else {
            Self::probe_fallback(path)
        }
    }

    fn probe_sysfs(dev_name: &str, sysfs: &Path) -> Self {
        let rotational = read_sysfs_u32(&sysfs.join("queue/rotational")).unwrap_or(0) == 1;
        let physical_block_size =
            read_sysfs_u32(&sysfs.join("queue/physical_block_size")).unwrap_or(4096);
        let optimal_io_size = read_sysfs_u32(&sysfs.join("queue/optimal_io_size")).unwrap_or(0);
        let discard_max = read_sysfs_u64(&sysfs.join("queue/discard_max_bytes")).unwrap_or(0);
        let numa_node = read_sysfs_i32(&sysfs.join("device/numa_node")).and_then(|n| {
            if n >= 0 {
                Some(n.unsigned_abs())
            } else {
                None
            }
        });
        let model = read_sysfs_string(&sysfs.join("device/model")).unwrap_or_default();

        let is_nvme = dev_name.starts_with("nvme");
        let is_virtio =
            model.contains("virtio") || model.contains("VBOX") || model.contains("QEMU");

        let medium = if is_virtio {
            DetectedMedium::Virtual
        } else if rotational {
            DetectedMedium::Hdd
        } else if is_nvme {
            DetectedMedium::NvmeSsd
        } else {
            DetectedMedium::SataSsd
        };

        let io_strategy = match medium {
            DetectedMedium::Hdd => IoStrategy::BufferedSequential,
            DetectedMedium::NvmeSsd | DetectedMedium::SataSsd => IoStrategy::DirectAligned,
            DetectedMedium::Virtual | DetectedMedium::Unknown => IoStrategy::FileBacked,
        };

        Self {
            medium,
            physical_block_size,
            optimal_io_size: if optimal_io_size > 0 {
                optimal_io_size
            } else {
                physical_block_size
            },
            rotational,
            numa_node,
            supports_trim: discard_max > 0,
            supports_smart: !is_virtio && !rotational, // SMART on SSDs, not VMs
            io_strategy,
        }
    }

    fn probe_fallback(path: &Path) -> Self {
        // Non-block-device path (regular file, or non-Linux).
        // Use file-backed defaults with 4K simulated alignment.
        let is_block_device = path
            .metadata()
            .is_ok_and(|m| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    m.file_type().is_block_device()
                }
                #[cfg(not(unix))]
                {
                    let _ = m;
                    false
                }
            });

        if is_block_device {
            // Block device but no sysfs — assume SSD with conservative defaults.
            Self {
                medium: DetectedMedium::Unknown,
                physical_block_size: 4096,
                optimal_io_size: 4096,
                rotational: false,
                numa_node: None,
                supports_trim: false,
                supports_smart: false,
                io_strategy: IoStrategy::DirectAligned,
            }
        } else {
            Self::file_backed_defaults()
        }
    }

    /// Default characteristics for file-backed devices.
    #[must_use]
    pub fn file_backed_defaults() -> Self {
        Self {
            medium: DetectedMedium::Virtual,
            physical_block_size: 4096, // Enforce 4K alignment even for files
            optimal_io_size: 4096,
            rotational: false,
            numa_node: None,
            supports_trim: false,
            supports_smart: false,
            io_strategy: IoStrategy::FileBacked,
        }
    }
}

fn read_sysfs_u32(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_sysfs_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_sysfs_i32(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_sysfs_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backed_defaults_are_4k_aligned() {
        let chars = DeviceCharacteristics::file_backed_defaults();
        assert_eq!(chars.physical_block_size, 4096);
        assert_eq!(chars.io_strategy, IoStrategy::FileBacked);
        assert_eq!(chars.medium, DetectedMedium::Virtual);
    }

    #[test]
    fn probe_nonexistent_path_returns_file_backed() {
        let chars = DeviceCharacteristics::probe(Path::new("/tmp/nonexistent-device"));
        assert_eq!(chars.io_strategy, IoStrategy::FileBacked);
    }
}
