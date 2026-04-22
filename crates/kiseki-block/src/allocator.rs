//! Bitmap-based extent allocator with in-memory free-list cache.
//!
//! Bitmap is the ground truth (on disk). Free-list is a derived
//! B-tree cache rebuilt from bitmap on startup. Per ADR-029.

use std::collections::BTreeMap;

use crate::error::AllocError;
use crate::extent::Extent;

/// Bitmap-based block allocator.
///
/// Thread safety: callers must serialize access via Mutex per device
/// (allocation is microseconds; I/O is the bottleneck).
pub struct BitmapAllocator {
    /// Total blocks in the data region.
    total_blocks: u64,
    /// Block size in bytes.
    block_size: u32,
    /// Allocation bitmap: 1 = allocated, 0 = free.
    bitmap: Vec<u8>,
    /// Free-list cache: offset → length (in blocks), sorted by offset.
    free_list: BTreeMap<u64, u64>,
    /// Maximum extent size in blocks.
    max_extent_blocks: u64,
}

/// Maximum extent size: 16MB.
const MAX_EXTENT_BYTES: u64 = 16 * 1024 * 1024;

impl BitmapAllocator {
    /// Create a new allocator for a device.
    ///
    /// The bitmap is initialized to all-free (zeros).
    #[must_use]
    pub fn new(total_blocks: u64, block_size: u32) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        // bitmap index always fits in usize for practical device sizes
        let bitmap_bytes = total_blocks.div_ceil(8) as usize;
        let bitmap = vec![0u8; bitmap_bytes];
        let max_extent_blocks = MAX_EXTENT_BYTES / u64::from(block_size);

        let mut alloc = Self {
            total_blocks,
            block_size,
            bitmap,
            free_list: BTreeMap::new(),
            max_extent_blocks,
        };
        alloc.rebuild_free_list();
        alloc
    }

    /// Create an allocator from an existing bitmap (loaded from device).
    #[must_use]
    pub fn from_bitmap(bitmap: Vec<u8>, total_blocks: u64, block_size: u32) -> Self {
        let max_extent_blocks = MAX_EXTENT_BYTES / u64::from(block_size);
        let mut alloc = Self {
            total_blocks,
            block_size,
            bitmap,
            free_list: BTreeMap::new(),
            max_extent_blocks,
        };
        alloc.rebuild_free_list();
        alloc
    }

    /// Allocate an extent of at least `size_bytes`.
    ///
    /// Returns a single extent up to `max_extent_blocks`. For larger
    /// requests, the caller must call `alloc` multiple times.
    pub fn alloc(&mut self, size_bytes: u64) -> Result<Extent, AllocError> {
        let blocks_needed = size_bytes.div_ceil(u64::from(self.block_size));
        let blocks_needed = blocks_needed.min(self.max_extent_blocks);

        if blocks_needed == 0 {
            return Err(AllocError::Inconsistency("zero-size allocation".into()));
        }

        // Best-fit: find smallest free extent >= blocks_needed.
        let best = self
            .free_list
            .iter()
            .filter(|(_, &len)| len >= blocks_needed)
            .min_by_key(|(_, &len)| len)
            .map(|(&offset, &len)| (offset, len));

        let Some((offset, free_len)) = best else {
            let largest = self.free_list.values().max().copied().unwrap_or(0);
            return Err(AllocError::DeviceFull {
                requested: size_bytes,
                available: largest * u64::from(self.block_size),
            });
        };

        // Remove the free extent from the list.
        self.free_list.remove(&offset);

        // If the free extent is larger, split and return the remainder.
        if free_len > blocks_needed {
            let remainder_offset = offset + blocks_needed;
            let remainder_len = free_len - blocks_needed;
            self.free_list.insert(remainder_offset, remainder_len);
        }

        // Mark blocks as allocated in bitmap.
        for i in offset..offset + blocks_needed {
            self.set_bit(i, true);
        }

        let byte_offset = offset * u64::from(self.block_size);
        let byte_length = blocks_needed * u64::from(self.block_size);
        Ok(Extent::new(byte_offset, byte_length))
    }

    /// Free an extent, returning blocks to the free pool.
    pub fn free(&mut self, extent: &Extent) -> Result<(), AllocError> {
        let block_offset = extent.offset / u64::from(self.block_size);
        let block_count = extent.length / u64::from(self.block_size);

        if block_offset + block_count > self.total_blocks {
            return Err(AllocError::Inconsistency(format!(
                "free beyond device: offset={}, count={}, total={}",
                block_offset, block_count, self.total_blocks
            )));
        }

        // Clear bitmap bits.
        for i in block_offset..block_offset + block_count {
            self.set_bit(i, false);
        }

        // Insert into free-list and coalesce with neighbors.
        self.free_list.insert(block_offset, block_count);
        self.coalesce(block_offset);

        Ok(())
    }

    /// Get the raw bitmap bytes (for writing to device).
    #[must_use]
    pub fn bitmap_bytes(&self) -> &[u8] {
        &self.bitmap
    }

    /// Total used bytes.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        let free_blocks: u64 = self.free_list.values().sum();
        (self.total_blocks - free_blocks) * u64::from(self.block_size)
    }

    /// Total capacity in bytes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_blocks * u64::from(self.block_size)
    }

    /// Number of free blocks.
    #[must_use]
    pub fn free_blocks(&self) -> u64 {
        self.free_list.values().sum()
    }

    // --- Internal ---

    fn set_bit(&mut self, block: u64, allocated: bool) {
        let byte_idx = (block / 8) as usize;
        let bit_idx = (block % 8) as u8;
        if byte_idx < self.bitmap.len() {
            if allocated {
                self.bitmap[byte_idx] |= 1 << bit_idx;
            } else {
                self.bitmap[byte_idx] &= !(1 << bit_idx);
            }
        }
    }

    fn get_bit(&self, block: u64) -> bool {
        let byte_idx = (block / 8) as usize;
        let bit_idx = (block % 8) as u8;
        if byte_idx < self.bitmap.len() {
            (self.bitmap[byte_idx] >> bit_idx) & 1 == 1
        } else {
            true // Out of range = "allocated" (safety)
        }
    }

    fn rebuild_free_list(&mut self) {
        self.free_list.clear();
        let mut run_start: Option<u64> = None;

        for block in 0..self.total_blocks {
            if self.get_bit(block) {
                // Allocated — end any free run.
                if let Some(start) = run_start.take() {
                    self.free_list.insert(start, block - start);
                }
            } else {
                // Free — start or continue a run.
                if run_start.is_none() {
                    run_start = Some(block);
                }
            }
        }

        // Close final run.
        if let Some(start) = run_start {
            self.free_list.insert(start, self.total_blocks - start);
        }
    }

    fn coalesce(&mut self, block_offset: u64) {
        // Try merge with previous extent.
        if let Some((&prev_offset, &prev_len)) = self.free_list.range(..block_offset).next_back() {
            if prev_offset + prev_len == block_offset {
                let cur_len = self.free_list.remove(&block_offset).unwrap_or(0);
                let merged_len = prev_len + cur_len;
                self.free_list.insert(prev_offset, merged_len);
                // Recurse to check if merged extent coalesces with next.
                self.coalesce_next(prev_offset);
                return;
            }
        }
        self.coalesce_next(block_offset);
    }

    fn coalesce_next(&mut self, block_offset: u64) {
        let Some(&cur_len) = self.free_list.get(&block_offset) else {
            return;
        };
        let next_offset = block_offset + cur_len;
        if let Some(&next_len) = self.free_list.get(&next_offset) {
            self.free_list.remove(&next_offset);
            self.free_list.insert(block_offset, cur_len + next_len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_and_free() {
        let mut alloc = BitmapAllocator::new(1024, 4096); // 1024 blocks, 4K each = 4MB
        let ext = alloc.alloc(4096).unwrap(); // 1 block
        assert_eq!(ext.length, 4096);
        assert_eq!(alloc.free_blocks(), 1023);

        alloc.free(&ext).unwrap();
        assert_eq!(alloc.free_blocks(), 1024);
    }

    #[test]
    fn alloc_rounds_up() {
        let mut alloc = BitmapAllocator::new(100, 4096);
        let ext = alloc.alloc(513).unwrap(); // 513 bytes → 1 block (4096)
        assert_eq!(ext.length, 4096);
    }

    #[test]
    fn alloc_multiple_then_free_coalesces() {
        let mut alloc = BitmapAllocator::new(100, 4096);
        let a = alloc.alloc(4096).unwrap();
        let b = alloc.alloc(4096).unwrap();
        let c = alloc.alloc(4096).unwrap();
        assert_eq!(alloc.free_blocks(), 97);

        // Free middle first.
        alloc.free(&b).unwrap();
        assert_eq!(alloc.free_blocks(), 98);

        // Free first — should coalesce with middle.
        alloc.free(&a).unwrap();
        assert_eq!(alloc.free_blocks(), 99);

        // Free last — should coalesce all three into one run.
        alloc.free(&c).unwrap();
        assert_eq!(alloc.free_blocks(), 100);
        assert_eq!(alloc.free_list.len(), 1); // Single contiguous free extent.
    }

    #[test]
    fn alloc_fails_when_full() {
        let mut alloc = BitmapAllocator::new(2, 4096); // 2 blocks only
        alloc.alloc(4096).unwrap();
        alloc.alloc(4096).unwrap();
        assert!(alloc.alloc(4096).is_err());
    }

    #[test]
    fn bitmap_roundtrip() {
        let mut alloc = BitmapAllocator::new(64, 4096);
        alloc.alloc(4096 * 10).unwrap(); // 10 blocks

        let bitmap = alloc.bitmap_bytes().to_vec();
        let alloc2 = BitmapAllocator::from_bitmap(bitmap, 64, 4096);
        assert_eq!(alloc2.free_blocks(), 54);
    }

    #[test]
    fn best_fit_selection() {
        let mut alloc = BitmapAllocator::new(100, 4096);
        // Allocate blocks 0-4 (5 blocks).
        let a = alloc.alloc(4096 * 5).unwrap();
        // Allocate blocks 5-14 (10 blocks — gap separator).
        let _b = alloc.alloc(4096 * 10).unwrap();
        // Allocate blocks 15-24 (10 blocks).
        let c = alloc.alloc(4096 * 10).unwrap();
        // Allocate blocks 25-34 (10 blocks — gap separator).
        let _d = alloc.alloc(4096 * 10).unwrap();

        // Free a (5 blocks at 0) and c (10 blocks at 15).
        // Free list: {0: 5} and {15: 10}. No coalescing (b and d separate them).
        alloc.free(&a).unwrap();
        alloc.free(&c).unwrap();

        // Allocate 4 blocks — should pick the 5-block extent (best fit).
        let e = alloc.alloc(4096 * 4).unwrap();
        assert_eq!(e.offset, a.offset); // Reuses the smaller 5-block gap.
    }
}
