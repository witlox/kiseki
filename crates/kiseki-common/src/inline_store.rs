//! Inline object store trait (ADR-030, I-SF5).
//!
//! Abstracts small-file inline content storage. Implemented by
//! `SmallObjectStore` in `kiseki-chunk`. Used by the Raft state
//! machine in `kiseki-log` to offload inline payloads on apply.

use std::io;

/// Trait for inline small-file content storage.
///
/// The state machine calls `put` on apply to offload inline payloads
/// from memory to persistent storage. `get` is used during snapshot
/// building. `delete` is used during GC (I-SF6).
pub trait InlineStore: Send + Sync {
    /// Store inline content keyed by chunk ID (32 bytes).
    /// Returns `true` if new, `false` if dedup hit.
    fn put(&self, key: &[u8; 32], data: &[u8]) -> io::Result<bool>;

    /// Retrieve inline content. Returns `None` if not found.
    fn get(&self, key: &[u8; 32]) -> io::Result<Option<Vec<u8>>>;

    /// Delete inline content. Returns `true` if entry existed.
    fn delete(&self, key: &[u8; 32]) -> io::Result<bool>;
}
