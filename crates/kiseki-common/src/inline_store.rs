//! Inline object store trait (ADR-030, I-SF5).
//!
//! Abstracts small-file inline content storage. Implemented by
//! `SmallObjectStore` in `kiseki-chunk`. Used by both the Raft state
//! machine and the in-memory store in `kiseki-log` to offload inline
//! payloads on apply. The canonical key derivation
//! ([`derive_inline_key`]) lives here so both backends agree.

use std::io;

/// Trait for inline small-file content storage.
///
/// The state machine calls `put` on apply to offload inline payloads
/// from memory to persistent storage. `get` is used during snapshot
/// building. `delete` is used during GC (I-SF6).
pub trait InlineStore: Send + Sync {
    /// Store inline content keyed by 32-byte composite key
    /// (see [`derive_inline_key`]). Returns `true` if new, `false`
    /// if dedup hit.
    fn put(&self, key: &[u8; 32], data: &[u8]) -> io::Result<bool>;

    /// Retrieve inline content. Returns `None` if not found.
    fn get(&self, key: &[u8; 32]) -> io::Result<Option<Vec<u8>>>;

    /// Delete inline content. Returns `true` if entry existed.
    fn delete(&self, key: &[u8; 32]) -> io::Result<bool>;
}

/// Canonical inline-store key derivation (I-SF5).
///
/// Multiple deltas can share the same `hashed_key` (e.g. successive
/// updates to the same path). To make per-delta inline payloads
/// uniquely addressable, the last 8 bytes of the key are XOR'd with
/// the sequence number's little-endian bytes. Both the Raft state
/// machine and the in-memory `MemShardStore` MUST use this function
/// — otherwise a payload written by one path will be invisible to
/// the GC path of the other.
#[must_use]
pub fn derive_inline_key(hashed_key: &[u8; 32], sequence: u64) -> [u8; 32] {
    let mut key = *hashed_key;
    let seq_bytes = sequence.to_le_bytes();
    for (i, &b) in seq_bytes.iter().enumerate() {
        key[24 + i] ^= b;
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_keys_differ_per_sequence() {
        let h = [0xABu8; 32];
        let k1 = derive_inline_key(&h, 1);
        let k2 = derive_inline_key(&h, 2);
        assert_ne!(k1, k2, "different sequences must produce different keys");
        // First 24 bytes are still the original hashed_key prefix.
        assert_eq!(&k1[..24], &h[..24]);
        assert_eq!(&k2[..24], &h[..24]);
    }

    #[test]
    fn key_is_self_inverse_under_xor() {
        let h = [0x55u8; 32];
        let k = derive_inline_key(&h, 0xDEAD_BEEF);
        // XORing the same sequence again recovers the original hashed_key.
        let recovered = derive_inline_key(&k, 0xDEAD_BEEF);
        assert_eq!(recovered, h);
    }
}
