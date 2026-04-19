//! Client-side chunk cache with bounded TTL.

use std::collections::HashMap;

use kiseki_common::ids::ChunkId;

/// Cached chunk entry.
struct CacheEntry {
    /// Decrypted plaintext chunk data.
    data: Vec<u8>,
    /// When this entry was cached (wall-clock ms).
    cached_at_ms: u64,
}

/// Client-side cache for decrypted chunk data.
pub struct ClientCache {
    entries: HashMap<ChunkId, CacheEntry>,
    /// TTL for cache entries in milliseconds.
    ttl_ms: u64,
    /// Maximum number of entries.
    max_entries: usize,
}

impl ClientCache {
    /// Create a new cache with the given TTL and max entries.
    #[must_use]
    pub fn new(ttl_ms: u64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ttl_ms,
            max_entries,
        }
    }

    /// Get a cached chunk if not expired.
    #[must_use]
    pub fn get(&self, chunk_id: &ChunkId, now_ms: u64) -> Option<&[u8]> {
        self.entries.get(chunk_id).and_then(|e| {
            if now_ms.saturating_sub(e.cached_at_ms) <= self.ttl_ms {
                Some(e.data.as_slice())
            } else {
                None
            }
        })
    }

    /// Insert a chunk into the cache.
    pub fn insert(&mut self, chunk_id: ChunkId, data: Vec<u8>, now_ms: u64) {
        // Evict if at capacity.
        if self.entries.len() >= self.max_entries {
            self.evict_oldest();
        }
        self.entries.insert(
            chunk_id,
            CacheEntry {
                data,
                cached_at_ms: now_ms,
            },
        );
    }

    /// Invalidate a specific chunk.
    pub fn invalidate(&mut self, chunk_id: &ChunkId) {
        self.entries.remove(chunk_id);
    }

    /// Evict all expired entries.
    pub fn evict_expired(&mut self, now_ms: u64) {
        let ttl = self.ttl_ms;
        self.entries
            .retain(|_, e| now_ms.saturating_sub(e.cached_at_ms) <= ttl);
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict the oldest entry.
    fn evict_oldest(&mut self) {
        if let Some(oldest_id) = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.cached_at_ms)
            .map(|(id, _)| *id)
        {
            self.entries.remove(&oldest_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_and_miss() {
        let mut cache = ClientCache::new(5000, 100);
        let id = ChunkId([0x01; 32]);
        cache.insert(id, vec![1, 2, 3], 1000);

        // Hit within TTL.
        assert_eq!(cache.get(&id, 3000), Some(&[1u8, 2, 3][..]));

        // Miss after TTL.
        assert_eq!(cache.get(&id, 7000), None);
    }

    #[test]
    fn invalidation() {
        let mut cache = ClientCache::new(5000, 100);
        let id = ChunkId([0x01; 32]);
        cache.insert(id, vec![1], 1000);
        cache.invalidate(&id);
        assert_eq!(cache.get(&id, 1000), None);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut cache = ClientCache::new(5000, 2);
        cache.insert(ChunkId([0x01; 32]), vec![1], 1000);
        cache.insert(ChunkId([0x02; 32]), vec![2], 2000);
        cache.insert(ChunkId([0x03; 32]), vec![3], 3000); // evicts oldest

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&ChunkId([0x01; 32]), 3000), None); // evicted
        assert!(cache.get(&ChunkId([0x02; 32]), 3000).is_some());
    }

    #[test]
    fn evict_expired() {
        let mut cache = ClientCache::new(1000, 100);
        cache.insert(ChunkId([0x01; 32]), vec![1], 1000);
        cache.insert(ChunkId([0x02; 32]), vec![2], 3000);

        cache.evict_expired(2500); // only [0x01] expired
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&ChunkId([0x02; 32]), 3000).is_some());
    }
}
