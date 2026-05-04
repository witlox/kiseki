//! File-backed device — sparse file implementation of [`DeviceBackend`].
//!
//! For VMs, development, and CI. Enforces the same 4K alignment as
//! `RawBlockDevice` to ensure tests catch alignment bugs. Per ADR-029.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::allocator::{BitmapAllocator, MAX_EXTENT_BYTES};
use crate::backend::{crc32c, DeviceBackend};
use crate::error::{AllocError, BlockError};
use crate::extent::Extent;
use crate::probe::DeviceCharacteristics;
use crate::superblock::Superblock;
use kiseki_common::locks::LockOrDie;

/// Header: 4-byte data length prefix.
const HEADER_SIZE: usize = 4;
/// Trailer: 4-byte CRC32.
const CRC_SIZE: usize = 4;
/// Total overhead per extent: header + trailer.
const OVERHEAD: usize = HEADER_SIZE + CRC_SIZE;

/// Maximum payload bytes (ciphertext) that fit in a single extent
/// after subtracting the per-extent header + CRC trailer overhead.
///
/// Callers writing payloads larger than this MUST split into multiple
/// extents — `alloc()` caps any single allocation at
/// [`MAX_EXTENT_BYTES`].
pub const MAX_EXTENT_PAYLOAD_BYTES: u64 = MAX_EXTENT_BYTES - OVERHEAD as u64;

/// File-backed device — uses a sparse file on the host filesystem.
///
/// Enforces 4K alignment to match `RawBlockDevice` behavior.
pub struct FileBackedDevice {
    _path: PathBuf,
    file: Mutex<File>,
    superblock: Superblock,
    allocator: Mutex<BitmapAllocator>,
    characteristics: DeviceCharacteristics,
}

impl FileBackedDevice {
    /// Initialize a new file-backed device at `path` with `size_bytes` capacity.
    ///
    /// Creates a sparse file, writes superblock and empty bitmap.
    pub fn init(path: &Path, size_bytes: u64) -> Result<Self, BlockError> {
        let chars = DeviceCharacteristics::file_backed_defaults();
        let sb = Superblock::new(size_bytes, chars.physical_block_size);

        // Check for existing superblock.
        if path.exists() {
            let mut f = File::open(path)?;
            let mut buf = vec![0u8; 4096];
            if f.read(&mut buf)? >= 8 && buf[..8] == crate::superblock::MAGIC {
                return Err(BlockError::AlreadyInitialized);
            }
        }

        // Create sparse file.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Set file size (sparse — doesn't allocate all blocks).
        file.set_len(size_bytes)?;

        // Write superblock.
        {
            let mut f = &file;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&sb.to_bytes())?;
        }

        // Write empty bitmaps (all zeros = all free).
        #[allow(clippy::cast_possible_truncation)]
        // bitmap index fits in usize for practical device sizes
        let bitmap_size = sb.total_blocks.div_ceil(8) as usize;
        let empty_bitmap = vec![0u8; bitmap_size];
        {
            let mut f = &file;
            f.seek(SeekFrom::Start(sb.bitmap_offset))?;
            f.write_all(&empty_bitmap)?;
            f.seek(SeekFrom::Start(sb.bitmap_mirror_offset))?;
            f.write_all(&empty_bitmap)?;
            f.sync_all()?;
        }

        let allocator = BitmapAllocator::new(sb.total_blocks, sb.block_size);

        Ok(Self {
            _path: path.to_owned(),
            file: Mutex::new(file),
            superblock: sb,
            allocator: Mutex::new(allocator),
            characteristics: chars,
        })
    }

    /// Open an existing file-backed device.
    pub fn open(path: &Path) -> Result<Self, BlockError> {
        if !path.exists() {
            return Err(BlockError::NotInitialized);
        }
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;

        // Read superblock.
        let mut sb_buf = vec![0u8; 4096];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut sb_buf)?;
        let sb = Superblock::from_bytes(&sb_buf)?;

        // Read primary bitmap.
        let bitmap_size_u64 = sb.total_blocks.div_ceil(8);
        assert!(
            usize::try_from(bitmap_size_u64).is_ok(),
            "bitmap too large for this platform"
        );
        #[allow(clippy::cast_possible_truncation)]
        // guarded by assert above
        let bitmap_size = bitmap_size_u64 as usize;
        let mut bitmap = vec![0u8; bitmap_size];
        file.seek(SeekFrom::Start(sb.bitmap_offset))?;
        file.read_exact(&mut bitmap)?;

        // Read mirror bitmap and compare with primary.
        let mut mirror = vec![0u8; bitmap_size];
        file.seek(SeekFrom::Start(sb.bitmap_mirror_offset))?;
        file.read_exact(&mut mirror)?;
        if bitmap != mirror {
            tracing::warn!("bitmap primary/mirror mismatch detected, using primary");
        }

        let allocator = BitmapAllocator::from_bitmap(bitmap, sb.total_blocks, sb.block_size);
        let chars = DeviceCharacteristics::file_backed_defaults();

        Ok(Self {
            _path: path.to_owned(),
            file: Mutex::new(file),
            superblock: sb,
            allocator: Mutex::new(allocator),
            characteristics: chars,
        })
    }

    /// Flush the bitmap to both primary and mirror regions on the file.
    fn flush_bitmap(&self) -> Result<(), BlockError> {
        let alloc = self.allocator.lock().lock_or_die("file.allocator");
        let bitmap = alloc.bitmap_bytes();
        let mut file = self.file.lock().lock_or_die("file.file");

        // Write primary.
        file.seek(SeekFrom::Start(self.superblock.bitmap_offset))?;
        file.write_all(bitmap)?;

        // Write mirror.
        file.seek(SeekFrom::Start(self.superblock.bitmap_mirror_offset))?;
        file.write_all(bitmap)?;

        Ok(())
    }
}

impl DeviceBackend for FileBackedDevice {
    fn alloc(&self, size: u64) -> Result<Extent, AllocError> {
        // Add overhead (length header + CRC32 trailer).
        let total = size + OVERHEAD as u64;
        let mut alloc = self.allocator.lock().lock_or_die("file.allocator");
        alloc.alloc(total)
    }

    #[tracing::instrument(skip(self, data), fields(offset = extent.offset, length = extent.length, bytes = data.len()))]
    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError> {
        if data.len() > u32::MAX as usize {
            tracing::warn!(bytes = data.len(), "block file write: data exceeds 4 GiB");
            return Err(BlockError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "data exceeds 4GB",
            )));
        }
        // Bug 5 (GCP 2026-05-04): without this guard, an oversized
        // payload silently writes past the extent boundary into the
        // next allocator region, corrupting whatever chunk lives
        // there. The bitmap allocator caps any single extent at
        // MAX_EXTENT_BYTES; callers writing larger payloads must split
        // across multiple extents.
        let payload_capacity = extent.length.saturating_sub(OVERHEAD as u64);
        if data.len() as u64 > payload_capacity {
            tracing::warn!(
                bytes = data.len(),
                extent_length = extent.length,
                payload_capacity,
                "block file write: data exceeds extent payload capacity",
            );
            return Err(BlockError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "data ({} bytes) exceeds extent payload capacity ({} bytes)",
                    data.len(),
                    payload_capacity
                ),
            )));
        }
        let crc = crc32c(data);
        let abs_offset = self.superblock.data_offset + extent.offset;
        #[allow(clippy::cast_possible_truncation)] // guarded by check above
        let data_len = data.len() as u32;

        let mut file = self.file.lock().lock_or_die("file.file");

        file.seek(SeekFrom::Start(abs_offset)).inspect_err(|e| {
            tracing::warn!(error = %e, "block file write: seek failed");
        })?;
        file.write_all(&data_len.to_le_bytes()).inspect_err(|e| {
            tracing::warn!(error = %e, "block file write: header write failed");
        })?;
        file.write_all(data).inspect_err(|e| {
            tracing::warn!(error = %e, "block file write: payload write failed");
        })?;
        // Note: partial CRC32 on crash is handled by WAL intent journal
        // (ADR-029 F-I6). If chunk_meta is not committed, the orphan extent
        // is freed by scrub.
        file.write_all(&crc.to_le_bytes()).inspect_err(|e| {
            tracing::warn!(error = %e, "block file write: CRC write failed");
        })?;

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(offset = extent.offset, length = extent.length))]
    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError> {
        let abs_offset = self.superblock.data_offset + extent.offset;

        let mut file = self.file.lock().lock_or_die("file.file");

        // Read length header (4 bytes).
        let mut len_buf = [0u8; HEADER_SIZE];
        file.seek(SeekFrom::Start(abs_offset)).inspect_err(|e| {
            tracing::warn!(error = %e, "block file read: seek failed");
        })?;
        file.read_exact(&mut len_buf).inspect_err(|e| {
            tracing::warn!(error = %e, "block file read: header read failed");
        })?;
        let data_len = u32::from_le_bytes(len_buf) as usize;

        // Bug 5 sibling guard: refuse to read beyond extent boundaries.
        // If the header claims a length larger than the extent can
        // hold, treat as corruption rather than reading into adjacent
        // extents (which would mask the underlying bug and pollute
        // returned data).
        let payload_capacity = extent.length.saturating_sub(OVERHEAD as u64);
        if data_len as u64 > payload_capacity {
            tracing::warn!(
                offset = extent.offset,
                extent_length = extent.length,
                claimed_len = data_len,
                payload_capacity,
                "block file read: header claims length beyond extent — corruption",
            );
            return Err(BlockError::Corruption {
                offset: extent.offset,
                expected: 0,
                actual: 0,
            });
        }

        // Read data + CRC32.
        let mut data = vec![0u8; data_len];
        file.read_exact(&mut data).inspect_err(|e| {
            tracing::warn!(error = %e, data_len, "block file read: payload read failed");
        })?;

        let mut crc_buf = [0u8; CRC_SIZE];
        file.read_exact(&mut crc_buf).inspect_err(|e| {
            tracing::warn!(error = %e, "block file read: CRC read failed");
        })?;
        let stored_crc = u32::from_le_bytes(crc_buf);
        let computed_crc = crc32c(&data);

        if stored_crc != computed_crc {
            tracing::warn!(
                offset = extent.offset,
                expected = stored_crc,
                actual = computed_crc,
                "block file read: CRC mismatch — corruption",
            );
            return Err(BlockError::Corruption {
                offset: extent.offset,
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        Ok(data)
    }

    fn free(&self, extent: &Extent) -> Result<(), AllocError> {
        let mut alloc = self.allocator.lock().lock_or_die("file.allocator");
        alloc.free(extent)
    }

    fn sync(&self) -> Result<(), BlockError> {
        self.flush_bitmap()?;
        let file = self.file.lock().lock_or_die("file.file");
        file.sync_all()?;
        Ok(())
    }

    fn capacity(&self) -> (u64, u64) {
        let alloc = self.allocator.lock().lock_or_die("file.allocator");
        (alloc.used_bytes(), alloc.total_bytes())
    }

    fn characteristics(&self) -> &DeviceCharacteristics {
        &self.characteristics
    }

    fn device_id(&self) -> [u8; 16] {
        self.superblock.device_id
    }

    fn bitmap_bytes(&self) -> Vec<u8> {
        let alloc = self.allocator.lock().lock_or_die("file.allocator");
        alloc.bitmap_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn init_and_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        // Init.
        let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();
        let (used, total) = dev.capacity();
        assert_eq!(used, 0);
        assert!(total > 0);
        dev.sync().unwrap();

        // Reopen.
        let dev2 = FileBackedDevice::open(&path).unwrap();
        let (used2, total2) = dev2.capacity();
        assert_eq!(used2, 0);
        assert_eq!(total2, total);
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();

        let data = b"hello, kiseki block device!";
        let extent = dev.alloc(data.len() as u64).unwrap();
        dev.write(&extent, data).unwrap();

        let read_back = dev.read(&extent).unwrap();
        assert_eq!(&read_back, data);
    }

    #[test]
    fn crc32_detects_corruption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();

        let data = b"important data";
        let extent = dev.alloc(data.len() as u64).unwrap();
        dev.write(&extent, data).unwrap();

        // Corrupt one byte in the data region (skip the 4-byte length header).
        {
            let abs_offset = dev.superblock.data_offset + extent.offset + HEADER_SIZE as u64;
            let mut file = dev.file.lock().unwrap();
            file.seek(SeekFrom::Start(abs_offset)).unwrap();
            file.write_all(&[0xFF]).unwrap(); // Overwrite first data byte.
        }

        // Read should detect corruption.
        let result = dev.read(&extent);
        assert!(matches!(result, Err(BlockError::Corruption { .. })));
    }

    #[test]
    fn alloc_free_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();

        let ext1 = dev.alloc(4096).unwrap();
        let ext2 = dev.alloc(4096).unwrap();
        let (used, _) = dev.capacity();
        assert!(used > 0);

        dev.free(&ext1).unwrap();
        dev.free(&ext2).unwrap();
        let (used_after, _) = dev.capacity();
        assert_eq!(used_after, 0);
    }

    #[test]
    fn data_survives_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        let data = b"persistent block data";
        let extent;

        // Write and sync.
        {
            let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();
            extent = dev.alloc(data.len() as u64).unwrap();
            dev.write(&extent, data).unwrap();
            dev.sync().unwrap();
        }

        // Reopen and read.
        {
            let dev = FileBackedDevice::open(&path).unwrap();
            let read_back = dev.read(&extent).unwrap();
            assert_eq!(&read_back, data);
        }
    }

    #[test]
    fn refuse_double_init() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        FileBackedDevice::init(&path, 64 * MB).unwrap();
        let result = FileBackedDevice::init(&path, 64 * MB);
        assert!(matches!(result, Err(BlockError::AlreadyInitialized)));
    }

    #[test]
    fn alignment_enforced() {
        let chars = DeviceCharacteristics::file_backed_defaults();
        assert_eq!(chars.physical_block_size, 4096);
    }

    /// Bug 5 (GCP 2026-05-04): the bitmap allocator silently truncates
    /// any single request to `MAX_EXTENT_BYTES = 16 MiB`. Combined with
    /// `write` not enforcing `data.len() <= extent.length`, oversized
    /// payloads overran into adjacent extent space. Subsequent writes
    /// then overwrote the first chunk's data, surfacing as a
    /// `BlockError::Corruption` on read.
    ///
    /// Contract: `alloc(N)` returns an extent that fits `N` bytes of
    /// payload, OR errors. Silent truncation is forbidden.
    #[test]
    fn alloc_refuses_request_larger_than_extent_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = FileBackedDevice::init(&path, 256 * MB).unwrap();

        // 64 MiB exceeds the 16 MiB per-extent cap. Caller must learn
        // this and split into multiple alloc + write pairs.
        let result = dev.alloc(64 * MB);
        match result {
            Err(AllocError::RequestTooLarge { requested, max }) => {
                assert!(
                    requested >= 64 * MB,
                    "requested={requested} should be >= asked size",
                );
                assert!(max <= 64 * MB, "max={max} should be <= requested");
            }
            Err(other) => panic!("expected RequestTooLarge, got {other:?}"),
            Ok(extent) => panic!(
                "alloc({}) silently returned {}-byte extent; this lets \
                 callers overrun into adjacent extents",
                64 * MB,
                extent.length,
            ),
        }
    }

    /// Bug 5 (defensive layer): even if a caller passes an oversized
    /// `data` for the extent it holds, `write` must refuse rather than
    /// overrun. Without this guard a single buggy callsite can corrupt
    /// any device.
    #[test]
    fn write_refuses_data_larger_than_extent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();

        // Allocate a small extent.
        let extent = dev.alloc(4096).unwrap();
        // Try to write more than the extent can hold.
        let oversize: Vec<u8> = vec![0xAB; usize::try_from(extent.length).unwrap() + 1];
        let result = dev.write(&extent, &oversize);
        assert!(
            result.is_err(),
            "write of {} bytes into a {}-byte extent should fail, not \
             overrun into adjacent space",
            oversize.len(),
            extent.length
        );
    }

    #[test]
    fn multiple_writes_and_reads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();

        let mut extents = Vec::new();
        for i in 0..100u32 {
            let data = format!("block data {i}");
            let ext = dev.alloc(data.len() as u64).unwrap();
            dev.write(&ext, data.as_bytes()).unwrap();
            extents.push((ext, data));
        }

        for (ext, expected) in &extents {
            let read_back = dev.read(ext).unwrap();
            assert_eq!(std::str::from_utf8(&read_back).unwrap(), expected);
        }
    }
}
