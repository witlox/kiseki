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

#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod allocator;
pub mod backend;
pub mod error;
pub mod extent;
pub mod file;
pub mod journal;
pub mod probe;
pub mod superblock;
pub mod trim;

pub use allocator::{BitmapAllocator, MAX_EXTENT_BYTES};
pub use backend::DeviceBackend;
pub use error::{AllocError, BlockError};
pub use extent::Extent;
pub use file::{FileBackedDevice, MAX_EXTENT_PAYLOAD_BYTES};
pub use journal::Journal;
pub use probe::{DetectedMedium, DeviceCharacteristics, IoStrategy};
pub use superblock::Superblock;
pub use trim::{TrimConfig, TrimQueue};
