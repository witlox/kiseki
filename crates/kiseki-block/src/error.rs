//! Error types for block device operations.

use thiserror::Error;

/// Block device I/O errors.
#[derive(Debug, Error)]
pub enum BlockError {
    /// I/O operation failed.
    #[error("block I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Data corruption detected (CRC32 mismatch).
    #[error(
        "data corruption at offset {offset}: expected CRC {expected:#010x}, got {actual:#010x}"
    )]
    Corruption {
        /// Byte offset on device.
        offset: u64,
        /// Expected CRC32 value.
        expected: u32,
        /// Actual CRC32 value.
        actual: u32,
    },

    /// Superblock validation failed.
    #[error("invalid superblock: {0}")]
    InvalidSuperblock(String),

    /// Device is not initialized.
    #[error("device not initialized (no Kiseki superblock found)")]
    NotInitialized,

    /// Device already initialized (use --force to reinitialize).
    #[error("device already initialized with Kiseki superblock")]
    AlreadyInitialized,

    /// Existing filesystem detected on device.
    #[error("existing filesystem detected: {0}")]
    ExistingFilesystem(String),
}

/// Allocation errors.
#[derive(Debug, Error)]
pub enum AllocError {
    /// Device is full — no extent large enough.
    #[error("device full: requested {requested} bytes, largest free extent is {available} bytes")]
    DeviceFull {
        /// Requested allocation size.
        requested: u64,
        /// Largest available contiguous extent.
        available: u64,
    },

    /// Internal allocator inconsistency.
    #[error("allocator inconsistency: {0}")]
    Inconsistency(String),
}
