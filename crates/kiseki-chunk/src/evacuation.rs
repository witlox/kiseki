//! Device evacuation worker.
//!
//! Plans and tracks the evacuation of chunks from a device that is being
//! decommissioned or has been flagged unhealthy.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kiseki_common::ids::ChunkId;
use kiseki_common::locks::LockOrDie;

/// Live progress tracker for an in-flight evacuation.
#[derive(Debug)]
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

/// Registry of in-flight evacuations, keyed by `evacuation_id`
/// (a UUID). Storage admin's `EvacuateDevice` (W5) inserts an
/// entry on start; the in-flight worker shares the
/// [`EvacuationProgress`] handle so it sees `cancel()` flipping
/// `cancelled` between chunks and can stop cleanly. ADR-025 W4
/// `CancelEvacuation` looks up by id and triggers cancellation.
#[derive(Debug, Default)]
pub struct EvacuationRegistry {
    inner: Mutex<HashMap<String, Arc<EvacuationProgress>>>,
}

impl EvacuationRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new in-flight evacuation. The registry holds an
    /// `Arc` clone so the worker thread can drop its handle when
    /// it finishes — a subsequent `cancel()` becomes a no-op once
    /// the only reference is the worker's already-dropped one.
    pub fn register(&self, id: String, progress: Arc<EvacuationProgress>) {
        let mut g = self
            .inner
            .lock()
            .lock_or_die("evacuation.inner");
        g.insert(id, progress);
    }

    /// Cancel the evacuation identified by `id`. Returns `true` if
    /// the registry held an entry (whether or not `cancel()` had
    /// already been called); `false` if the id was unknown — the
    /// admin RPC translates `false` into `NotFound`.
    pub fn cancel(&self, id: &str) -> bool {
        let g = self
            .inner
            .lock()
            .lock_or_die("evacuation.inner");
        if let Some(p) = g.get(id) {
            p.cancel();
            true
        } else {
            false
        }
    }

    /// Snapshot the current set of evacuation ids. Test helper
    /// today; W5's `ListEvacuations` will use it (or a
    /// progress-projection variant).
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let g = self
            .inner
            .lock()
            .lock_or_die("evacuation.inner");
        g.keys().cloned().collect()
    }

    /// Remove an entry once its worker has finished. Idempotent.
    pub fn unregister(&self, id: &str) {
        let mut g = self
            .inner
            .lock()
            .lock_or_die("evacuation.inner");
        g.remove(id);
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

    #[test]
    fn registry_cancel_flips_progress_flag() {
        let r = EvacuationRegistry::new();
        let p = Arc::new(EvacuationProgress::new([0x03; 16], 5));
        r.register("ev-1".to_owned(), Arc::clone(&p));
        let cancelled = r.cancel("ev-1");
        assert!(cancelled, "registered id must report success");
        assert!(p.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn registry_cancel_unknown_id_returns_false() {
        let r = EvacuationRegistry::new();
        assert!(!r.cancel("no-such-id"));
    }

    #[test]
    fn registry_unregister_removes_entry() {
        let r = EvacuationRegistry::new();
        let p = Arc::new(EvacuationProgress::new([0x04; 16], 1));
        r.register("ev-2".to_owned(), p);
        r.unregister("ev-2");
        assert!(!r.cancel("ev-2"), "after unregister id must be absent");
        assert!(r.ids().is_empty());
    }

    #[test]
    fn registry_ids_lists_all_entries() {
        let r = EvacuationRegistry::new();
        r.register(
            "a".to_owned(),
            Arc::new(EvacuationProgress::new([1; 16], 1)),
        );
        r.register(
            "b".to_owned(),
            Arc::new(EvacuationProgress::new([2; 16], 1)),
        );
        let mut ids = r.ids();
        ids.sort();
        assert_eq!(ids, vec!["a".to_owned(), "b".to_owned()]);
    }
}
