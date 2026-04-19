//! Epoch management — create, rotate, retain master keys.
//!
//! The key manager stores one master key per epoch. During rotation,
//! two epochs coexist (I-K6). Old epoch keys are retained until all
//! chunks encrypted under that epoch have been re-wrapped.

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;

use crate::error::KeyManagerError;

/// Metadata about a key epoch.
#[derive(Clone, Debug)]
pub struct EpochInfo {
    /// Epoch identifier.
    pub epoch: KeyEpoch,
    /// Whether this is the current (active) epoch for new operations.
    pub is_current: bool,
    /// Whether background re-wrapping from this epoch is complete.
    pub migration_complete: bool,
}

/// Key manager operations trait.
///
/// Implementations: `MemKeyStore` (in-memory, for testing),
/// Raft-backed store (production, future).
pub trait KeyManagerOps {
    /// Fetch the master key for a given epoch.
    ///
    /// Returns the key material. The caller (storage node) caches this
    /// locally for HKDF derivation (ADR-003).
    fn fetch_master_key(&self, epoch: KeyEpoch) -> Result<&SystemMasterKey, KeyManagerError>;

    /// Get the current (latest) epoch.
    fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError>;

    /// Rotate the system master key — creates a new epoch with a fresh
    /// CSPRNG-generated master key.
    ///
    /// The previous epoch's key is retained until migration is marked
    /// complete (I-K6). Returns the new epoch.
    fn rotate(&mut self) -> Result<KeyEpoch, KeyManagerError>;

    /// Mark an old epoch's migration as complete. The key is retained
    /// for reads but no new operations use it.
    fn mark_migration_complete(&mut self, epoch: KeyEpoch) -> Result<(), KeyManagerError>;

    /// List all epochs and their status.
    fn list_epochs(&self) -> Vec<EpochInfo>;
}
