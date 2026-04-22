//! Extent — a contiguous range on a block device.

use serde::{Deserialize, Serialize};

/// A contiguous range of blocks on a device.
///
/// All offsets and lengths are in bytes, aligned to the device's
/// physical block size.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Extent {
    /// Byte offset from the start of the data region.
    pub offset: u64,
    /// Length in bytes (block-aligned).
    pub length: u64,
}

impl Extent {
    /// Create a new extent.
    #[must_use]
    pub fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    /// End offset (exclusive).
    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }

    /// Check if this extent is adjacent to (and directly before) another.
    #[must_use]
    pub fn is_adjacent_before(&self, other: &Self) -> bool {
        self.end() == other.offset
    }

    /// Merge with an adjacent extent. Returns None if not adjacent.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Option<Self> {
        if self.is_adjacent_before(other) {
            Some(Self::new(self.offset, self.length + other.length))
        } else if other.is_adjacent_before(self) {
            Some(Self::new(other.offset, self.length + other.length))
        } else {
            None
        }
    }
}

impl std::fmt::Display for Extent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:#x}..{:#x} ({} bytes)]",
            self.offset,
            self.end(),
            self.length
        )
    }
}

impl Ord for Extent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for Extent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_adjacent() {
        let a = Extent::new(0, 4096);
        let b = Extent::new(4096, 4096);
        let merged = a.merge(&b).unwrap();
        assert_eq!(merged, Extent::new(0, 8192));
    }

    #[test]
    fn merge_non_adjacent_returns_none() {
        let a = Extent::new(0, 4096);
        let b = Extent::new(8192, 4096);
        assert!(a.merge(&b).is_none());
    }

    #[test]
    fn merge_reversed() {
        let a = Extent::new(4096, 4096);
        let b = Extent::new(0, 4096);
        let merged = a.merge(&b).unwrap();
        assert_eq!(merged, Extent::new(0, 8192));
    }
}
