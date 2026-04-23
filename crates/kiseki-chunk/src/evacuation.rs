//! Device evacuation worker.
//!
//! Plans and tracks the evacuation of chunks from a device that is being
//! decommissioned or has been flagged unhealthy.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kiseki_common::ids::ChunkId;

/// Live progress tracker for an in-flight evacuation.
pub struct EvacuationProgress {
    /// Number of chunks evacuated so far.
    pub chunks_evacuated: AtomicU64,
    /// Bytes evacuated so far.
    pub bytes_evacuated: AtomicU64,
    /// Total number of chunks to evacuate.
    pub chunks_total: u64,
    /// Set to `true` to request cancellation.
    pub cancelled: AtomicBool,
    /// Device being evacuated.
    pub device_id: [u8; 16],
}

impl EvacuationProgress {
    /// Create a new progress tracker for `total` chunks on `device_id`.
    #[must_use]
    pub fn new(device_id: [u8; 16], total: u64) -> Self {
        Self {
            chunks_evacuated: AtomicU64::new(0),
            bytes_evacuated: AtomicU64::new(0),
            chunks_total: total,
            cancelled: AtomicBool::new(false),
            device_id,
        }
    }

    /// Request cancellation of the evacuation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns `true` when all chunks have been evacuated.
    pub fn is_complete(&self) -> bool {
        self.chunks_evacuated.load(Ordering::Acquire) >= self.chunks_total
    }
}

/// A plan describing which chunks to move and where.
#[derive(Debug, Clone)]
pub struct EvacuationPlan {
    /// Device being evacuated.
    pub device_id: [u8; 16],
    /// Chunks that must be moved off the device.
    pub chunks_to_evacuate: Vec<ChunkId>,
    /// Candidate target devices that can receive chunks.
    pub target_devices: Vec<[u8; 16]>,
    /// Estimated total bytes to transfer.
    pub estimated_bytes: u64,
}

/// Build an evacuation plan for `device_id`.
///
/// - `chunk_ids_on_device`: all chunks currently stored on the device.
/// - `available_targets`: set of device IDs that can accept migrated chunks.
/// - `estimated_bytes`: estimated total bytes to transfer.
#[must_use]
pub fn plan_evacuation(
    device_id: [u8; 16],
    chunk_ids_on_device: Vec<ChunkId>,
    available_targets: Vec<[u8; 16]>,
    estimated_bytes: u64,
) -> EvacuationPlan {
    EvacuationPlan {
        device_id,
        chunks_to_evacuate: chunk_ids_on_device,
        target_devices: available_targets,
        estimated_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_includes_all_chunks_on_device() {
        let dev = [0xAA; 16];
        let chunks = vec![ChunkId([1; 32]), ChunkId([2; 32]), ChunkId([3; 32])];
        let targets = vec![[0xBB; 16], [0xCC; 16]];

        let plan = plan_evacuation(dev, chunks.clone(), targets, 4096);
        assert_eq!(plan.chunks_to_evacuate.len(), 3);
        assert_eq!(plan.device_id, dev);
        assert_eq!(plan.estimated_bytes, 4096);
        assert_eq!(plan.target_devices.len(), 2);
    }

    #[test]
    fn no_targets_yields_empty_target_list() {
        let dev = [0x11; 16];
        let chunks = vec![ChunkId([0x99; 32])];

        let plan = plan_evacuation(dev, chunks, vec![], 1024);
        assert!(plan.target_devices.is_empty());
        assert_eq!(plan.chunks_to_evacuate.len(), 1);
    }

    #[test]
    fn cancel_flag_works() {
        let progress = EvacuationProgress::new([0x01; 16], 10);
        assert!(!progress.cancelled.load(Ordering::Acquire));

        progress.cancel();
        assert!(progress.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn is_complete_reflects_progress() {
        let progress = EvacuationProgress::new([0x02; 16], 3);
        assert!(!progress.is_complete());

        progress.chunks_evacuated.store(3, Ordering::Release);
        assert!(progress.is_complete());
    }
}
