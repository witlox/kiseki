//! In-memory chunk store — reference implementation of [`ChunkOps`].

use std::collections::{HashMap, HashSet};

use kiseki_common::ids::ChunkId;
use kiseki_crypto::envelope::Envelope;

use crate::error::ChunkError;
use crate::pool::AffinityPool;

/// Stored chunk entry.
struct ChunkEntry {
    envelope: Envelope,
    refcount: u64,
    retention_holds: HashSet<String>,
    pool: String,
}

/// Chunk storage operations trait.
pub trait ChunkOps {
    /// Write a chunk. If the chunk ID already exists (dedup hit),
    /// increments the refcount instead of writing new data (I-C1, I-C2).
    fn write_chunk(&mut self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError>;

    /// Read a chunk by ID.
    fn read_chunk(&self, chunk_id: &ChunkId) -> Result<&Envelope, ChunkError>;

    /// Increment refcount for an existing chunk (dedup).
    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;

    /// Decrement refcount. Returns the new refcount.
    fn decrement_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;

    /// Set a retention hold on a chunk (I-C2b).
    fn set_retention_hold(&mut self, chunk_id: &ChunkId, hold_name: &str)
        -> Result<(), ChunkError>;

    /// Release a retention hold.
    fn release_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError>;

    /// Run GC: delete chunks with refcount=0 and no retention holds.
    /// Returns the number of chunks deleted.
    fn gc(&mut self) -> u64;

    /// Get the refcount for a chunk.
    fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;
}

/// In-memory chunk store.
pub struct ChunkStore {
    chunks: HashMap<ChunkId, ChunkEntry>,
    pools: HashMap<String, AffinityPool>,
}

impl ChunkStore {
    /// Create an empty chunk store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            pools: HashMap::new(),
        }
    }

    /// Add an affinity pool.
    pub fn add_pool(&mut self, pool: AffinityPool) {
        self.pools.insert(pool.name.clone(), pool);
    }

    /// Total number of stored chunks.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

impl Default for ChunkStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkOps for ChunkStore {
    fn write_chunk(&mut self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError> {
        let chunk_id = envelope.chunk_id;

        // Dedup: if chunk already exists, just bump refcount.
        if let Some(entry) = self.chunks.get_mut(&chunk_id) {
            entry.refcount += 1;
            return Ok(false); // not a new write
        }

        // Check pool capacity.
        if let Some(p) = self.pools.get_mut(pool) {
            let size = envelope.ciphertext.len() as u64;
            if !p.has_capacity(size) {
                return Err(ChunkError::PoolFull(pool.to_owned()));
            }
            p.used_bytes += size;
        }

        self.chunks.insert(
            chunk_id,
            ChunkEntry {
                envelope,
                refcount: 1,
                retention_holds: HashSet::new(),
                pool: pool.to_owned(),
            },
        );

        Ok(true) // new write
    }

    fn read_chunk(&self, chunk_id: &ChunkId) -> Result<&Envelope, ChunkError> {
        self.chunks
            .get(chunk_id)
            .map(|e| &e.envelope)
            .ok_or(ChunkError::NotFound(*chunk_id))
    }

    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.refcount += 1;
        Ok(entry.refcount)
    }

    fn decrement_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        if entry.refcount == 0 {
            return Err(ChunkError::RefcountUnderflow(*chunk_id));
        }
        entry.refcount -= 1;
        Ok(entry.refcount)
    }

    fn set_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.retention_holds.insert(hold_name.to_owned());
        Ok(())
    }

    fn release_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.retention_holds.remove(hold_name);
        Ok(())
    }

    fn gc(&mut self) -> u64 {
        let to_remove: Vec<ChunkId> = self
            .chunks
            .iter()
            .filter(|(_, e)| e.refcount == 0 && e.retention_holds.is_empty())
            .map(|(id, _)| *id)
            .collect();

        let count = to_remove.len() as u64;

        for id in &to_remove {
            if let Some(entry) = self.chunks.remove(id) {
                if let Some(pool) = self.pools.get_mut(&entry.pool) {
                    pool.used_bytes = pool
                        .used_bytes
                        .saturating_sub(entry.envelope.ciphertext.len() as u64);
                }
            }
        }

        count
    }

    fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        self.chunks
            .get(chunk_id)
            .map(|e| e.refcount)
            .ok_or(ChunkError::NotFound(*chunk_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::DurabilityStrategy;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_crypto::aead::{GCM_NONCE_LEN, GCM_TAG_LEN};

    fn test_envelope(chunk_id_byte: u8) -> Envelope {
        Envelope {
            ciphertext: vec![0xab; 256],
            auth_tag: [0xcc; GCM_TAG_LEN],
            nonce: [0xdd; GCM_NONCE_LEN],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([chunk_id_byte; 32]),
        }
    }

    fn setup_store() -> ChunkStore {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool::new(
            "fast-nvme",
            DurabilityStrategy::default(),
            1024 * 1024 * 1024,
        ));
        store
    }

    #[test]
    fn write_and_read_roundtrip() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        let is_new = store.write_chunk(env, "fast-nvme");
        assert!(is_new.is_ok());
        assert!(is_new.unwrap_or_else(|_| unreachable!()));

        let read = store.read_chunk(&chunk_id);
        assert!(read.is_ok());
        assert_eq!(read.unwrap_or_else(|_| unreachable!()).chunk_id, chunk_id);
    }

    #[test]
    fn dedup_increments_refcount() {
        let mut store = setup_store();
        let env1 = test_envelope(0x01);
        let env2 = test_envelope(0x01); // same chunk ID
        let chunk_id = env1.chunk_id;

        store
            .write_chunk(env1, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        let is_new = store
            .write_chunk(env2, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        assert!(!is_new); // dedup hit

        assert_eq!(
            store.refcount(&chunk_id).unwrap_or_else(|_| unreachable!()),
            2
        );
    }

    #[test]
    fn gc_respects_refcount() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        store
            .write_chunk(env, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());

        // Refcount = 1, should NOT be GC'd.
        assert_eq!(store.gc(), 0);

        // Decrement to 0.
        store
            .decrement_refcount(&chunk_id)
            .unwrap_or_else(|_| unreachable!());

        // Now GC should remove it.
        assert_eq!(store.gc(), 1);
        assert_eq!(store.chunk_count(), 0);
    }

    #[test]
    fn retention_hold_blocks_gc() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        store
            .write_chunk(env, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        store
            .decrement_refcount(&chunk_id)
            .unwrap_or_else(|_| unreachable!());
        store
            .set_retention_hold(&chunk_id, "hipaa-7yr")
            .unwrap_or_else(|_| unreachable!());

        // Refcount = 0 but hold active → NOT GC'd (I-C2b).
        assert_eq!(store.gc(), 0);

        // Release hold → now GC works.
        store
            .release_retention_hold(&chunk_id, "hipaa-7yr")
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.gc(), 1);
    }

    #[test]
    fn refcount_underflow_rejected() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        store
            .write_chunk(env, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        store
            .decrement_refcount(&chunk_id)
            .unwrap_or_else(|_| unreachable!());

        // Second decrement: refcount is 0 → underflow error.
        let result = store.decrement_refcount(&chunk_id);
        assert!(result.is_err());
    }

    #[test]
    fn chunk_not_found() {
        let store = setup_store();
        let result = store.read_chunk(&ChunkId([0xff; 32]));
        assert!(result.is_err());
    }
}
