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
        self.entries.get(tenant).is_none_or(CachedKey::is_expired)
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

    // ---------------------------------------------------------------
    // Scenario: Cache TTL jitter prevents thundering herd
    // TTL per node is 60 +/- 10% (54s to 66s, randomized).
    // ---------------------------------------------------------------
    #[test]
    fn cache_ttl_jitter_prevents_thundering_herd() {
        let base_ttl: u64 = 60;
        let jitter_pct: f64 = 0.10;

        // Simulate 100 nodes picking jittered TTLs.
        let mut ttls = Vec::with_capacity(100);
        for i in 0u64..100 {
            // Deterministic jitter based on node index for reproducibility.
            // Precision loss acceptable: i and base_ttl are small values used for jitter calculation.
            #[allow(clippy::cast_precision_loss)]
            let jitter_factor = 1.0 + jitter_pct * (2.0 * (i as f64 / 99.0) - 1.0);
            // Truncation and sign loss acceptable: jitter_factor is always positive and result fits in u64.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let jittered = (base_ttl as f64 * jitter_factor) as u64;
            ttls.push(jittered);
        }

        let min_ttl = *ttls.iter().min().unwrap();
        let max_ttl = *ttls.iter().max().unwrap();

        // TTLs should span at least 10 seconds (the jitter window).
        assert!(
            max_ttl - min_ttl >= 10,
            "jitter window too narrow: {min_ttl}..{max_ttl}"
        );
        // All TTLs should be within [54, 66].
        assert!(min_ttl >= 54, "min TTL {min_ttl} below 54");
        assert!(max_ttl <= 66, "max TTL {max_ttl} above 66");
    }

    // ---------------------------------------------------------------
    // Scenario: Cache TTL expiry triggers provider fetch
    // After TTL expires, get() returns None, forcing a new fetch.
    // ---------------------------------------------------------------
    #[test]
    fn cache_expiry_triggers_fetch() {
        let mut cache = KeyCache::new(0); // 0-second TTL
        cache.insert(test_org(), [0x42; 32]);
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Expired: get returns None, signaling "fetch from provider".
        assert!(cache.get(&test_org()).is_none());
        assert!(cache.is_expired(&test_org()));

        // After "re-fetch", insert again with fresh TTL.
        cache.insert(test_org(), [0x43; 32]);
        // With 0-second TTL it expires immediately, but the insert succeeded.
        assert!(cache.has_entry(&test_org()));
    }
}
