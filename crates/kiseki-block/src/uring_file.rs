//! io_uring-backed device — `DeviceBackend` impl driven by a per-device
//! `io_uring` submission queue (GH #39).
//!
//! Mirrors `FileBackedDevice`'s on-disk layout exactly (same
//! [`Superblock`], same bitmap, same per-extent length-header + CRC32C
//! trailer) so the existing unit tests can validate both backends and
//! the runtime can switch between them per-device without migrating
//! data. The difference is purely in how bytes move between userspace
//! and the kernel: `FileBackedDevice` calls `pread` / `pwrite` /
//! `fsync` synchronously and pays one syscall per op; this backend
//! submits the same logical ops as `IORING_OP_READ` /
//! `IORING_OP_WRITE` / `IORING_OP_FSYNC` to a per-device ring set up
//! with `IORING_SETUP_SQPOLL`, dropping the syscall on the submit
//! side once the kernel SQ-polling thread is awake.
//!
//! Trait surface is synchronous (`fn read / write / sync(&self) ->
//! Result<…>`), so each call submits a single SQE and waits on its
//! CQE before returning. The batching win promised by uring lives in
//! the per-op cost (no syscall on submit, fewer context switches on
//! completion under load) and in the future `fsync` coalescing the
//! chunk-store will get when it starts driving the ring directly —
//! one ring entry, one wait, N fsyncs.
//!
//! Buffer ownership: the caller passes `&[u8]` / `&mut [u8]`, and the
//! method blocks on the CQE before returning. That means the buffer
//! is guaranteed live for the duration of the submission — exactly
//! the safety contract `io_uring` requires for non-fixed buffers. If
//! the issue ever moves to fixed buffers / `IORING_REGISTER_BUFFERS`
//! the trait will need an `&'static` / `Bytes`-owning surface, but
//! the sync API the rest of `kiseki-block` consumes today is the
//! tightest the unsafe contract can be drawn.
//!
//! Concurrency: `io_uring::IoUring` is `Send + Sync` per the upstream
//! impl, but `SubmissionQueue` / `CompletionQueue` are not — the
//! `IoUring::submission()` / `IoUring::completion()` borrows are
//! `&mut self`. We wrap the ring in a `Mutex` so the synchronous
//! trait can serve `Send + Sync` callers; under contention the chunk-
//! store's existing per-device serialization is preserved. Per-shard
//! / per-disk parallelism comes from the runtime fanning out devices.
//!
//! Failure mode: `try_init` / `try_open` return `Err(BlockError::Io)`
//! if the kernel doesn't support uring (or the build is too old). The
//! runtime falls back to `FileBackedDevice` in that case so the
//! cluster keeps starting.
//!
//! Perf shape observed on the 2026-05-15 dev box bench (`NVMe` `ext4`,
//! `cargo bench -p kiseki-block --features io_uring`):
//!
//! - 4 KB write+fsync: roughly tied with `FileBackedDevice` (both
//!   bound by `ext4` journal commit, not syscall cost — the
//!   ≥ 20 % gate spelled out in GH #39 is NOT met on this shape).
//! - 64 KB write+fsync: roughly 2× win in steady-state (5–6 ms uring
//!   vs 7–15 ms file, high variance on dev box).
//! - read at both shapes: regresses against the cached-`pread` tight
//!   loop the bench harness drives (page cache hits avoid any
//!   syscall after the first iteration; uring re-enters the kernel
//!   each iteration).
//!
//! The runtime selection in `kiseki-server` keeps this backend opt-in
//! per-device for that reason — the 64 KB+ shape is what matters for
//! the chunk-fsync hot path, and the small-block read regression is
//! a measurement-shape artifact in a tight-loop bench, not a
//! production concern (the chunk-store re-reads the same extent
//! essentially never).

#![allow(unsafe_code)] // io_uring submission is inherently unsafe — see safety notes at each `unsafe` block.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use io_uring::{opcode, types, IoUring};

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

/// Submission queue depth. 32 entries is enough for the worst-case
/// `write_chunk` shape (header + payload + CRC submitted as one big
/// `IORING_OP_WRITE`) plus a few in-flight syncs without backpressure.
/// Bump if the chunk store ever submits multi-fsync batches.
const RING_ENTRIES: u32 = 32;

/// Idle ms before the kernel SQ-polling thread parks. 1 ms keeps it
/// responsive without burning a CPU continuously when idle. The
/// chunk-store reaches the ring multiple times per ms under
/// fabric-write load, so the thread stays warm in practice.
const SQPOLL_IDLE_MS: u32 = 1;

/// Maximum payload bytes (ciphertext) that fit in a single extent
/// after subtracting the per-extent header + CRC trailer overhead.
/// Same value as [`crate::file::MAX_EXTENT_PAYLOAD_BYTES`] — kept as a
/// crate-internal `const` so the two backends stay in lock-step.
pub const MAX_EXTENT_PAYLOAD_BYTES: u64 = MAX_EXTENT_BYTES - OVERHEAD as u64;

/// io_uring-backed device — uses a sparse file on the host filesystem
/// driven through a per-device `io_uring` for the hot read / write /
/// fsync path. Bitmap, superblock, and on-disk extent format are
/// identical to [`crate::file::FileBackedDevice`] so the runtime can
/// switch backends per-device without migrating data.
pub struct UringFileBackedDevice {
    _path: PathBuf,
    file: File,
    ring: Mutex<IoUring>,
    superblock: Superblock,
    allocator: Mutex<BitmapAllocator>,
    characteristics: DeviceCharacteristics,
}

impl UringFileBackedDevice {
    /// Try to initialize a new uring-backed device at `path`.
    ///
    /// Returns `Err(BlockError::Io)` if the kernel doesn't support
    /// `IoUring::new` (too old, or no `CONFIG_IO_URING=y`). Callers
    /// should fall back to [`crate::file::FileBackedDevice::init`].
    ///
    /// Sets up `IORING_SETUP_SQPOLL` so subsequent submissions don't
    /// pay the syscall on the submit side once the kernel polling
    /// thread is awake.
    pub fn try_init(path: &Path, size_bytes: u64) -> Result<Self, BlockError> {
        let chars = DeviceCharacteristics::file_backed_defaults();
        let sb = Superblock::new(size_bytes, chars.physical_block_size);

        if path.exists() {
            let mut f = File::open(path)?;
            let mut buf = vec![0u8; 4096];
            if f.read(&mut buf)? >= 8 && buf[..8] == crate::superblock::MAGIC {
                return Err(BlockError::AlreadyInitialized);
            }
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        file.set_len(size_bytes)?;

        // Superblock + bitmaps via plain blocking writes — the init
        // path runs once at device creation and the synchronous
        // codepath here keeps the file layout identical to
        // FileBackedDevice for cross-backend portability.
        {
            let mut f = &file;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&sb.to_bytes())?;
        }

        #[allow(clippy::cast_possible_truncation)]
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
        let ring = new_ring()?;

        Ok(Self {
            _path: path.to_owned(),
            file,
            ring: Mutex::new(ring),
            superblock: sb,
            allocator: Mutex::new(allocator),
            characteristics: chars,
        })
    }

    /// Try to open an existing uring-backed device.
    ///
    /// Same fallback semantics as [`Self::try_init`].
    pub fn try_open(path: &Path) -> Result<Self, BlockError> {
        if !path.exists() {
            return Err(BlockError::NotInitialized);
        }
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;

        let mut sb_buf = vec![0u8; 4096];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut sb_buf)?;
        let sb = Superblock::from_bytes(&sb_buf)?;

        let bitmap_size_u64 = sb.total_blocks.div_ceil(8);
        assert!(
            usize::try_from(bitmap_size_u64).is_ok(),
            "bitmap too large for this platform"
        );
        #[allow(clippy::cast_possible_truncation)]
        let bitmap_size = bitmap_size_u64 as usize;
        let mut bitmap = vec![0u8; bitmap_size];
        file.seek(SeekFrom::Start(sb.bitmap_offset))?;
        file.read_exact(&mut bitmap)?;

        let mut mirror = vec![0u8; bitmap_size];
        file.seek(SeekFrom::Start(sb.bitmap_mirror_offset))?;
        file.read_exact(&mut mirror)?;
        if bitmap != mirror {
            tracing::warn!("uring: bitmap primary/mirror mismatch detected, using primary");
        }

        let allocator = BitmapAllocator::from_bitmap(bitmap, sb.total_blocks, sb.block_size);
        let chars = DeviceCharacteristics::file_backed_defaults();
        let ring = new_ring()?;

        Ok(Self {
            _path: path.to_owned(),
            file,
            ring: Mutex::new(ring),
            superblock: sb,
            allocator: Mutex::new(allocator),
            characteristics: chars,
        })
    }

    /// Flush the bitmap to both primary and mirror regions on the file.
    /// Uses plain blocking writes — this is a cold path called from
    /// `sync()` once per group-commit window, not from the per-write
    /// hot path the `io_uring` submission targets.
    fn flush_bitmap(&self) -> Result<(), BlockError> {
        let alloc = self.allocator.lock().lock_or_die("uring.allocator");
        let bitmap = alloc.bitmap_bytes();
        let mut f = &self.file;
        f.seek(SeekFrom::Start(self.superblock.bitmap_offset))?;
        f.write_all(bitmap)?;
        f.seek(SeekFrom::Start(self.superblock.bitmap_mirror_offset))?;
        f.write_all(bitmap)?;
        Ok(())
    }

    /// Submit a single SQE and wait for its CQE. The user-data tag is
    /// only meaningful for multi-op batches; for the per-op
    /// synchronous shape we just take the next CQE off the queue.
    ///
    /// Returns the CQE result (negative = `-errno`, non-negative =
    /// op-specific count, e.g. bytes transferred for read/write).
    ///
    /// # Safety contract
    ///
    /// The caller MUST ensure any buffer pointer encoded in `entry`
    /// outlives the call — `submit_and_wait(1)` blocks until the CQE
    /// is available, so the buffer is guaranteed live as long as the
    /// caller passes a borrow whose lifetime spans this method.
    fn submit_wait(&self, entry: &io_uring::squeue::Entry) -> Result<i32, BlockError> {
        let mut ring = self.ring.lock().lock_or_die("uring.ring");

        // SAFETY: the only pointer in `entry` is the buffer pointer
        // we encoded in the calling op-builder above; the buffer
        // lives in the caller's stack / heap and outlives this
        // function because we `submit_and_wait(1)` before returning,
        // and the lock keeps the ring exclusive while the CQE is
        // pending.
        unsafe {
            ring.submission().push(entry).map_err(|e| {
                BlockError::Io(std::io::Error::other(format!(
                    "io_uring submission push failed: {e}"
                )))
            })?;
        }

        ring.submit_and_wait(1).map_err(BlockError::Io)?;

        let cqe = ring.completion().next().ok_or_else(|| {
            BlockError::Io(std::io::Error::other(
                "io_uring completion queue empty after submit_and_wait(1)",
            ))
        })?;

        Ok(cqe.result())
    }

    /// Write the given byte slice at `abs_offset` via `IORING_OP_WRITE`.
    fn pwrite_uring(&self, abs_offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let len = u32::try_from(buf.len()).map_err(|_| {
            BlockError::Io(std::io::Error::other("io_uring write: payload exceeds u32"))
        })?;
        let fd = types::Fd(self.file.as_raw_fd());
        let entry = opcode::Write::new(fd, buf.as_ptr(), len)
            .offset(abs_offset)
            .build()
            .user_data(0);
        let res = self.submit_wait(&entry)?;
        let transferred = cqe_result_as_usize(res, "io_uring write")?;
        if transferred != buf.len() {
            return Err(BlockError::Io(std::io::Error::other(format!(
                "io_uring short write: requested {} got {}",
                buf.len(),
                transferred
            ))));
        }
        Ok(())
    }

    /// Read into the given mutable slice from `abs_offset` via `IORING_OP_READ`.
    fn pread_uring(&self, abs_offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let len = u32::try_from(buf.len()).map_err(|_| {
            BlockError::Io(std::io::Error::other("io_uring read: length exceeds u32"))
        })?;
        let fd = types::Fd(self.file.as_raw_fd());
        let entry = opcode::Read::new(fd, buf.as_mut_ptr(), len)
            .offset(abs_offset)
            .build()
            .user_data(0);
        let res = self.submit_wait(&entry)?;
        let transferred = cqe_result_as_usize(res, "io_uring read")?;
        if transferred != buf.len() {
            return Err(BlockError::Io(std::io::Error::other(format!(
                "io_uring short read: requested {} got {}",
                buf.len(),
                transferred
            ))));
        }
        Ok(())
    }

    /// Submit an `IORING_OP_FSYNC` and wait for its CQE.
    fn fsync_uring(&self) -> Result<(), BlockError> {
        let fd = types::Fd(self.file.as_raw_fd());
        let entry = opcode::Fsync::new(fd).build().user_data(0);
        let res = self.submit_wait(&entry)?;
        if res < 0 {
            return Err(BlockError::Io(std::io::Error::from_raw_os_error(-res)));
        }
        Ok(())
    }
}

/// Coerce an `i32` CQE result (already checked to be non-negative
/// when the negative branch returns an error) into a `usize` byte
/// count. Wrapping the cast in `u32::try_from(res).map_err(...)`
/// would lose the `result` value for the error message, so we go
/// through the lossless `i32 -> u32 -> usize` chain explicitly. A
/// negative `res` is rejected up front so the cast never sees a
/// sign-bit-set value.
fn cqe_result_as_usize(res: i32, op_label: &str) -> Result<usize, BlockError> {
    if res < 0 {
        return Err(BlockError::Io(std::io::Error::from_raw_os_error(-res)));
    }
    // res >= 0 here, so `as u32` is the safe widening cast.
    #[allow(clippy::cast_sign_loss)]
    let unsigned = res as u32;
    usize::try_from(unsigned).map_err(|_| {
        BlockError::Io(std::io::Error::other(format!(
            "{op_label}: CQE result {res} exceeds usize on this platform",
        )))
    })
}

/// Construct a new per-device ring with `IORING_SETUP_SQPOLL`. Falls
/// back to a non-SQPOLL ring if SQPOLL setup fails (e.g. `CAP_SYS_NICE`
/// missing on older kernels), so unprivileged tests can still drive
/// the backend without losing the rest of the win.
fn new_ring() -> Result<IoUring, BlockError> {
    match IoUring::builder()
        .setup_sqpoll(SQPOLL_IDLE_MS)
        .build(RING_ENTRIES)
    {
        Ok(r) => Ok(r),
        Err(sqpoll_err) => {
            tracing::info!(
                error = %sqpoll_err,
                "io_uring SQPOLL setup failed; falling back to non-SQPOLL ring",
            );
            IoUring::new(RING_ENTRIES).map_err(BlockError::Io)
        }
    }
}

impl DeviceBackend for UringFileBackedDevice {
    fn alloc(&self, size: u64) -> Result<Extent, AllocError> {
        let total = size + OVERHEAD as u64;
        let mut alloc = self.allocator.lock().lock_or_die("uring.allocator");
        alloc.alloc(total)
    }

    #[tracing::instrument(skip(self, data), fields(offset = extent.offset, length = extent.length, bytes = data.len()))]
    fn write(&self, extent: &Extent, data: &[u8]) -> Result<(), BlockError> {
        if data.len() > u32::MAX as usize {
            tracing::warn!(bytes = data.len(), "uring write: data exceeds 4 GiB");
            return Err(BlockError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "data exceeds 4GB",
            )));
        }
        let payload_capacity = extent.length.saturating_sub(OVERHEAD as u64);
        if data.len() as u64 > payload_capacity {
            tracing::warn!(
                bytes = data.len(),
                extent_length = extent.length,
                payload_capacity,
                "uring write: data exceeds extent payload capacity",
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
        #[allow(clippy::cast_possible_truncation)]
        let data_len = data.len() as u32;

        // Coalesce header + payload + CRC into one buffer so the
        // whole extent is one `IORING_OP_WRITE`, not three. The
        // chunk-store amortizes this allocation across the existing
        // `bytes::Bytes` pool one level up; here we just need a
        // contiguous slice the kernel can read from while we wait
        // for the CQE.
        let mut buf = Vec::with_capacity(HEADER_SIZE + data.len() + CRC_SIZE);
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.extend_from_slice(data);
        buf.extend_from_slice(&crc.to_le_bytes());

        self.pwrite_uring(abs_offset, &buf).inspect_err(|e| {
            tracing::warn!(error = %e, "uring write: pwrite failed");
        })
    }

    #[tracing::instrument(skip(self), fields(offset = extent.offset, length = extent.length))]
    fn read(&self, extent: &Extent) -> Result<Vec<u8>, BlockError> {
        let abs_offset = self.superblock.data_offset + extent.offset;

        // One SQE for the whole extent (header + payload + CRC). The
        // initial implementation split this into a 4-byte header
        // read followed by a payload+CRC read — two ring round-trips
        // per `read`. On the 2026-05-15 dev box bench that pessimized
        // the small-block path against `FileBackedDevice`'s single
        // buffered `pread` (3.7 us vs 13.8 us at 4 KB). One SQE
        // matches FileBackedDevice's one syscall and lets the kernel
        // do its single contiguous read.
        let extent_len = usize::try_from(extent.length).map_err(|_| {
            BlockError::Io(std::io::Error::other(
                "uring read: extent length exceeds usize",
            ))
        })?;
        let mut buf = vec![0u8; extent_len];
        self.pread_uring(abs_offset, &mut buf).inspect_err(|e| {
            tracing::warn!(error = %e, "uring read: extent read failed");
        })?;

        // Parse header.
        let mut len_arr = [0u8; HEADER_SIZE];
        len_arr.copy_from_slice(&buf[..HEADER_SIZE]);
        let data_len = u32::from_le_bytes(len_arr) as usize;

        let payload_capacity = extent.length.saturating_sub(OVERHEAD as u64);
        if data_len as u64 > payload_capacity {
            tracing::warn!(
                offset = extent.offset,
                extent_length = extent.length,
                claimed_len = data_len,
                payload_capacity,
                "uring read: header claims length beyond extent — corruption",
            );
            return Err(BlockError::Corruption {
                offset: extent.offset,
                expected: 0,
                actual: 0,
            });
        }

        // Slice payload + CRC trailer out of the buffer.
        let payload_end = HEADER_SIZE + data_len;
        let crc_end = payload_end + CRC_SIZE;
        let mut crc_arr = [0u8; CRC_SIZE];
        crc_arr.copy_from_slice(&buf[payload_end..crc_end]);
        let stored_crc = u32::from_le_bytes(crc_arr);

        // Drop bytes outside the actual payload range. `buf` already
        // owns the allocation; truncating + draining the header is
        // cheaper than `to_vec` on the slice for the bigger shapes.
        let mut payload = buf;
        payload.truncate(payload_end);
        payload.drain(..HEADER_SIZE);

        let computed_crc = crc32c(&payload);
        if stored_crc != computed_crc {
            tracing::warn!(
                offset = extent.offset,
                expected = stored_crc,
                actual = computed_crc,
                "uring read: CRC mismatch — corruption",
            );
            return Err(BlockError::Corruption {
                offset: extent.offset,
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        Ok(payload)
    }

    fn free(&self, extent: &Extent) -> Result<(), AllocError> {
        let mut alloc = self.allocator.lock().lock_or_die("uring.allocator");
        alloc.free(extent)
    }

    fn sync(&self) -> Result<(), BlockError> {
        self.flush_bitmap()?;
        self.fsync_uring()
    }

    fn capacity(&self) -> (u64, u64) {
        let alloc = self.allocator.lock().lock_or_die("uring.allocator");
        (alloc.used_bytes(), alloc.total_bytes())
    }

    fn characteristics(&self) -> &DeviceCharacteristics {
        &self.characteristics
    }

    fn device_id(&self) -> [u8; 16] {
        self.superblock.device_id
    }

    fn bitmap_bytes(&self) -> Vec<u8> {
        let alloc = self.allocator.lock().lock_or_die("uring.allocator");
        alloc.bitmap_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::FileBackedDevice;
    use tempfile::tempdir;

    const MB: u64 = 1024 * 1024;

    /// Skip helper: if the host can't construct an `IoUring` (no
    /// kernel support, CI sandbox blocking the syscall, etc.),
    /// return false so individual tests can early-exit with an
    /// informative log instead of failing the whole crate's test
    /// run.
    fn uring_supported() -> bool {
        IoUring::new(8).is_ok()
    }

    #[test]
    fn init_and_open() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();
        let (used, total) = dev.capacity();
        assert_eq!(used, 0);
        assert!(total > 0);
        dev.sync().unwrap();

        let dev2 = UringFileBackedDevice::try_open(&path).unwrap();
        let (used2, total2) = dev2.capacity();
        assert_eq!(used2, 0);
        assert_eq!(total2, total);
    }

    #[test]
    fn write_read_roundtrip() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();

        let data = b"hello, kiseki io_uring block device!";
        let extent = dev.alloc(data.len() as u64).unwrap();
        dev.write(&extent, data).unwrap();

        let read_back = dev.read(&extent).unwrap();
        assert_eq!(&read_back, data);
    }

    #[test]
    fn data_survives_reopen() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        let data = b"persistent uring block data";
        let extent;

        {
            let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();
            extent = dev.alloc(data.len() as u64).unwrap();
            dev.write(&extent, data).unwrap();
            dev.sync().unwrap();
        }

        {
            let dev = UringFileBackedDevice::try_open(&path).unwrap();
            let read_back = dev.read(&extent).unwrap();
            assert_eq!(&read_back, data);
        }
    }

    /// The on-disk layout is identical to `FileBackedDevice`, so a
    /// file written by one MUST be readable by the other. This
    /// catches silent layout drift if either backend is changed
    /// independently.
    #[test]
    fn cross_backend_layout_compat_uring_writes_file_reads() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        let data = b"layout-compat probe";
        let extent;
        {
            let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();
            extent = dev.alloc(data.len() as u64).unwrap();
            dev.write(&extent, data).unwrap();
            dev.sync().unwrap();
        }
        {
            let dev = FileBackedDevice::open(&path).unwrap();
            let read_back = dev.read(&extent).unwrap();
            assert_eq!(&read_back, data);
        }
    }

    #[test]
    fn cross_backend_layout_compat_file_writes_uring_reads() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");

        let data = b"reverse layout-compat probe";
        let extent;
        {
            let dev = FileBackedDevice::init(&path, 64 * MB).unwrap();
            extent = dev.alloc(data.len() as u64).unwrap();
            dev.write(&extent, data).unwrap();
            dev.sync().unwrap();
        }
        {
            let dev = UringFileBackedDevice::try_open(&path).unwrap();
            let read_back = dev.read(&extent).unwrap();
            assert_eq!(&read_back, data);
        }
    }

    #[test]
    fn crc32_detects_corruption() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();

        let data = b"important uring data";
        let extent = dev.alloc(data.len() as u64).unwrap();
        dev.write(&extent, data).unwrap();
        dev.sync().unwrap();

        // Corrupt one byte in the payload region (skip the 4-byte header).
        {
            let abs_offset = dev.superblock.data_offset + extent.offset + HEADER_SIZE as u64;
            let mut f = OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(abs_offset)).unwrap();
            f.write_all(&[0xFF]).unwrap();
            f.sync_all().unwrap();
        }

        // Reopen so the inner file handle picks up the on-disk byte.
        let dev2 = UringFileBackedDevice::try_open(&path).unwrap();
        let result = dev2.read(&extent);
        assert!(matches!(result, Err(BlockError::Corruption { .. })));
    }

    #[test]
    fn write_refuses_data_larger_than_extent() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();

        let extent = dev.alloc(4096).unwrap();
        let oversize: Vec<u8> = vec![0xAB; usize::try_from(extent.length).unwrap() + 1];
        let result = dev.write(&extent, &oversize);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_writes_and_reads() {
        if !uring_supported() {
            eprintln!("io_uring not supported on this host; skipping");
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.dev");
        let dev = UringFileBackedDevice::try_init(&path, 64 * MB).unwrap();

        let mut extents = Vec::new();
        for i in 0..100u32 {
            let data = format!("uring block data {i}");
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
