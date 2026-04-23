//! Pool rebalance worker.
//!
//! Computes migration plans when devices in a pool exceed a capacity
//! threshold, and provides progress tracking with cancellation support.
//!
//! Spec: ADR-024, I-D1.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kiseki_common::ids::ChunkId;

/// Configuration for a rebalance operation.
#[derive(Clone, Debug)]
pub struct RebalanceConfig {
    /// Maximum bytes per second for data migration (default: 100 MB/s).
    pub rate_limit_bytes_per_sec: u64,
}

impl Default for RebalanceConfig {
    fn default() -> Self {
        Self {
            rate_limit_bytes_per_sec: 100 * 1024 * 1024, // 100 MB/s
        }
    }
}

/// Tracks rebalance progress with atomic counters.
pub struct RebalanceProgress {
    /// Number of chunks moved so far.
    pub chunks_moved: AtomicU64,
    /// Bytes moved so far.
    pub bytes_moved: AtomicU64,
    /// Total chunks to move.
    pub chunks_total: u64,
    /// Cancellation flag.
    pub cancelled: AtomicBool,
}

impl RebalanceProgress {
    /// Create a new progress tracker.
    #[must_use]
    pub fn new(total: u64) -> Self {
        Self {
            chunks_moved: AtomicU64::new(0),
            bytes_moved: AtomicU64::new(0),
            chunks_total: total,
            cancelled: AtomicBool::new(false),
        }
    }

    /// Signal cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Check if cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Record progress for one chunk.
    pub fn record(&self, bytes: u64) {
        self.chunks_moved.fetch_add(1, Ordering::Relaxed);
        self.bytes_moved.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// A migration plan for one source-target device pair.
#[derive(Clone, Debug)]
pub struct RebalancePlan {
    /// Device to move chunks from.
    pub source_device: [u8; 16],
    /// Device to move chunks to.
    pub target_device: [u8; 16],
    /// Chunks to migrate.
    pub chunks_to_move: Vec<ChunkId>,
    /// Estimated total bytes to transfer.
    pub estimated_bytes: u64,
}

/// Device usage snapshot for rebalance planning.
#[derive(Clone, Debug)]
pub struct DeviceUsage {
    /// Device UUID.
    pub device_id: [u8; 16],
    /// Current used bytes.
    pub used_bytes: u64,
    /// Total capacity in bytes.
    pub capacity_bytes: u64,
    /// Chunks currently stored on this device.
    pub chunks: Vec<ChunkId>,
}

impl DeviceUsage {
    /// Usage as a fraction (0.0 - 1.0).
    #[must_use]
    pub fn usage_ratio(&self) -> f64 {
        if self.capacity_bytes == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.used_bytes as f64 / self.capacity_bytes as f64
        }
    }
}

/// Compute rebalance plans for a pool.
///
/// Identifies devices above `capacity_threshold` (0.0-1.0) and generates
/// migration plans to move chunks to under-utilized devices.
///
/// Returns an empty vec if all devices are within threshold.
#[must_use]
pub fn compute_plan(pool_devices: &[DeviceUsage], capacity_threshold: f64) -> Vec<RebalancePlan> {
    if pool_devices.is_empty() {
        return Vec::new();
    }

    // Find over-capacity and under-capacity devices.
    let mut over: Vec<&DeviceUsage> = pool_devices
        .iter()
        .filter(|d| d.usage_ratio() > capacity_threshold)
        .collect();
    let mut under: Vec<&DeviceUsage> = pool_devices
        .iter()
        .filter(|d| d.usage_ratio() <= capacity_threshold)
        .collect();

    if over.is_empty() || under.is_empty() {
        return Vec::new();
    }

    // Sort: most over-capacity first, most under-capacity first.
    over.sort_by(|a, b| {
        b.usage_ratio()
            .partial_cmp(&a.usage_ratio())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    under.sort_by(|a, b| {
        a.usage_ratio()
            .partial_cmp(&b.usage_ratio())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut plans = Vec::new();
    let mut under_idx = 0;

    for src in &over {
        if under_idx >= under.len() {
            break;
        }

        // Compute how many bytes to shed to reach threshold.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let target_used = (src.capacity_bytes as f64 * capacity_threshold) as u64;
        let excess = src.used_bytes.saturating_sub(target_used);

        if excess == 0 || src.chunks.is_empty() {
            continue;
        }

        // Estimate avg chunk size to decide how many chunks to move.
        #[allow(clippy::cast_precision_loss)]
        let avg_chunk_size = if src.chunks.is_empty() {
            0
        } else {
            src.used_bytes / src.chunks.len() as u64
        };

        if avg_chunk_size == 0 {
            continue;
        }

        #[allow(clippy::cast_possible_truncation)]
        let chunks_needed = excess.div_ceil(avg_chunk_size) as usize;
        let chunks_to_move: Vec<ChunkId> = src.chunks.iter().take(chunks_needed).copied().collect();
        let estimated_bytes = chunks_to_move.len() as u64 * avg_chunk_size;

        plans.push(RebalancePlan {
            source_device: src.device_id,
            target_device: under[under_idx].device_id,
            chunks_to_move,
            estimated_bytes,
        });

        under_idx += 1;
    }

    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pool_empty_plan() {
        let plans = compute_plan(&[], 0.8);
        assert!(plans.is_empty());
    }

    #[test]
    fn balanced_pool_empty_plan() {
        let devices = vec![
            DeviceUsage {
                device_id: [1; 16],
                used_bytes: 50,
                capacity_bytes: 100,
                chunks: vec![ChunkId([0xaa; 32])],
            },
            DeviceUsage {
                device_id: [2; 16],
                used_bytes: 60,
                capacity_bytes: 100,
                chunks: vec![ChunkId([0xbb; 32])],
            },
        ];

        let plans = compute_plan(&devices, 0.8);
        assert!(plans.is_empty());
    }

    #[test]
    fn over_capacity_generates_migration_plan() {
        let devices = vec![
            DeviceUsage {
                device_id: [1; 16],
                used_bytes: 95,
                capacity_bytes: 100,
                chunks: vec![
                    ChunkId([0x01; 32]),
                    ChunkId([0x02; 32]),
                    ChunkId([0x03; 32]),
                    ChunkId([0x04; 32]),
                    ChunkId([0x05; 32]),
                ],
            },
            DeviceUsage {
                device_id: [2; 16],
                used_bytes: 30,
                capacity_bytes: 100,
                chunks: vec![ChunkId([0xaa; 32])],
            },
        ];

        let plans = compute_plan(&devices, 0.8);
        assert!(!plans.is_empty());
        assert_eq!(plans[0].source_device, [1; 16]);
        assert_eq!(plans[0].target_device, [2; 16]);
        assert!(!plans[0].chunks_to_move.is_empty());
        assert!(plans[0].estimated_bytes > 0);
    }

    #[test]
    fn cancel_flag_works() {
        let progress = RebalanceProgress::new(100);
        assert!(!progress.is_cancelled());

        progress.cancel();
        assert!(progress.is_cancelled());
    }

    #[test]
    fn progress_tracking() {
        let progress = RebalanceProgress::new(10);
        assert_eq!(progress.chunks_moved.load(Ordering::Relaxed), 0);
        assert_eq!(progress.bytes_moved.load(Ordering::Relaxed), 0);

        progress.record(4096);
        progress.record(4096);

        assert_eq!(progress.chunks_moved.load(Ordering::Relaxed), 2);
        assert_eq!(progress.bytes_moved.load(Ordering::Relaxed), 8192);
        assert_eq!(progress.chunks_total, 10);
    }
}
