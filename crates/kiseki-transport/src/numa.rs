//! NUMA topology detection and thread affinity pinning.
//!
//! Detects NUMA topology from sysfs (Linux) and pins threads to the
//! NUMA node of the associated NIC or `NVMe` controller. No-op on
//! non-Linux platforms.

use std::io;
use std::path::Path;

/// A NUMA node with its CPUs and associated devices.
#[derive(Clone, Debug)]
pub struct NumaNode {
    /// NUMA node ID.
    pub id: u32,
    /// Online CPU IDs on this node.
    pub cpus: Vec<u32>,
    /// PCI devices associated with this node (device names).
    pub devices: Vec<String>,
    /// Total memory on this node in MB (0 if unknown).
    pub memory_mb: u64,
}

/// System NUMA topology.
#[derive(Clone, Debug)]
pub struct NumaTopology {
    /// NUMA nodes detected on the system.
    pub nodes: Vec<NumaNode>,
}

impl NumaTopology {
    /// Detect NUMA topology from sysfs.
    ///
    /// On non-Linux platforms, returns a single-node topology with no
    /// CPU or device information.
    #[must_use]
    pub fn detect() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::detect_linux()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                nodes: vec![NumaNode {
                    id: 0,
                    cpus: Vec::new(),
                    devices: Vec::new(),
                    memory_mb: 0,
                }],
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn detect_linux() -> Self {
        let base = Path::new("/sys/devices/system/node");
        if !base.exists() {
            return Self {
                nodes: vec![NumaNode {
                    id: 0,
                    cpus: Vec::new(),
                    devices: Vec::new(),
                    memory_mb: 0,
                }],
            };
        }

        let mut nodes = Vec::new();
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(id_str) = name.strip_prefix("node") {
                    if let Ok(id) = id_str.parse::<u32>() {
                        let node_path = base.join(&name);
                        let cpus = parse_cpu_list(&node_path.join("cpulist"));
                        let memory_mb = parse_meminfo_mb(&node_path.join("meminfo"));
                        nodes.push(NumaNode {
                            id,
                            cpus,
                            devices: Vec::new(),
                            memory_mb,
                        });
                    }
                }
            }
        }

        if nodes.is_empty() {
            nodes.push(NumaNode {
                id: 0,
                cpus: Vec::new(),
                devices: Vec::new(),
                memory_mb: 0,
            });
        }

        nodes.sort_by_key(|n| n.id);
        Self { nodes }
    }

    /// Find which NUMA node a device belongs to.
    ///
    /// Reads `/sys/class/<class>/<device>/device/numa_node`.
    #[must_use]
    pub fn device_numa_node(class: &str, device: &str) -> Option<u32> {
        let path = Path::new("/sys/class")
            .join(class)
            .join(device)
            .join("device")
            .join("numa_node");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .and_then(|n| u32::try_from(n).ok())
    }

    /// Get CPUs for a given NUMA node ID.
    #[must_use]
    pub fn cpus_for_node(&self, node_id: u32) -> &[u32] {
        self.nodes
            .iter()
            .find(|n| n.id == node_id)
            .map_or(&[], |n| &n.cpus)
    }

    /// Number of NUMA nodes detected.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

/// Parse a CPU list string like "0-3,8-11" into individual CPU IDs.
#[cfg(any(target_os = "linux", test))]
fn parse_cpu_list(path: &Path) -> Vec<u32> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut cpus = Vec::new();
    for part in content.trim().split(',') {
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(s), Ok(e)) = (start.parse::<u32>(), end.parse::<u32>()) {
                cpus.extend(s..=e);
            }
        } else if let Ok(cpu) = part.parse::<u32>() {
            cpus.push(cpu);
        }
    }
    cpus
}

/// Parse `meminfo` file to get total memory in MB for a NUMA node.
#[cfg(target_os = "linux")]
fn parse_meminfo_mb(path: &Path) -> u64 {
    let Ok(content) = std::fs::read_to_string(path) else {
        return 0;
    };
    for line in content.lines() {
        if line.contains("MemTotal") {
            // Format: "Node 0 MemTotal:    123456 kB"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                if let Ok(kb) = parts[3].parse::<u64>() {
                    return kb / 1024;
                }
            }
        }
    }
    0
}

/// Pin the current thread to the given set of CPUs.
///
/// On Linux, uses `sched_setaffinity()`. On other platforms, this is a
/// no-op that logs a warning.
///
/// # Errors
///
/// Returns an error if `sched_setaffinity()` fails on Linux.
pub fn pin_current_thread(cpus: &[u32]) -> io::Result<()> {
    if cpus.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        pin_linux(cpus)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cpus;
        tracing::warn!("NUMA thread pinning not supported on this platform");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn pin_linux(cpus: &[u32]) -> io::Result<()> {
    // SAFETY: cpu_set is a stack-allocated, zeroed libc struct.
    // sched_setaffinity is called with pid=0 (current thread)
    // and a valid cpu_set pointer.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &cpu in cpus {
            libc::CPU_SET(cpu as usize, &mut set);
        }
        let ret = libc::sched_setaffinity(
            0, // current thread
            std::mem::size_of::<libc::cpu_set_t>(),
            &set,
        );
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpu_list_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cpulist");
        std::fs::write(&path, "0-3,8-11\n").unwrap();
        let cpus = parse_cpu_list(&path);
        assert_eq!(cpus, vec![0, 1, 2, 3, 8, 9, 10, 11]);
    }

    #[test]
    fn parse_cpu_list_single() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cpulist");
        std::fs::write(&path, "0,2,4\n").unwrap();
        let cpus = parse_cpu_list(&path);
        assert_eq!(cpus, vec![0, 2, 4]);
    }

    #[test]
    fn parse_cpu_list_missing_file() {
        let cpus = parse_cpu_list(Path::new("/nonexistent/cpulist"));
        assert!(cpus.is_empty());
    }

    #[test]
    fn detect_returns_at_least_one_node() {
        let topo = NumaTopology::detect();
        assert!(topo.node_count() >= 1);
    }

    #[test]
    fn pin_empty_cpus_is_noop() {
        assert!(pin_current_thread(&[]).is_ok());
    }

    #[test]
    fn pin_current_thread_does_not_panic() {
        // On non-Linux: no-op. On Linux: may succeed or fail depending
        // on available CPUs, but should not panic.
        let _ = pin_current_thread(&[0]);
    }

    #[test]
    fn device_numa_node_nonexistent() {
        assert!(NumaTopology::device_numa_node("nonexistent", "dev0").is_none());
    }
}
