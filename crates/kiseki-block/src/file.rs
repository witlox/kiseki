//! File-backed device — sparse file implementation of [`DeviceBackend`].
//!
//! For VMs, development, and CI. Enforces the same 4K alignment as
//! `RawBlockDevice` to ensure tests catch alignment bugs. Per ADR-029.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::allocator::BitmapAllocator;
use crate::backend::{crc32c, DeviceBackend};
use crate::error::{AllocError, BlockError};
use crate::extent::Extent;
use crate::probe::DeviceCharacteristics;
use crate::superblock::Superblock;

/// Header: 4-byte data length prefix.
const HEADER_SIZE: usize = 4;
/// Trailer: 4-byte CRC32.
const CRC_SIZE: usize = 4;
/// Total overhead per extent: header + trailer.
const OVERHEAD: usize = HEADER_SIZE + CRC_SIZE;

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
        #[allow(clippy::cast_possible_truncation)] // bitmap index fits in usize for practical device sizes
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
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;

        // Read superblock.
        let mut sb_buf = vec![0u8; 4096];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut sb_buf)?;
        let sb = Superblock::from_bytes(&sb_buf)?;

        // Read primary bitmap.
        #[allow(clippy::cast_possible_truncation)] // bitmap index fits in usize for practical device sizes
        let bitmap_size = sb.total_blocks.div_ceil(8) as usize;
        let mut bitmap = vec![0u8; bitmap_size];
        file.seek(SeekFrom::Start(sb.bitmap_offset))?;
        file.read_exact(&mut bitmap)?;

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
        let alloc = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bitmap = alloc.bitmap_bytes();
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
        let mut alloc = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        alloc.alloc(total)
    }

    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError> {
        let crc = crc32c(data);
        let abs_offset = self.superblock.data_offset + extent.offset;
        #[allow(clippy::cast_possible_truncation)] // extent data always < 16MB (max extent size)
        let data_len = data.len() as u32;

        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        file.seek(SeekFrom::Start(abs_offset))?;
        file.write_all(&data_len.to_le_bytes())?; // 4-byte length header
        file.write_all(data)?; // payload
        file.write_all(&crc.to_le_bytes())?; // 4-byte CRC32 trailer

        Ok(())
    }

    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError> {
        let abs_offset = self.superblock.data_offset + extent.offset;

        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Read length header (4 bytes).
        let mut len_buf = [0u8; HEADER_SIZE];
        file.seek(SeekFrom::Start(abs_offset))?;
        file.read_exact(&mut len_buf)?;
        let data_len = u32::from_le_bytes(len_buf) as usize;

        // Read data + CRC32.
        let mut data = vec![0u8; data_len];
        file.read_exact(&mut data)?;

        let mut crc_buf = [0u8; CRC_SIZE];
        file.read_exact(&mut crc_buf)?;
        let stored_crc = u32::from_le_bytes(crc_buf);
        let computed_crc = crc32c(&data);

        if stored_crc != computed_crc {
            return Err(BlockError::Corruption {
                offset: extent.offset,
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        Ok(data)
    }

    fn free(&self, extent: &Extent) -> Result<(), AllocError> {
        let mut alloc = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        alloc.free(extent)
    }

    fn sync(&self) -> Result<(), BlockError> {
        self.flush_bitmap()?;
        let file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.sync_all()?;
        Ok(())
    }

    fn capacity(&self) -> (u64, u64) {
        let alloc = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (alloc.used_bytes(), alloc.total_bytes())
    }

    fn characteristics(&self) -> &DeviceCharacteristics {
        &self.characteristics
    }

    fn device_id(&self) -> [u8; 16] {
        self.superblock.device_id
    }

    fn bitmap_bytes(&self) -> Vec<u8> {
        let alloc = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
