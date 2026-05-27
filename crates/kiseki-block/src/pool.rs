//! `DevicePool` — spans N heterogeneous JBOD members into one logical
//! [`DeviceBackend`] (ADR-024: each NVMe/SSD/HDD is an independent pool
//! member; the node presents their combined capacity).
//!
//! A single global address space lets the pool present as one
//! `DeviceBackend` to [`PersistentChunkStore`] without changing the
//! [`Extent`] type or the persisted chunk-meta format. Each member owns
//! a contiguous slice of the global offset space: member `i` occupies
//! `[bases[i], bases[i] + member_total[i])`. `alloc` picks a member and
//! offsets its local extent into that slice; `read`/`write`/`free`
//! route a global offset back to the owning member. An extent never
//! spans a member boundary (alloc always lands inside one member), so
//! routing is a single range lookup.
//!
//! [`PersistentChunkStore`]: ../../kiseki_chunk/persistent_store/struct.PersistentChunkStore.html
//! [`Extent`]: crate::extent::Extent

use std::sync::Arc;

use crate::backend::{DeviceBackend, DeviceUsage};
use crate::error::{AllocError, BlockError};
use crate::extent::Extent;
use crate::probe::{DeviceCharacteristics, StorageTier};

/// Aggregates several [`DeviceBackend`] members behind one backend.
pub struct DevicePool {
    members: Vec<Arc<dyn DeviceBackend>>,
    /// `bases[i]` = global offset of member `i`'s slice (cumulative sum
    /// of preceding members' total capacities). Length == members.
    bases: Vec<u64>,
    /// Cost/performance tier of each member (parallel to `members`),
    /// cached at construction so `alloc` doesn't re-probe.
    tiers: Vec<StorageTier>,
    total_bytes: u64,
    device_id: [u8; 16],
    characteristics: DeviceCharacteristics,
}

impl DevicePool {
    /// Build a pool over `members` (must be non-empty). Member order is
    /// the caller's `KISEKI_RAW_DEVICES` order and must be stable across
    /// restarts — the global address space (hence every persisted
    /// extent offset) depends on it.
    ///
    /// # Errors
    /// Returns [`BlockError::NotInitialized`] if `members` is empty.
    pub fn new(members: Vec<Arc<dyn DeviceBackend>>) -> Result<Self, BlockError> {
        if members.is_empty() {
            return Err(BlockError::NotInitialized);
        }
        let mut bases = Vec::with_capacity(members.len());
        let mut tiers = Vec::with_capacity(members.len());
        let mut acc = 0u64;
        for m in &members {
            bases.push(acc);
            tiers.push(StorageTier::of(m.characteristics().medium));
            acc = acc.saturating_add(m.capacity().1);
        }
        let device_id = members[0].device_id();
        let characteristics = members[0].characteristics().clone();
        Ok(Self {
            members,
            bases,
            tiers,
            total_bytes: acc,
            device_id,
            characteristics,
        })
    }

    /// Number of pool members.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Always false — [`new`](Self::new) rejects an empty member list.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Free bytes on member `i`.
    fn member_free(&self, i: usize) -> u64 {
        let (used, total) = self.members[i].capacity();
        total.saturating_sub(used)
    }

    /// Try members in `order` until one satisfies the request; translate
    /// the chosen member's local extent into the pool's global address
    /// space.
    fn try_alloc_ordered(&self, size: u64, order: &[usize]) -> Result<Extent, AllocError> {
        let mut last_err = AllocError::DeviceFull {
            requested: size,
            available: 0,
        };
        for &i in order {
            match self.members[i].alloc(size) {
                Ok(local) => return Ok(Extent::new(self.bases[i] + local.offset, local.length)),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Map a global offset to `(member_index, local_offset)`.
    fn route(&self, global_offset: u64) -> (usize, u64) {
        // Largest i with bases[i] <= global_offset. N is small (≤ 16);
        // a linear scan from the top is simplest and branch-friendly.
        let mut idx = 0usize;
        for (i, &base) in self.bases.iter().enumerate() {
            if base <= global_offset {
                idx = i;
            } else {
                break;
            }
        }
        (idx, global_offset - self.bases[idx])
    }
}

impl DeviceBackend for DevicePool {
    fn alloc(&self, size: u64) -> Result<Extent, AllocError> {
        // Class-aware default placement (ADR-024): prefer the fastest
        // tier, and within a tier the member with the most free space.
        // Spill to the next-slower tier only when the faster ones can't
        // satisfy the request — so hot data lands on NVMe across a
        // heterogeneous fleet, and the cluster degrades to bulk/cold
        // instead of returning ENOSPC while slower capacity remains.
        let mut order: Vec<usize> = (0..self.members.len()).collect();
        order.sort_by_key(|&i| (self.tiers[i], std::cmp::Reverse(self.member_free(i))));
        self.try_alloc_ordered(size, &order)
    }

    fn alloc_in_tier(&self, size: u64, tier: Option<StorageTier>) -> Result<Extent, AllocError> {
        // ADR-045 §D4: honor an explicit tier hint — place on the
        // requested tier's members first (most-free), then spill to the
        // remaining tiers in cost order (fast → cold). No hint falls
        // back to the fastest-fit default.
        let Some(want) = tier else {
            return self.alloc(size);
        };
        let mut order: Vec<usize> = (0..self.members.len()).collect();
        order.sort_by_key(|&i| {
            (
                self.tiers[i] != want, // requested tier sorts first
                self.tiers[i],         // then cost order among the rest
                std::cmp::Reverse(self.member_free(i)),
            )
        });
        self.try_alloc_ordered(size, &order)
    }

    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError> {
        let (i, local) = self.route(extent.offset);
        self.members[i].write(&Extent::new(local, extent.length), data)
    }

    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError> {
        let (i, local) = self.route(extent.offset);
        self.members[i].read(&Extent::new(local, extent.length))
    }

    fn free(&self, extent: &Extent) -> Result<(), AllocError> {
        let (i, local) = self.route(extent.offset);
        self.members[i].free(&Extent::new(local, extent.length))
    }

    fn sync(&self) -> Result<(), BlockError> {
        // Sync every member; surface the first error after attempting
        // all so a single bad device can't leave others unflushed.
        let mut first_err = None;
        for m in &self.members {
            if let Err(e) = m.sync() {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn capacity(&self) -> (u64, u64) {
        let used = self.members.iter().map(|m| m.capacity().0).sum();
        (used, self.total_bytes)
    }

    fn characteristics(&self) -> &DeviceCharacteristics {
        &self.characteristics
    }

    fn device_id(&self) -> [u8; 16] {
        self.device_id
    }

    fn bitmap_bytes(&self) -> Vec<u8> {
        // Concatenate member bitmaps — used by scrub/debug, not on the
        // hot path.
        let mut out = Vec::new();
        for m in &self.members {
            out.extend_from_slice(&m.bitmap_bytes());
        }
        out
    }

    fn device_breakdown(&self) -> Vec<DeviceUsage> {
        // One entry per member (heterogeneous JBOD). flat_map so a
        // nested pool still flattens to one entry per physical device.
        self.members
            .iter()
            .flat_map(|m| m.device_breakdown())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileBackedDevice;
    use tempfile::tempdir;

    const MB: u64 = 1024 * 1024;

    fn member(dir: &std::path::Path, name: &str, size: u64) -> Arc<dyn DeviceBackend> {
        Arc::new(FileBackedDevice::init(&dir.join(name), size).unwrap())
    }

    #[test]
    fn capacity_is_sum_of_members() {
        let dir = tempdir().unwrap();
        let pool = DevicePool::new(vec![
            member(dir.path(), "a.dev", 64 * MB),
            member(dir.path(), "b.dev", 32 * MB),
            member(dir.path(), "c.dev", 16 * MB),
        ])
        .unwrap();
        let (_used, total) = pool.capacity();
        // Sum of the three members' data-region totals.
        let expected: u64 = [64 * MB, 32 * MB, 16 * MB]
            .iter()
            .map(|&s| {
                FileBackedDevice::init(&dir.path().join(format!("probe-{s}")), s)
                    .unwrap()
                    .capacity()
                    .1
            })
            .sum();
        assert_eq!(total, expected);
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn write_read_roundtrip_across_members() {
        let dir = tempdir().unwrap();
        let pool = DevicePool::new(vec![
            member(dir.path(), "a.dev", 8 * MB),
            member(dir.path(), "b.dev", 8 * MB),
            member(dir.path(), "c.dev", 8 * MB),
        ])
        .unwrap();
        // Enough distinct writes to land on more than one member.
        let mut kept = Vec::new();
        for i in 0..300u32 {
            let payload = format!("pool payload {i} ----------------------------------------");
            let ext = pool.alloc(payload.len() as u64).unwrap();
            pool.write(&ext, payload.as_bytes()).unwrap();
            kept.push((ext, payload));
        }
        for (ext, expected) in &kept {
            assert_eq!(pool.read(ext).unwrap(), expected.as_bytes());
        }
    }

    #[test]
    fn routing_hits_distinct_members() {
        let dir = tempdir().unwrap();
        let pool = DevicePool::new(vec![
            member(dir.path(), "a.dev", 8 * MB),
            member(dir.path(), "b.dev", 8 * MB),
        ])
        .unwrap();
        // An offset in member-0's slice routes to 0; an offset past
        // member-0's total routes to 1.
        let m0_total = pool.members[0].capacity().1;
        assert_eq!(pool.route(0), (0, 0));
        assert_eq!(pool.route(4096), (0, 4096));
        assert_eq!(pool.route(m0_total), (1, 0));
        assert_eq!(pool.route(m0_total + 4096), (1, 4096));
    }

    #[test]
    fn free_returns_capacity_across_members() {
        let dir = tempdir().unwrap();
        let pool = DevicePool::new(vec![
            member(dir.path(), "a.dev", 8 * MB),
            member(dir.path(), "b.dev", 8 * MB),
        ])
        .unwrap();
        let mut exts = Vec::new();
        for _ in 0..50 {
            exts.push(pool.alloc(8192).unwrap());
        }
        assert!(pool.capacity().0 > 0);
        for e in &exts {
            pool.free(e).unwrap();
        }
        assert_eq!(pool.capacity().0, 0);
    }

    #[test]
    fn empty_pool_rejected() {
        assert!(DevicePool::new(vec![]).is_err());
    }

    /// Mock that wraps a file-backed device but reports a chosen medium,
    /// so tier-preference routing can be exercised (a real file always
    /// probes as `Bulk`).
    struct TierMock {
        inner: FileBackedDevice,
        chars: crate::probe::DeviceCharacteristics,
    }

    impl TierMock {
        fn new(path: &std::path::Path, size: u64, medium: crate::probe::DetectedMedium) -> Self {
            let inner = FileBackedDevice::init(path, size).unwrap();
            let mut chars = crate::probe::DeviceCharacteristics::file_backed_defaults();
            chars.medium = medium;
            Self { inner, chars }
        }
    }

    impl DeviceBackend for TierMock {
        fn alloc(&self, size: u64) -> Result<Extent, AllocError> {
            self.inner.alloc(size)
        }
        fn write(&self, e: &Extent, d: &[u8]) -> Result<(), BlockError> {
            self.inner.write(e, d)
        }
        fn read(&self, e: &Extent) -> Result<Vec<u8>, BlockError> {
            self.inner.read(e)
        }
        fn free(&self, e: &Extent) -> Result<(), AllocError> {
            self.inner.free(e)
        }
        fn sync(&self) -> Result<(), BlockError> {
            self.inner.sync()
        }
        fn capacity(&self) -> (u64, u64) {
            self.inner.capacity()
        }
        fn characteristics(&self) -> &crate::probe::DeviceCharacteristics {
            &self.chars
        }
        fn device_id(&self) -> [u8; 16] {
            self.inner.device_id()
        }
        fn bitmap_bytes(&self) -> Vec<u8> {
            self.inner.bitmap_bytes()
        }
    }

    #[test]
    fn alloc_in_tier_prefers_requested_class_then_spills() {
        use crate::probe::{DetectedMedium, StorageTier};
        let dir = tempdir().unwrap();
        // member 0 = fast (NVMe), member 1 = cold (HDD).
        let fast: Arc<dyn DeviceBackend> = Arc::new(TierMock::new(
            &dir.path().join("fast"),
            8 * MB,
            DetectedMedium::NvmeSsd,
        ));
        let cold: Arc<dyn DeviceBackend> = Arc::new(TierMock::new(
            &dir.path().join("cold"),
            8 * MB,
            DetectedMedium::Hdd,
        ));
        let pool = DevicePool::new(vec![fast, cold]).unwrap();
        let fast_total = pool.members[0].capacity().1; // member-0 slice = [0, fast_total)

        // Cold hint → lands on the cold member (global offset past member-0).
        let c = pool.alloc_in_tier(4096, Some(StorageTier::Cold)).unwrap();
        assert!(
            c.offset >= fast_total,
            "Cold hint must land on the cold member"
        );

        // Fast hint → lands on the fast member.
        let f = pool.alloc_in_tier(4096, Some(StorageTier::Fast)).unwrap();
        assert!(
            f.offset < fast_total,
            "Fast hint must land on the fast member"
        );

        // No hint → fastest-fit default (fast member).
        let n = pool.alloc_in_tier(4096, None).unwrap();
        assert!(n.offset < fast_total, "no hint defaults to fastest tier");
    }
}
