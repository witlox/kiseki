//! Certificate Revocation List (CRL) cache and verification.
//!
//! Fetches CRL from distribution points, caches it with TTL,
//! and checks certificate serial numbers against revoked list.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A cached CRL entry.
#[derive(Clone, Debug)]
pub struct CrlCache {
    /// Revoked certificate serial numbers.
    revoked_serials: HashSet<Vec<u8>>,
    /// When this CRL was last fetched.
    last_fetched: Instant,
    /// Cache TTL.
    ttl: Duration,
}

impl CrlCache {
    /// Create a new CRL cache.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            revoked_serials: HashSet::new(),
            last_fetched: Instant::now(),
            ttl,
        }
    }

    /// Update the cache with a set of revoked serial numbers.
    pub fn update(&mut self, serials: impl IntoIterator<Item = Vec<u8>>) {
        self.revoked_serials = serials.into_iter().collect();
        self.last_fetched = Instant::now();
    }

    /// Check if a certificate serial number is revoked.
    /// Returns `Err(CrlStale)` if the cache needs refresh.
    pub fn is_revoked(&self, serial: &[u8]) -> Result<bool, CrlStale> {
        if self.is_stale() {
            return Err(CrlStale);
        }
        Ok(self.revoked_serials.contains(serial))
    }

    /// Whether the cache needs a refresh.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.last_fetched.elapsed() >= self.ttl
    }

    /// Number of revoked entries.
    #[must_use]
    pub fn revoked_count(&self) -> usize {
        self.revoked_serials.len()
    }
}

/// Error returned when the CRL cache is stale and must be refreshed.
#[derive(Debug, Clone, Copy)]
pub struct CrlStale;

impl std::fmt::Display for CrlStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CRL cache is stale — refresh before checking revocation")
    }
}

impl std::error::Error for CrlStale {}

impl Default for CrlCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(3600)) // 1-hour default TTL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache() {
        let cache = CrlCache::default();
        assert!(!cache.is_revoked(b"serial-123").unwrap());
        assert_eq!(cache.revoked_count(), 0);
    }

    #[test]
    fn check_revoked() {
        let mut cache = CrlCache::default();
        cache.update(vec![b"bad-serial".to_vec(), b"also-bad".to_vec()]);

        assert!(cache.is_revoked(b"bad-serial").unwrap());
        assert!(cache.is_revoked(b"also-bad").unwrap());
        assert!(!cache.is_revoked(b"good-serial").unwrap());
        assert_eq!(cache.revoked_count(), 2);
    }

    #[test]
    fn update_replaces() {
        let mut cache = CrlCache::default();
        cache.update(vec![b"old".to_vec()]);
        assert!(cache.is_revoked(b"old").unwrap());

        cache.update(vec![b"new".to_vec()]);
        assert!(!cache.is_revoked(b"old").unwrap());
        assert!(cache.is_revoked(b"new").unwrap());
    }

    #[test]
    fn staleness() {
        let cache = CrlCache::new(Duration::from_millis(0));
        // With zero TTL, immediately stale.
        assert!(cache.is_stale());
    }

    #[test]
    fn stale_cache_rejects_queries() {
        let mut cache = CrlCache::new(Duration::from_millis(0));
        cache.update(vec![b"serial".to_vec()]);
        // Stale cache returns Err instead of a potentially outdated answer.
        assert!(cache.is_revoked(b"serial").is_err());
    }

    #[test]
    fn not_stale_within_ttl() {
        let cache = CrlCache::new(Duration::from_secs(3600));
        assert!(!cache.is_stale());
    }
}
