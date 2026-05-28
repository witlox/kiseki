//! Raw block-device backend — opens an existing block device (or a
//! pre-sized regular file) and drives it through the same superblock /
//! bitmap / extent / CRC machinery as [`FileBackedDevice`], honoring
//! the per-medium [`IoStrategy`] probed from sysfs (ADR-024, ADR-029).
//!
//! Differences from [`FileBackedDevice`]:
//!   - Never `set_len`s the target — a block device has a fixed size
//!     reported by `seek(End)`. The file path is for *opening*, not
//!     creating.
//!   - Honors the probed strategy: `O_DIRECT` (page-cache bypass) for
//!     `NVMe` / `SATA` SSD, buffered for HDD (readahead helps), plain
//!     for virtio / file.
//!   - All I/O is block-aligned: the bitmap allocator returns
//!     whole-block extents and the superblock places the data region on
//!     a block boundary, so every read/write covers whole blocks at an
//!     aligned offset — the hard requirement under `O_DIRECT`. No
//!     `unsafe`: aligned buffers come from over-allocate-and-slice.
//!
//! [`FileBackedDevice`]: crate::file::FileBackedDevice

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::allocator::BitmapAllocator;
use crate::backend::{crc32c, DeviceBackend};
use crate::error::{AllocError, BlockError};
use crate::extent::Extent;
use crate::probe::{DeviceCharacteristics, IoStrategy};
use crate::superblock::{Superblock, MAGIC, SUPERBLOCK_SIZE};
use kiseki_common::locks::LockOrDie;

/// Header: 4-byte little-endian payload length prefix.
const HEADER_SIZE: usize = 4;
/// Trailer: 4-byte CRC32C.
const CRC_SIZE: usize = 4;
/// Per-extent framing overhead (must match [`crate::file`]).
const OVERHEAD: usize = HEADER_SIZE + CRC_SIZE;

/// A page-aligned, zero-filled scratch buffer.
///
/// `O_DIRECT` requires the user buffer, the file offset, and the I/O
/// length to be aligned to the device's logical block size. Offsets and
/// lengths are already block-aligned by construction (the allocator
/// hands out whole-block extents and the superblock data region starts
/// on a block boundary); this gives us an aligned *buffer* without
/// `unsafe` by over-allocating and slicing to the first aligned start.
struct AlignedBuf {
    buf: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize, align: usize) -> Self {
        debug_assert!(align.is_power_of_two() && align > 0);
        // Over-allocate by `align` so an aligned start always exists
        // inside the allocation. The Vec's backing pointer is stable for
        // the buffer's lifetime (we never grow it), so the computed
        // offset stays valid.
        let buf = vec![0u8; len + align];
        let addr = buf.as_ptr() as usize;
        let off = (align - (addr % align)) % align;
        Self { buf, off, len }
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[self.off..self.off + self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf[self.off..self.off + self.len]
    }
}

/// Open flags for a given I/O strategy. `O_DIRECT` bypasses the page
/// cache (ADR-029's anti-goal: don't double-cache what our own layer
/// caches); durability is provided by the runtime's periodic
/// `device.sync()` group-commit task plus cross-node replication
/// (I-L5), matching the file-backed path — so we do **not** add
/// `O_DSYNC` here (that would force a per-write barrier and serialize
/// concurrent fabric writers).
fn open_flags(strategy: IoStrategy) -> i32 {
    match strategy {
        #[cfg(target_os = "linux")]
        IoStrategy::DirectAligned => libc::O_DIRECT,
        // Non-Linux unix has no O_DIRECT flag (kiseki targets Linux);
        // fall back to buffered + periodic sync.
        #[cfg(not(target_os = "linux"))]
        IoStrategy::DirectAligned => 0,
        // HDD benefits from kernel readahead — buffered I/O, synced by
        // the group-commit task.
        IoStrategy::BufferedSequential | IoStrategy::FileBacked => 0,
    }
}

/// Raw block-device implementation of [`DeviceBackend`].
pub struct RawBlockDevice {
    _path: PathBuf,
    file: Mutex<File>,
    superblock: Superblock,
    allocator: Mutex<BitmapAllocator>,
    characteristics: DeviceCharacteristics,
}

impl RawBlockDevice {
    /// Open `path` as a chunk-data device, writing the on-disk format
    /// (superblock + empty bitmaps) on first use and rebuilding the
    /// allocator from the persisted bitmap on subsequent opens.
    ///
    /// `path` may be a block device (size via `seek(End)`) or a
    /// pre-sized regular file. Symlinks (e.g. `/dev/disk/by-id/...`) are
    /// resolved before probing so the sysfs classification sees the real
    /// `/dev/<name>`.
    ///
    /// # Errors
    /// Returns [`BlockError`] if the device can't be opened, is too
    /// small to hold a superblock + bitmaps, or carries a superblock
    /// this binary can't parse.
    pub fn open_or_init(path: &Path) -> Result<Self, BlockError> {
        // Resolve by-id / by-path symlinks so the sysfs probe sees the
        // real device node (GCP exposes local SSDs as
        // /dev/disk/by-id/google-local-nvme-ssd-N -> ../../nvmeXn1).
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        let chars = DeviceCharacteristics::probe(&canonical);
        let block_size = chars.physical_block_size;
        let align = block_size as usize;

        let file = open_with_strategy(path, chars.io_strategy)?;

        // A block device reports its capacity via seek-to-end; a
        // regular file reports its current length the same way.
        let mut probe = &file;
        let size_bytes = probe.seek(SeekFrom::End(0))?;
        if size_bytes < SUPERBLOCK_SIZE.saturating_mul(4) {
            return Err(BlockError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "device {} too small for chunk store: {size_bytes} bytes",
                    path.display()
                ),
            )));
        }

        // Read the first block; presence of MAGIC distinguishes
        // open-existing from first-init.
        let sb_size = usize::try_from(SUPERBLOCK_SIZE).expect("SUPERBLOCK_SIZE (4096) fits usize");
        let mut sb_buf = AlignedBuf::new(sb_size, align);
        file.read_exact_at(sb_buf.as_mut_slice(), 0)?;

        let (superblock, allocator) = if sb_buf.as_slice()[..8] == MAGIC {
            let sb = Superblock::from_bytes(sb_buf.as_slice())?;
            let bitmap = read_bitmap(&file, &sb, align)?;
            let alloc = BitmapAllocator::from_bitmap(bitmap, sb.total_blocks, sb.block_size);
            tracing::info!(
                device = %path.display(),
                medium = ?chars.medium,
                strategy = ?chars.io_strategy,
                total_gb = sb.total_blocks * u64::from(sb.block_size) / (1024 * 1024 * 1024),
                "raw block device opened (existing superblock)",
            );
            (sb, alloc)
        } else {
            let sb = Superblock::new(size_bytes, block_size);
            write_superblock(&file, &sb, align)?;
            write_empty_bitmaps(&file, &sb, align)?;
            file.sync_all()?;
            let alloc = BitmapAllocator::new(sb.total_blocks, sb.block_size);
            tracing::info!(
                device = %path.display(),
                medium = ?chars.medium,
                strategy = ?chars.io_strategy,
                total_gb = sb.total_blocks * u64::from(sb.block_size) / (1024 * 1024 * 1024),
                "raw block device initialized (new superblock)",
            );
            (sb, alloc)
        };

        Ok(Self {
            _path: path.to_owned(),
            file: Mutex::new(file),
            superblock,
            allocator: Mutex::new(allocator),
            characteristics: chars,
        })
    }

    /// Flush the in-memory bitmap to both on-disk copies (primary +
    /// mirror). Called from `sync()`.
    fn flush_bitmap(&self) -> Result<(), BlockError> {
        let bitmap = {
            let alloc = self.allocator.lock().lock_or_die("raw.allocator");
            alloc.bitmap_bytes().to_vec()
        };
        let align = self.superblock.block_size as usize;
        let region = bitmap_region_bytes(&self.superblock);
        let mut buf = AlignedBuf::new(region, align);
        buf.as_mut_slice()[..bitmap.len()].copy_from_slice(&bitmap);
        let file = self.file.lock().lock_or_die("raw.file");
        file.write_all_at(buf.as_slice(), self.superblock.bitmap_offset)?;
        file.write_all_at(buf.as_slice(), self.superblock.bitmap_mirror_offset)?;
        Ok(())
    }
}

impl DeviceBackend for RawBlockDevice {
    fn alloc(&self, size: u64) -> Result<Extent, AllocError> {
        let total = size + OVERHEAD as u64;
        let mut alloc = self.allocator.lock().lock_or_die("raw.allocator");
        alloc.alloc(total)
    }

    #[tracing::instrument(skip(self, data), fields(offset = extent.offset, length = extent.length, bytes = data.len()))]
    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError> {
        let payload_capacity = extent.length.saturating_sub(OVERHEAD as u64);
        if data.len() as u64 > payload_capacity {
            tracing::warn!(
                bytes = data.len(),
                extent_length = extent.length,
                payload_capacity,
                "raw write: data exceeds extent payload capacity",
            );
            return Err(BlockError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "data ({} bytes) exceeds extent payload capacity ({payload_capacity} bytes)",
                    data.len(),
                ),
            )));
        }
        #[allow(clippy::cast_possible_truncation)] // guarded by payload_capacity check
        let data_len = data.len() as u32;
        let crc = crc32c(data);

        // The extent is block-aligned in both offset and length (the
        // allocator rounds `size + OVERHEAD` up to whole blocks), so we
        // write the whole extent as one aligned, whole-block I/O —
        // exactly what O_DIRECT demands. Layout: [len][data][crc][pad].
        let align = self.superblock.block_size as usize;
        #[allow(clippy::cast_possible_truncation)] // extent.length is bounded by MAX_EXTENT_BYTES
        let mut buf = AlignedBuf::new(extent.length as usize, align);
        {
            let s = buf.as_mut_slice();
            s[0..HEADER_SIZE].copy_from_slice(&data_len.to_le_bytes());
            s[HEADER_SIZE..HEADER_SIZE + data.len()].copy_from_slice(data);
            let crc_at = HEADER_SIZE + data.len();
            s[crc_at..crc_at + CRC_SIZE].copy_from_slice(&crc.to_le_bytes());
        }
        let abs_offset = self.superblock.data_offset + extent.offset;
        let file = self.file.lock().lock_or_die("raw.file");
        file.write_all_at(buf.as_slice(), abs_offset)
            .inspect_err(|e| {
                tracing::warn!(error = %e, abs_offset, "raw write: write_all_at failed");
            })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(offset = extent.offset, length = extent.length))]
    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError> {
        let align = self.superblock.block_size as usize;
        #[allow(clippy::cast_possible_truncation)] // extent.length is bounded by MAX_EXTENT_BYTES
        let mut buf = AlignedBuf::new(extent.length as usize, align);
        let abs_offset = self.superblock.data_offset + extent.offset;
        {
            let file = self.file.lock().lock_or_die("raw.file");
            file.read_exact_at(buf.as_mut_slice(), abs_offset)
                .inspect_err(|e| {
                    tracing::warn!(error = %e, abs_offset, "raw read: read_exact_at failed");
                })?;
        }
        let s = buf.as_slice();
        let mut len_buf = [0u8; HEADER_SIZE];
        len_buf.copy_from_slice(&s[0..HEADER_SIZE]);
        let data_len = u32::from_le_bytes(len_buf) as usize;

        let payload_capacity = extent.length.saturating_sub(OVERHEAD as u64);
        if data_len as u64 > payload_capacity {
            tracing::warn!(
                offset = extent.offset,
                extent_length = extent.length,
                claimed_len = data_len,
                payload_capacity,
                "raw read: header claims length beyond extent — corruption",
            );
            return Err(BlockError::Corruption {
                offset: extent.offset,
                expected: 0,
                actual: 0,
            });
        }

        let data = s[HEADER_SIZE..HEADER_SIZE + data_len].to_vec();
        let mut crc_buf = [0u8; CRC_SIZE];
        crc_buf.copy_from_slice(&s[HEADER_SIZE + data_len..HEADER_SIZE + data_len + CRC_SIZE]);
        let stored_crc = u32::from_le_bytes(crc_buf);
        let computed_crc = crc32c(&data);
        if stored_crc != computed_crc {
            tracing::warn!(
                offset = extent.offset,
                expected = stored_crc,
                actual = computed_crc,
                "raw read: CRC mismatch — corruption",
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
        let mut alloc = self.allocator.lock().lock_or_die("raw.allocator");
        alloc.free(extent)
    }

    fn sync(&self) -> Result<(), BlockError> {
        self.flush_bitmap()?;
        let file = self.file.lock().lock_or_die("raw.file");
        file.sync_all()?;
        Ok(())
    }

    fn capacity(&self) -> (u64, u64) {
        let alloc = self.allocator.lock().lock_or_die("raw.allocator");
        (alloc.used_bytes(), alloc.total_bytes())
    }

    fn characteristics(&self) -> &DeviceCharacteristics {
        &self.characteristics
    }

    fn device_id(&self) -> [u8; 16] {
        self.superblock.device_id
    }

    fn bitmap_bytes(&self) -> Vec<u8> {
        let alloc = self.allocator.lock().lock_or_die("raw.allocator");
        alloc.bitmap_bytes().to_vec()
    }
}

/// On-disk size (block-aligned) of one bitmap copy.
fn bitmap_region_bytes(sb: &Superblock) -> usize {
    #[allow(clippy::cast_possible_truncation)] // bitmap region fits usize on supported device sizes
    let region = (sb.bitmap_blocks * u64::from(sb.block_size)) as usize;
    region
}

/// Open `path` with the strategy's flags, retrying without `O_DIRECT`
/// if the filesystem rejects it (e.g. tmpfs) so a misconfigured mount
/// degrades to buffered rather than failing to start.
fn open_with_strategy(path: &Path, strategy: IoStrategy) -> Result<File, BlockError> {
    let flags = open_flags(strategy);
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    if flags != 0 {
        opts.custom_flags(flags);
    }
    match opts.open(path) {
        Ok(f) => Ok(f),
        Err(e) if flags != 0 => {
            tracing::warn!(
                device = %path.display(),
                error = %e,
                "raw device open with O_DIRECT failed — retrying buffered",
            );
            Ok(OpenOptions::new().read(true).write(true).open(path)?)
        }
        Err(e) => Err(BlockError::Io(e)),
    }
}

/// Write the superblock into the first block via an aligned buffer.
fn write_superblock(file: &File, sb: &Superblock, align: usize) -> Result<(), BlockError> {
    let bytes = sb.to_bytes();
    let sb_size = usize::try_from(SUPERBLOCK_SIZE).expect("SUPERBLOCK_SIZE (4096) fits usize");
    let mut buf = AlignedBuf::new(sb_size, align);
    buf.as_mut_slice()[..bytes.len()].copy_from_slice(&bytes);
    file.write_all_at(buf.as_slice(), 0)?;
    Ok(())
}

/// Zero both bitmap copies (all blocks free) via aligned, whole-block
/// writes.
fn write_empty_bitmaps(file: &File, sb: &Superblock, align: usize) -> Result<(), BlockError> {
    let region = bitmap_region_bytes(sb);
    let buf = AlignedBuf::new(region, align); // already zeroed
    file.write_all_at(buf.as_slice(), sb.bitmap_offset)?;
    file.write_all_at(buf.as_slice(), sb.bitmap_mirror_offset)?;
    Ok(())
}

/// Read the primary bitmap (logical length, trimmed from the aligned
/// on-disk region) and warn on a primary/mirror mismatch.
fn read_bitmap(file: &File, sb: &Superblock, align: usize) -> Result<Vec<u8>, BlockError> {
    let region = bitmap_region_bytes(sb);
    #[allow(clippy::cast_possible_truncation)]
    // bitmap logical size fits usize on supported devices
    let logical = sb.total_blocks.div_ceil(8) as usize;

    let mut primary = AlignedBuf::new(region, align);
    file.read_exact_at(primary.as_mut_slice(), sb.bitmap_offset)?;
    let mut mirror = AlignedBuf::new(region, align);
    file.read_exact_at(mirror.as_mut_slice(), sb.bitmap_mirror_offset)?;
    if primary.as_slice()[..logical] != mirror.as_slice()[..logical] {
        tracing::warn!(
            device = %uuid::Uuid::from_bytes(sb.device_id),
            "raw bitmap primary/mirror mismatch — using primary",
        );
    }
    Ok(primary.as_slice()[..logical].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const MB: u64 = 1024 * 1024;

    /// Create a pre-sized regular file and open it as a raw device. On
    /// a regular file the probe returns `FileBacked` (no `O_DIRECT`), so
    /// these tests exercise the format/alloc/CRC machinery without
    /// needing a real block device or O_DIRECT-capable filesystem.
    fn dev(path: &Path, size: u64) -> RawBlockDevice {
        // Pre-size the backing file (a block device would already have
        // a fixed size; we simulate that here).
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        f.set_len(size).unwrap();
        drop(f);
        RawBlockDevice::open_or_init(path).unwrap()
    }

    #[test]
    fn init_and_reopen_preserves_capacity() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let d = dev(&path, 64 * MB);
        let (used, total) = d.capacity();
        assert_eq!(used, 0);
        assert!(total > 0);
        d.sync().unwrap();
        drop(d);

        let d2 = RawBlockDevice::open_or_init(&path).unwrap();
        let (used2, total2) = d2.capacity();
        assert_eq!(used2, 0);
        assert_eq!(total2, total);
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let d = dev(&path, 64 * MB);
        let data = b"hello from the raw block device backend";
        let ext = d.alloc(data.len() as u64).unwrap();
        d.write(&ext, data).unwrap();
        assert_eq!(d.read(&ext).unwrap(), data);
    }

    #[test]
    fn data_survives_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let data = b"persistent raw data";
        let ext;
        {
            let d = dev(&path, 64 * MB);
            ext = d.alloc(data.len() as u64).unwrap();
            d.write(&ext, data).unwrap();
            d.sync().unwrap();
        }
        let d = RawBlockDevice::open_or_init(&path).unwrap();
        assert_eq!(d.read(&ext).unwrap(), data);
    }

    #[test]
    fn crc_detects_corruption() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let d = dev(&path, 64 * MB);
        let data = b"important";
        let ext = d.alloc(data.len() as u64).unwrap();
        d.write(&ext, data).unwrap();
        // Flip a payload byte directly on the backing file.
        {
            let f = OpenOptions::new().write(true).open(&path).unwrap();
            let at = d.superblock.data_offset + ext.offset + HEADER_SIZE as u64;
            f.write_all_at(&[0xFF], at).unwrap();
        }
        // Reopen so we don't read a cached buffer.
        let d2 = RawBlockDevice::open_or_init(&path).unwrap();
        assert!(matches!(d2.read(&ext), Err(BlockError::Corruption { .. })));
    }

    #[test]
    fn alloc_free_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let d = dev(&path, 64 * MB);
        let e1 = d.alloc(4096).unwrap();
        let e2 = d.alloc(4096).unwrap();
        assert!(d.capacity().0 > 0);
        d.free(&e1).unwrap();
        d.free(&e2).unwrap();
        assert_eq!(d.capacity().0, 0);
    }

    #[test]
    fn many_distinct_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let d = dev(&path, 64 * MB);
        let mut kept = Vec::new();
        for i in 0..200u32 {
            let payload = format!("raw block payload number {i} with some filler");
            let ext = d.alloc(payload.len() as u64).unwrap();
            d.write(&ext, payload.as_bytes()).unwrap();
            kept.push((ext, payload));
        }
        for (ext, expected) in &kept {
            assert_eq!(d.read(ext).unwrap(), expected.as_bytes());
        }
    }

    #[test]
    fn write_refuses_oversized_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.img");
        let d = dev(&path, 64 * MB);
        let ext = d.alloc(4096).unwrap();
        let oversize = vec![0xABu8; usize::try_from(ext.length).unwrap() + 1];
        assert!(d.write(&ext, &oversize).is_err());
    }

    #[test]
    fn rejects_too_small_device() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.img");
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(SUPERBLOCK_SIZE).unwrap(); // too small for sb + 2 bitmaps + data
        drop(f);
        assert!(RawBlockDevice::open_or_init(&path).is_err());
    }
}
