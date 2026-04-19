//! Key material types with zeroize + mlock protection (I-K8).
//!
//! All key material is wrapped in `Zeroizing<T>` so it is wiped from
//! memory on drop. Pages holding key material are `mlock`'d to prevent
//! swapping and marked `MADV_DONTDUMP` to exclude from core dumps.
//! `Debug` impls never print key bytes.

use kiseki_common::tenancy::KeyEpoch;
use zeroize::Zeroizing;

use crate::mem_protect;

/// System master key — one per epoch. Cached locally on storage nodes
/// for HKDF derivation (ADR-003). The key manager distributes these;
/// the storage node never sends chunk IDs back.
pub struct SystemMasterKey {
    /// Key material — 32 bytes for AES-256.
    material: Zeroizing<[u8; 32]>,
    /// Whether mlock succeeded for this key's memory page.
    mlocked: bool,
    /// Epoch this key belongs to.
    pub epoch: KeyEpoch,
}

impl SystemMasterKey {
    /// Create a new master key from raw material.
    ///
    /// Attempts to `mlock` the key material page. Failure is
    /// non-fatal (`RLIMIT_MEMLOCK` may be insufficient).
    #[must_use]
    pub fn new(material: [u8; 32], epoch: KeyEpoch) -> Self {
        let mut key = Self {
            material: Zeroizing::new(material),
            mlocked: false,
            epoch,
        };
        // SAFETY: material field is a valid [u8; 32] within the struct,
        // and will remain valid until Drop.
        key.mlocked = unsafe { mem_protect::mlock(key.material.as_ptr(), 32) };
        key
    }

    /// Access the raw key bytes. Caller must not log, persist, or
    /// transmit this value (I-K8).
    #[must_use]
    pub fn material(&self) -> &[u8; 32] {
        &self.material
    }
}

impl Drop for SystemMasterKey {
    fn drop(&mut self) {
        if self.mlocked {
            // SAFETY: same pointer and length as the mlock call in new().
            unsafe { mem_protect::munlock(self.material.as_ptr(), 32) };
        }
        // Zeroizing handles zeroing the material on drop.
    }
}

// I-K8: Debug must never print key material or mlocked flag.
impl core::fmt::Debug for SystemMasterKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SystemMasterKey")
            .field("epoch", &self.epoch)
            .field("material", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Tenant KEK material — obtained from the tenant KMS, cached with
/// bounded TTL per ADR-011. Destruction = crypto-shred (I-K5).
pub struct TenantKek {
    /// Wrapping key material — 32 bytes.
    material: Zeroizing<[u8; 32]>,
    /// Whether mlock succeeded.
    mlocked: bool,
    /// Epoch of this tenant KEK.
    pub epoch: KeyEpoch,
}

impl TenantKek {
    /// Create from raw material obtained from tenant KMS.
    #[must_use]
    pub fn new(material: [u8; 32], epoch: KeyEpoch) -> Self {
        let mut key = Self {
            material: Zeroizing::new(material),
            mlocked: false,
            epoch,
        };
        // SAFETY: material field is a valid [u8; 32] within the struct.
        key.mlocked = unsafe { mem_protect::mlock(key.material.as_ptr(), 32) };
        key
    }

    /// Access the raw key bytes. Caller must not log, persist, or
    /// transmit this value (I-K8).
    #[must_use]
    pub fn material(&self) -> &[u8; 32] {
        &self.material
    }
}

impl Drop for TenantKek {
    fn drop(&mut self) {
        if self.mlocked {
            // SAFETY: same pointer and length as the mlock call in new().
            unsafe { mem_protect::munlock(self.material.as_ptr(), 32) };
        }
    }
}

impl core::fmt::Debug for TenantKek {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TenantKek")
            .field("epoch", &self.epoch)
            .field("material", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Local cache for master keys — one per active epoch.
/// Storage nodes hold this to derive per-chunk DEKs locally (ADR-003).
pub struct MasterKeyCache {
    keys: Vec<SystemMasterKey>,
}

impl MasterKeyCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// Insert or replace a master key for the given epoch.
    pub fn insert(&mut self, key: SystemMasterKey) {
        // Replace if epoch already present.
        if let Some(existing) = self.keys.iter_mut().find(|k| k.epoch == key.epoch) {
            *existing = key;
        } else {
            self.keys.push(key);
        }
    }

    /// Look up the master key for a given epoch.
    pub(crate) fn get(&self, epoch: KeyEpoch) -> Option<&SystemMasterKey> {
        self.keys.iter().find(|k| k.epoch == epoch)
    }

    /// Return the current (highest) epoch key.
    #[must_use]
    pub fn current(&self) -> Option<&SystemMasterKey> {
        self.keys.iter().max_by_key(|k| k.epoch)
    }
}

impl Default for MasterKeyCache {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for MasterKeyCache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MasterKeyCache")
            .field("epoch_count", &self.keys.len())
            .finish()
    }
}
