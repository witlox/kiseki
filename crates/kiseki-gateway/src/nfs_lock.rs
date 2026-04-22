//! `NFSv4` lock state machine.
//!
//! Tracks byte-range locks per file handle. `NFSv4` requires stateful
//! lock management with lock holders, byte ranges, and lease-based
//! expiry. Used by `nfs4_server` for LOCK, LOCKU, LOCKT operations.
//!
//! Lock types: Read (shared), Write (exclusive). Read locks allow
//! concurrent readers. Write locks are exclusive — no other locks
//! (read or write) may overlap.

use std::collections::HashMap;
use std::sync::Mutex;

/// Lock type per `NFSv4` spec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockType {
    /// Shared read lock — multiple readers allowed.
    Read,
    /// Exclusive write lock — no other locks may overlap.
    Write,
}

/// A byte-range lock on a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteRangeLock {
    /// Lock owner identifier (client-assigned, opaque).
    pub owner: String,
    /// Lock type.
    pub lock_type: LockType,
    /// Start offset (inclusive).
    pub offset: u64,
    /// Length of locked range (0 = to end of file, per NFS spec).
    pub length: u64,
    /// Wall-clock time (ms) when the lock was acquired.
    pub acquired_at_ms: u64,
    /// Lease duration (ms). Lock expires after this duration without renewal.
    pub lease_ms: u64,
}

impl ByteRangeLock {
    /// Check whether this lock has expired.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.acquired_at_ms) > self.lease_ms
    }

    /// Check whether two byte ranges overlap.
    #[must_use]
    pub fn overlaps(&self, offset: u64, length: u64) -> bool {
        let self_end = if self.length == 0 {
            u64::MAX
        } else {
            self.offset.saturating_add(self.length)
        };
        let other_end = if length == 0 {
            u64::MAX
        } else {
            offset.saturating_add(length)
        };
        self.offset < other_end && offset < self_end
    }
}

/// Error from lock operations.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Conflicting lock exists.
    #[error("lock denied: conflicting {0:?} lock held by {1}")]
    Denied(LockType, String),
    /// Lock not found (for unlock).
    #[error("lock not found for owner {0}")]
    NotFound(String),
    /// Lock expired before operation completed.
    #[error("lock expired")]
    Expired,
}

/// Per-file lock state.
#[derive(Debug, Default)]
struct FileLockState {
    locks: Vec<ByteRangeLock>,
}

impl FileLockState {
    /// Try to acquire a lock. Returns `Ok(())` if granted, `Err` if conflicting.
    fn try_lock(&mut self, lock: ByteRangeLock, now_ms: u64) -> Result<(), LockError> {
        // Expire stale locks first.
        self.locks.retain(|l| !l.is_expired(now_ms));

        // Check for conflicts.
        for existing in &self.locks {
            if !existing.overlaps(lock.offset, lock.length) {
                continue;
            }
            // Same owner can upgrade/re-lock.
            if existing.owner == lock.owner {
                continue;
            }
            // Read + Read is allowed.
            if existing.lock_type == LockType::Read && lock.lock_type == LockType::Read {
                continue;
            }
            // Any other combination conflicts.
            return Err(LockError::Denied(
                existing.lock_type,
                existing.owner.clone(),
            ));
        }

        // Remove any existing lock from the same owner on the same range
        // (re-lock / upgrade).
        self.locks.retain(|l| {
            !(l.owner == lock.owner && l.offset == lock.offset && l.length == lock.length)
        });

        self.locks.push(lock);
        Ok(())
    }

    /// Release a lock by owner and range.
    fn unlock(&mut self, owner: &str, offset: u64, length: u64) -> Result<(), LockError> {
        let before = self.locks.len();
        self.locks
            .retain(|l| !(l.owner == owner && l.offset == offset && l.length == length));
        if self.locks.len() == before {
            Err(LockError::NotFound(owner.to_owned()))
        } else {
            Ok(())
        }
    }

    /// Test whether a lock would conflict (LOCKT).
    fn test_lock(
        &self,
        lock_type: LockType,
        offset: u64,
        length: u64,
        owner: &str,
        now_ms: u64,
    ) -> Option<&ByteRangeLock> {
        for existing in &self.locks {
            if existing.is_expired(now_ms) {
                continue;
            }
            if !existing.overlaps(offset, length) {
                continue;
            }
            if existing.owner == owner {
                continue;
            }
            if existing.lock_type == LockType::Read && lock_type == LockType::Read {
                continue;
            }
            return Some(existing);
        }
        None
    }

    /// Expire all stale locks. Returns count expired.
    fn expire(&mut self, now_ms: u64) -> u64 {
        let before = self.locks.len();
        self.locks.retain(|l| !l.is_expired(now_ms));
        (before - self.locks.len()) as u64
    }
}

/// Lock manager — tracks byte-range locks across all files.
///
/// Thread-safe via `Mutex`. Used by `nfs4_server` for stateful
/// lock operations.
pub struct LockManager {
    state: Mutex<HashMap<[u8; 32], FileLockState>>,
    /// Default lease duration for new locks (ms).
    pub default_lease_ms: u64,
}

impl LockManager {
    /// Create a new lock manager with the given default lease.
    #[must_use]
    pub fn new(default_lease_ms: u64) -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            default_lease_ms,
        }
    }

    /// Acquire a byte-range lock on a file handle.
    pub fn lock(
        &self,
        file_handle: [u8; 32],
        owner: &str,
        lock_type: LockType,
        offset: u64,
        length: u64,
        now_ms: u64,
    ) -> Result<(), LockError> {
        let lock = ByteRangeLock {
            owner: owner.to_owned(),
            lock_type,
            offset,
            length,
            acquired_at_ms: now_ms,
            lease_ms: self.default_lease_ms,
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entry(file_handle)
            .or_default()
            .try_lock(lock, now_ms)
    }

    /// Release a byte-range lock.
    pub fn unlock(
        &self,
        file_handle: [u8; 32],
        owner: &str,
        offset: u64,
        length: u64,
    ) -> Result<(), LockError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .entry(file_handle)
            .or_default()
            .unlock(owner, offset, length)
    }

    /// Test whether a lock would conflict without acquiring it (LOCKT).
    #[must_use]
    pub fn test_lock(
        &self,
        file_handle: [u8; 32],
        lock_type: LockType,
        offset: u64,
        length: u64,
        owner: &str,
        now_ms: u64,
    ) -> Option<ByteRangeLock> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .get(&file_handle)
            .and_then(|fs| fs.test_lock(lock_type, offset, length, owner, now_ms).cloned())
    }

    /// Expire all stale locks across all files. Returns total expired.
    pub fn expire_all(&self, now_ms: u64) -> u64 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut total = 0;
        for fs in state.values_mut() {
            total += fs.expire(now_ms);
        }
        // Remove entries with no remaining locks.
        state.retain(|_, fs| !fs.locks.is_empty());
        total
    }

    /// Count total active locks.
    #[must_use]
    pub fn lock_count(&self) -> usize {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.values().map(|fs| fs.locks.len()).sum()
    }
}

impl Default for LockManager {
    fn default() -> Self {
        // `NFSv4` default lease is 90 seconds.
        Self::new(90_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_locks_are_shared() {
        let mgr = LockManager::default();
        let fh = [0x01; 32];
        mgr.lock(fh, "owner-a", LockType::Read, 0, 100, 1000)
            .unwrap();
        mgr.lock(fh, "owner-b", LockType::Read, 0, 100, 1000)
            .unwrap();
        assert_eq!(mgr.lock_count(), 2);
    }

    #[test]
    fn write_lock_is_exclusive() {
        let mgr = LockManager::default();
        let fh = [0x02; 32];
        mgr.lock(fh, "owner-a", LockType::Write, 0, 100, 1000)
            .unwrap();
        let err = mgr
            .lock(fh, "owner-b", LockType::Write, 50, 100, 1000)
            .unwrap_err();
        assert!(matches!(err, LockError::Denied(LockType::Write, _)));
    }

    #[test]
    fn write_blocks_read() {
        let mgr = LockManager::default();
        let fh = [0x03; 32];
        mgr.lock(fh, "owner-a", LockType::Write, 0, 100, 1000)
            .unwrap();
        let err = mgr
            .lock(fh, "owner-b", LockType::Read, 50, 50, 1000)
            .unwrap_err();
        assert!(matches!(err, LockError::Denied(LockType::Write, _)));
    }

    #[test]
    fn non_overlapping_locks_allowed() {
        let mgr = LockManager::default();
        let fh = [0x04; 32];
        mgr.lock(fh, "owner-a", LockType::Write, 0, 50, 1000)
            .unwrap();
        mgr.lock(fh, "owner-b", LockType::Write, 50, 50, 1000)
            .unwrap();
        assert_eq!(mgr.lock_count(), 2);
    }

    #[test]
    fn unlock_removes_lock() {
        let mgr = LockManager::default();
        let fh = [0x05; 32];
        mgr.lock(fh, "owner-a", LockType::Read, 0, 100, 1000)
            .unwrap();
        mgr.unlock(fh, "owner-a", 0, 100).unwrap();
        assert_eq!(mgr.lock_count(), 0);
    }

    #[test]
    fn expired_lock_does_not_block() {
        let mgr = LockManager::new(1000); // 1 second lease
        let fh = [0x06; 32];
        mgr.lock(fh, "owner-a", LockType::Write, 0, 100, 1000)
            .unwrap();
        // At t=3000 (2 seconds later), the lock has expired.
        mgr.lock(fh, "owner-b", LockType::Write, 0, 100, 3000)
            .unwrap();
        assert_eq!(mgr.lock_count(), 1);
    }

    #[test]
    fn test_lock_detects_conflict() {
        let mgr = LockManager::default();
        let fh = [0x07; 32];
        mgr.lock(fh, "owner-a", LockType::Write, 0, 100, 1000)
            .unwrap();
        let conflict = mgr.test_lock(fh, LockType::Read, 50, 50, "owner-b", 1000);
        assert!(conflict.is_some());
        assert_eq!(conflict.unwrap().owner, "owner-a");
    }

    #[test]
    fn same_owner_can_relock() {
        let mgr = LockManager::default();
        let fh = [0x08; 32];
        mgr.lock(fh, "owner-a", LockType::Read, 0, 100, 1000)
            .unwrap();
        // Same owner, same range — re-lock (upgrade to write).
        mgr.lock(fh, "owner-a", LockType::Write, 0, 100, 2000)
            .unwrap();
        assert_eq!(mgr.lock_count(), 1);
    }

    #[test]
    fn expire_all_cleans_stale() {
        let mgr = LockManager::new(500);
        let fh = [0x09; 32];
        mgr.lock(fh, "owner-a", LockType::Read, 0, 100, 1000)
            .unwrap();
        mgr.lock(fh, "owner-b", LockType::Read, 0, 100, 1000)
            .unwrap();
        let expired = mgr.expire_all(2000);
        assert_eq!(expired, 2);
        assert_eq!(mgr.lock_count(), 0);
    }

    #[test]
    fn zero_length_means_eof() {
        let mgr = LockManager::default();
        let fh = [0x0A; 32];
        // Lock entire file (offset=0, length=0 = to EOF).
        mgr.lock(fh, "owner-a", LockType::Write, 0, 0, 1000)
            .unwrap();
        // Any range should conflict.
        let err = mgr
            .lock(fh, "owner-b", LockType::Read, 999_999, 1, 1000)
            .unwrap_err();
        assert!(matches!(err, LockError::Denied(_, _)));
    }
}
