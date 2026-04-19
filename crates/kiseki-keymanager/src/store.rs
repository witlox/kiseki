//! In-memory key store — reference implementation of [`KeyManagerOps`].
//!
//! Production use will replace with a Raft-backed store (ADR-007).

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::{KeyManagerHealth, KeyManagerStatus};

/// Entry for a single epoch in the key store.
struct EpochEntry {
    key: SystemMasterKey,
    is_current: bool,
    migration_complete: bool,
}

/// In-memory key store for testing and development.
pub struct MemKeyStore {
    epochs: Vec<EpochEntry>,
    status: KeyManagerStatus,
}

impl MemKeyStore {
    /// Create an empty key store and generate the initial epoch (epoch 1).
    pub fn new() -> Result<Self, KeyManagerError> {
        let mut store = Self {
            epochs: Vec::new(),
            status: KeyManagerStatus::Healthy,
        };

        // Generate the initial master key (epoch 1).
        let key_material = generate_master_key()?;
        store.epochs.push(EpochEntry {
            key: SystemMasterKey::new(key_material, KeyEpoch(1)),
            is_current: true,
            migration_complete: true, // initial epoch has nothing to migrate
        });

        Ok(store)
    }

    /// Get the health status of this key store.
    #[must_use]
    pub fn health(&self) -> KeyManagerHealth {
        KeyManagerHealth {
            status: self.status,
            epoch_count: self.epochs.len(),
            current_epoch: self
                .epochs
                .iter()
                .find(|e| e.is_current)
                .map(|e| e.key.epoch.0),
        }
    }

    /// Set the status (for testing failure scenarios).
    pub fn set_status(&mut self, status: KeyManagerStatus) {
        self.status = status;
    }
}

impl Default for MemKeyStore {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            epochs: Vec::new(),
            status: KeyManagerStatus::Unavailable,
        })
    }
}

impl KeyManagerOps for MemKeyStore {
    fn fetch_master_key(&self, epoch: KeyEpoch) -> Result<&SystemMasterKey, KeyManagerError> {
        if self.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        self.epochs
            .iter()
            .find(|e| e.key.epoch == epoch)
            .map(|e| &e.key)
            .ok_or(KeyManagerError::EpochNotFound(epoch))
    }

    fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        if self.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        self.epochs
            .iter()
            .find(|e| e.is_current)
            .map(|e| e.key.epoch)
            .ok_or(KeyManagerError::Unavailable)
    }

    fn rotate(&mut self) -> Result<KeyEpoch, KeyManagerError> {
        if self.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }

        // Determine next epoch number.
        let next_epoch = self.epochs.iter().map(|e| e.key.epoch.0).max().unwrap_or(0) + 1;

        // Generate new master key.
        let key_material = generate_master_key()?;

        // Demote the current epoch.
        for entry in &mut self.epochs {
            if entry.is_current {
                entry.is_current = false;
            }
        }

        // Insert new epoch as current.
        let new_epoch = KeyEpoch(next_epoch);
        self.epochs.push(EpochEntry {
            key: SystemMasterKey::new(key_material, new_epoch),
            is_current: true,
            migration_complete: false,
        });

        Ok(new_epoch)
    }

    fn mark_migration_complete(&mut self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        let entry = self
            .epochs
            .iter_mut()
            .find(|e| e.key.epoch == epoch)
            .ok_or(KeyManagerError::EpochNotFound(epoch))?;
        entry.migration_complete = true;
        Ok(())
    }

    fn list_epochs(&self) -> Vec<EpochInfo> {
        self.epochs
            .iter()
            .map(|e| EpochInfo {
                epoch: e.key.epoch,
                is_current: e.is_current,
                migration_complete: e.migration_complete,
            })
            .collect()
    }
}

impl core::fmt::Debug for MemKeyStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemKeyStore")
            .field("epoch_count", &self.epochs.len())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Generate a 32-byte master key from the system CSPRNG.
fn generate_master_key() -> Result<[u8; 32], KeyManagerError> {
    let mut key = [0u8; 32];
    aws_lc_rs::rand::fill(&mut key).map_err(|_| KeyManagerError::KeyGenerationFailed)?;
    Ok(key)
}
