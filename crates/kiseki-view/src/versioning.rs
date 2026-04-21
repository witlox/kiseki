//! Object version history for views (I-K9, I-V3).
//!
//! Tracks version chains per object (composition). Each version has a
//! sequence number, timestamp, and whether it's the current version.
//! Supports historical reads (point-in-time queries).

use std::collections::HashMap;

use kiseki_common::ids::{CompositionId, SequenceNumber};

/// A single version in an object's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectVersion {
    /// Version number (monotonically increasing per object).
    pub version: u64,
    /// Log sequence when this version was created.
    pub sequence: SequenceNumber,
    /// Composition ID for this version's data.
    pub composition_id: CompositionId,
    /// Wall-clock timestamp (ms since epoch).
    pub timestamp_ms: u64,
    /// Whether this is the current (latest) version.
    pub is_current: bool,
    /// Whether this version has been deleted (tombstone).
    pub is_deleted: bool,
}

/// Version store — tracks version chains per object key.
pub struct VersionStore {
    /// Keyed by hashed object key → ordered list of versions.
    versions: HashMap<[u8; 32], Vec<ObjectVersion>>,
}

impl VersionStore {
    /// Create an empty version store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
        }
    }

    /// Record a new version for an object.
    pub fn add_version(
        &mut self,
        hashed_key: [u8; 32],
        composition_id: CompositionId,
        sequence: SequenceNumber,
        timestamp_ms: u64,
    ) -> u64 {
        let chain = self.versions.entry(hashed_key).or_default();

        // Unmark previous current version.
        for v in chain.iter_mut() {
            v.is_current = false;
        }

        let version = chain.len() as u64 + 1;
        chain.push(ObjectVersion {
            version,
            sequence,
            composition_id,
            timestamp_ms,
            is_current: true,
            is_deleted: false,
        });

        version
    }

    /// Mark an object as deleted (tombstone version).
    pub fn delete_version(
        &mut self,
        hashed_key: [u8; 32],
        sequence: SequenceNumber,
        timestamp_ms: u64,
    ) -> Option<u64> {
        let chain = self.versions.get_mut(&hashed_key)?;

        for v in chain.iter_mut() {
            v.is_current = false;
        }

        let version = chain.len() as u64 + 1;
        chain.push(ObjectVersion {
            version,
            sequence,
            composition_id: CompositionId(uuid::Uuid::nil()),
            timestamp_ms,
            is_current: true,
            is_deleted: true,
        });

        Some(version)
    }

    /// Get the current (latest non-deleted) version for an object.
    #[must_use]
    pub fn current_version(&self, hashed_key: &[u8; 32]) -> Option<&ObjectVersion> {
        self.versions
            .get(hashed_key)?
            .iter()
            .rev()
            .find(|v| v.is_current && !v.is_deleted)
    }

    /// Get a specific version by number.
    #[must_use]
    pub fn get_version(&self, hashed_key: &[u8; 32], version: u64) -> Option<&ObjectVersion> {
        self.versions
            .get(hashed_key)?
            .iter()
            .find(|v| v.version == version)
    }

    /// List all versions for an object (newest first).
    #[must_use]
    pub fn list_versions(&self, hashed_key: &[u8; 32]) -> Vec<&ObjectVersion> {
        match self.versions.get(hashed_key) {
            Some(chain) => chain.iter().rev().collect(),
            None => Vec::new(),
        }
    }

    /// Point-in-time read: get the version that was current at a given timestamp.
    /// Returns `None` if the object was deleted at that point in time.
    #[must_use]
    pub fn version_at_time(
        &self,
        hashed_key: &[u8; 32],
        timestamp_ms: u64,
    ) -> Option<&ObjectVersion> {
        let v = self
            .versions
            .get(hashed_key)?
            .iter()
            .rev()
            .find(|v| v.timestamp_ms <= timestamp_ms)?;
        // If the version at this time was a delete tombstone, return None.
        if v.is_deleted {
            None
        } else {
            Some(v)
        }
    }

    /// Total tracked objects.
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.versions.len()
    }

    /// Total versions across all objects.
    #[must_use]
    pub fn total_versions(&self) -> usize {
        self.versions.values().map(Vec::len).sum()
    }
}

impl Default for VersionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_a() -> [u8; 32] {
        [0xAA; 32]
    }

    fn comp(n: u128) -> CompositionId {
        CompositionId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn add_and_get_current() {
        let mut store = VersionStore::new();
        store.add_version(key_a(), comp(1), SequenceNumber(1), 1000);

        let v = store.current_version(&key_a()).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(v.composition_id, comp(1));
        assert!(v.is_current);
    }

    #[test]
    fn multiple_versions() {
        let mut store = VersionStore::new();
        store.add_version(key_a(), comp(1), SequenceNumber(1), 1000);
        store.add_version(key_a(), comp(2), SequenceNumber(2), 2000);
        store.add_version(key_a(), comp(3), SequenceNumber(3), 3000);

        let v = store.current_version(&key_a()).unwrap();
        assert_eq!(v.version, 3);
        assert_eq!(v.composition_id, comp(3));

        // Old versions still accessible.
        let v1 = store.get_version(&key_a(), 1).unwrap();
        assert_eq!(v1.composition_id, comp(1));
        assert!(!v1.is_current);
    }

    #[test]
    fn delete_version() {
        let mut store = VersionStore::new();
        store.add_version(key_a(), comp(1), SequenceNumber(1), 1000);
        store.delete_version(key_a(), SequenceNumber(2), 2000);

        // Current version is None (deleted).
        assert!(store.current_version(&key_a()).is_none());

        // All versions still listed.
        let versions = store.list_versions(&key_a());
        assert_eq!(versions.len(), 2);
        assert!(versions[0].is_deleted);
    }

    #[test]
    fn point_in_time_read() {
        let mut store = VersionStore::new();
        store.add_version(key_a(), comp(1), SequenceNumber(1), 1000);
        store.add_version(key_a(), comp(2), SequenceNumber(2), 2000);
        store.add_version(key_a(), comp(3), SequenceNumber(3), 3000);

        // At t=1500, version 1 was current.
        let v = store.version_at_time(&key_a(), 1500).unwrap();
        assert_eq!(v.version, 1);

        // At t=2500, version 2 was current.
        let v = store.version_at_time(&key_a(), 2500).unwrap();
        assert_eq!(v.version, 2);

        // At t=5000, version 3 is current.
        let v = store.version_at_time(&key_a(), 5000).unwrap();
        assert_eq!(v.version, 3);
    }

    #[test]
    fn nonexistent_key() {
        let store = VersionStore::new();
        assert!(store.current_version(&key_a()).is_none());
        assert!(store.list_versions(&key_a()).is_empty());
    }

    #[test]
    fn counts() {
        let mut store = VersionStore::new();
        assert_eq!(store.object_count(), 0);
        assert_eq!(store.total_versions(), 0);

        store.add_version(key_a(), comp(1), SequenceNumber(1), 1000);
        store.add_version(key_a(), comp(2), SequenceNumber(2), 2000);
        store.add_version([0xBB; 32], comp(3), SequenceNumber(3), 3000);

        assert_eq!(store.object_count(), 2);
        assert_eq!(store.total_versions(), 3);
    }
}
