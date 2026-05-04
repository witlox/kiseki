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
///
/// Delegates to the [`crc32c`] crate which auto-detects SSE4.2
/// (`x86_64`) and ARM CRC intrinsics at runtime. The 2026-05-04
/// docker compose baseline showed the previous hand-rolled
/// bit-by-bit loop running at ~470 MiB/s in release builds —
/// single-handedly responsible for ~34 ms of every 16 MiB fabric
/// `write_chunk`. The HW path clears 5+ GiB/s on modern `x86_64`
/// and 3+ GiB/s on ARM, with NIC line rate to spare.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
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

    /// 2026-05-04 docker compose baseline showed receiver-side
    /// `write_chunk` taking 54 ms for a 16 MiB extent — 64 % of the
    /// fabric PUT round trip. The chunk-store write boils down to
    /// `device.write` which appends a CRC32C trailer, and the
    /// CRC32C in this module is a hand-rolled bit-by-bit loop
    /// (~75 MB/s). For a 16 MiB extent that's ~225 ms of CPU just
    /// computing the checksum — single-handedly capping fabric write
    /// throughput well below the network's 533 MB/s ceiling.
    ///
    /// Pin the contract: CRC32C of a 16 MiB buffer must complete at
    /// ≥ 1 GB/s. The bit-loop won't meet this on any reasonable
    /// hardware; a HW-accelerated implementation (SSE4.2 / ARM CRC)
    /// hits 3–10 GB/s. The 1 GB/s floor leaves headroom for slow CI
    /// runners while still rejecting the bit-loop unambiguously.
    #[test]
    fn crc32c_throughput_at_least_one_gibibyte_per_second() {
        const SIZE: usize = 16 * 1024 * 1024;
        const MIN_MIB_PER_SEC: f64 = 1024.0;

        let data = vec![0xA5u8; SIZE];
        let started = std::time::Instant::now();
        // black_box the input + result so a release build can't elide
        // the call (LLVM is happy to strip a CRC whose result is
        // discarded — release-mode benchmarks need this guard).
        let result = crc32c(std::hint::black_box(&data));
        std::hint::black_box(result);
        let elapsed = started.elapsed();

        #[allow(clippy::cast_precision_loss)] // SIZE is 16 MiB, well below f64 mantissa
        let mib = (SIZE as f64) / (1024.0 * 1024.0);
        let mib_per_sec = mib / elapsed.as_secs_f64();
        assert!(
            mib_per_sec >= MIN_MIB_PER_SEC,
            "crc32c throughput {mib_per_sec:.1} MiB/s on a {SIZE} B buffer — \
             must be ≥ {MIN_MIB_PER_SEC:.0} MiB/s (1 GiB/s). The hand-rolled \
             bit-loop tops out around 75 MiB/s; a HW-accelerated CRC32C \
             implementation (the `crc32c` crate uses SSE4.2 / ARM CRC \
             intrinsics) is required.",
        );
    }
}
