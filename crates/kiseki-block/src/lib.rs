//! Raw block device I/O for Kiseki (ADR-029).
//!
//! Manages data devices (`NVMe`, SSD, HDD, or file-backed for VMs/CI)
//! with auto-detection of device characteristics, bitmap-based extent
//! allocation, per-extent CRC32, and crash-safe write ordering.
//!
//! The `DeviceBackend` trait provides a uniform interface. Callers
//! never need to know whether the backend is raw block or file-backed.
//!
//! Invariant mapping:
//!   - I-C7 — block-aligned I/O (auto-detected physical block size)
//!   - I-C8 — bitmap is ground truth, journaled in redb

// Default crate-wide: forbid `unsafe`. The optional `io_uring`
// feature opens a narrow opt-in: `uring_file` contains documented
// `unsafe { ring.submission().push(...) }` calls (the kernel reads
// the buffer pointer encoded in the SQE), so when the feature is
// on we relax this to `allow` and the module itself adds
// `#![allow(unsafe_code)]` with safety comments at every `unsafe`
// site. The default-features build keeps the original `deny`.
#![cfg_attr(not(feature = "io_uring"), deny(unsafe_code))]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod allocator;
pub mod backend;
pub mod error;
pub mod extent;
pub mod file;
pub mod journal;
pub mod metrics;
pub mod probe;
pub mod superblock;
pub mod trim;

#[cfg(feature = "io_uring")]
pub mod uring_file;

pub use metrics::BlockMetrics;

pub use allocator::{BitmapAllocator, MAX_EXTENT_BYTES};
pub use backend::DeviceBackend;
pub use error::{AllocError, BlockError};
pub use extent::Extent;
pub use file::{FileBackedDevice, MAX_EXTENT_PAYLOAD_BYTES};
pub use journal::Journal;
pub use probe::{DetectedMedium, DeviceCharacteristics, IoStrategy};
pub use superblock::Superblock;
pub use trim::{TrimConfig, TrimQueue};

#[cfg(feature = "io_uring")]
pub use uring_file::UringFileBackedDevice;

use std::path::Path;
use std::sync::Arc;

/// Runtime backend selector. Reads `KISEKI_IO_URING` (any non-empty
/// non-zero value enables uring) and returns a freshly-opened device
/// boxed behind [`DeviceBackend`].
///
/// Wiring contract for `kiseki-server`:
///
/// - `KISEKI_IO_URING` unset / `0` / `false` -> always
///   [`FileBackedDevice`] (the default).
/// - `KISEKI_IO_URING=1` AND the `io_uring` Cargo feature is
///   compiled in -> [`UringFileBackedDevice`], with a fall-back to
///   [`FileBackedDevice`] if the kernel rejects the ring setup
///   (older kernels, sandboxed CI without `CONFIG_IO_URING=y`).
/// - `KISEKI_IO_URING=1` but the binary was built without
///   `--features kiseki-block/io_uring` -> warn and use
///   [`FileBackedDevice`] (operator probably mis-deployed).
///
/// The chunk-store integration (`PersistentChunkStore::init` /
/// `open`) currently hard-codes [`FileBackedDevice`] internally;
/// wiring `from_device` constructors through it is tracked as a
/// follow-up to GH #39 so this issue's scope stays in
/// `kiseki-block` + the runtime config surface.
pub fn open_or_init_device(
    path: &Path,
    size_bytes: u64,
) -> Result<Arc<dyn DeviceBackend>, BlockError> {
    let want_uring = std::env::var("KISEKI_IO_URING")
        .ok()
        .is_some_and(|v| !matches!(v.as_str(), "" | "0" | "false" | "FALSE"));

    #[cfg(feature = "io_uring")]
    if want_uring {
        let init = if path.exists() {
            UringFileBackedDevice::try_open(path)
        } else {
            UringFileBackedDevice::try_init(path, size_bytes)
        };
        match init {
            Ok(dev) => {
                tracing::info!(
                    path = %path.display(),
                    "device backend: io_uring (KISEKI_IO_URING=1)",
                );
                return Ok(Arc::new(dev));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "io_uring backend init failed; falling back to FileBackedDevice",
                );
            }
        }
    }

    #[cfg(not(feature = "io_uring"))]
    if want_uring {
        tracing::warn!(
            "KISEKI_IO_URING=1 set but binary built without `--features \
             kiseki-block/io_uring`; using FileBackedDevice"
        );
    }

    let dev = if path.exists() {
        FileBackedDevice::open(path)?
    } else {
        FileBackedDevice::init(path, size_bytes)?
    };
    Ok(Arc::new(dev))
}
