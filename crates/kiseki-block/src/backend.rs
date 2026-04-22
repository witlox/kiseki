//! `DeviceBackend` trait — uniform interface for block device I/O.

use crate::error::{AllocError, BlockError};
use crate::extent::Extent;
use crate::probe::DeviceCharacteristics;

/// Abstraction over a storage device — raw block or file-backed.
///
/// Auto-detects device characteristics and adapts I/O strategy.
/// Callers never need to know which backend is in use.
pub trait DeviceBackend: Send + Sync {
    /// Allocate a contiguous extent of at least `size` bytes.
    /// Alignment matches the device's physical block size.
    fn alloc(&self, size: u64) -> Result<Extent, AllocError>;

    /// Write data at the given extent. Appends CRC32 trailer.
    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError>;

    /// Read data from the given extent. Verifies CRC32 trailer.
    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError>;

    /// Free an extent, returning blocks to the free pool.
    fn free(&self, extent: &Extent) -> Result<(), AllocError>;

    /// Sync all pending writes to stable storage.
    fn sync(&self) -> Result<(), BlockError>;

    /// Device capacity: (`used_bytes`, `total_bytes`).
    fn capacity(&self) -> (u64, u64);

    /// Probed device characteristics (read-only after open).
    fn characteristics(&self) -> &DeviceCharacteristics;

    /// Device UUID from superblock.
    fn device_id(&self) -> [u8; 16];

    /// Get a copy of the allocation bitmap (for persistence/scrub).
    fn bitmap_bytes(&self) -> Vec<u8>;

    /// Run a consistency scrub: verify bitmap integrity, detect orphan
    /// extents, check CRC32 on sampled blocks. Returns a human-readable
    /// report or empty string if clean. Default: no-op.
    fn scrub(&self) -> String {
        String::new()
    }
}

/// Compute CRC32 of data using the Castagnoli polynomial (CRC32C).
/// This is hardware-accelerated on modern x86 (SSE4.2) and ARM (CRC).
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    // Simple software CRC32C (IEEE polynomial).
    // In production, replace with a hardware-accelerated crate.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0x82F6_3B78; // Castagnoli polynomial
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_deterministic() {
        let data = b"hello world";
        assert_eq!(crc32c(data), crc32c(data));
    }

    #[test]
    fn crc32c_different_data() {
        assert_ne!(crc32c(b"hello"), crc32c(b"world"));
    }

    #[test]
    fn crc32c_empty() {
        let _ = crc32c(b""); // Should not panic.
    }

    #[test]
    fn crc32c_test_vector() {
        // Known CRC32C test vector: CRC32C("123456789") = 0xE3069283
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }
}
