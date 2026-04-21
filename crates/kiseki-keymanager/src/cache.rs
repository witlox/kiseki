//! Key cache with TTL — caches tenant KEK material locally.
//!
//! When the tenant KMS is reachable, the cache is refreshed. When
//! unreachable, cached keys are used until TTL expires.
//!
//! Spec: ADR-011 (cache TTL bounds), I-K15.

use std::collections::HashMap;
use std::time::Instant;

use kiseki_common::ids::OrgId;

/// Cached tenant key entry with TTL tracking.
#[derive(Debug)]
pub struct CachedKey {
    /// Raw key material (would be Zeroizing in production).
    pub material: [u8; 32],
    /// When the key was cached.
    pub cached_at: Instant,
    /// TTL in seconds.
    pub ttl_secs: u64,
}

impl CachedKey {
    /// Check if the cached key has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed().as_secs() >= self.ttl_secs
    }
}

/// Per-tenant key cache.
pub struct KeyCache {
    entries: HashMap<OrgId, CachedKey>,
    /// Default TTL for new entries (seconds).
    pub default_ttl_secs: u64,
}

impl KeyCache {
    /// Create a new cache with the given default TTL.
    #[must_use]
    pub fn new(default_ttl_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            default_ttl_secs,
        }
    }

    /// Insert or refresh a tenant's cached key.
    pub fn insert(&mut self, tenant: OrgId, material: [u8; 32]) {
        self.entries.insert(
            tenant,
            CachedKey {
                material,
                cached_at: Instant::now(),
                ttl_secs: self.default_ttl_secs,
            },
        );
    }

    /// Look up a tenant's cached key. Returns `None` if expired or absent.
    #[must_use]
    pub fn get(&self, tenant: &OrgId) -> Option<&CachedKey> {
        self.entries.get(tenant).filter(|k| !k.is_expired())
    }

    /// Check if a tenant has a cached key (including expired).
    #[must_use]
    pub fn has_entry(&self, tenant: &OrgId) -> bool {
        self.entries.contains_key(tenant)
    }

    /// Check if a tenant's cached key is expired.
    #[must_use]
    pub fn is_expired(&self, tenant: &OrgId) -> bool {
        self.entries.get(tenant).map_or(true, CachedKey::is_expired)
    }

    /// Remove a tenant's cached key (e.g., on crypto-shred).
    pub fn remove(&mut self, tenant: &OrgId) {
        self.entries.remove(tenant);
    }

    /// Evict all expired entries.
    pub fn evict_expired(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, v| !v.is_expired());
        before - self.entries.len()
    }
}

impl Default for KeyCache {
    fn default() -> Self {
        Self::new(300) // 5 minutes default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_org() -> OrgId {
        OrgId(uuid::Uuid::from_u128(1))
    }

    #[test]
    fn insert_and_get() {
        let mut cache = KeyCache::new(300);
        cache.insert(test_org(), [0x42; 32]);
        assert!(cache.get(&test_org()).is_some());
    }

    #[test]
    fn expired_returns_none() {
        let mut cache = KeyCache::new(0); // 0-second TTL
        cache.insert(test_org(), [0x42; 32]);
        // Immediately expired.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(cache.get(&test_org()).is_none());
        assert!(cache.is_expired(&test_org()));
    }

    #[test]
    fn remove_clears_entry() {
        let mut cache = KeyCache::new(300);
        cache.insert(test_org(), [0x42; 32]);
        cache.remove(&test_org());
        assert!(!cache.has_entry(&test_org()));
    }
}
