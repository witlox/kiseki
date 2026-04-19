//! In-memory key store — reference implementation of [`KeyManagerOps`].
//!
//! Uses `Mutex` for interior mutability so that `KeyManagerOps` methods
//! can take `&self` (required for Raft-backed implementations).

use std::sync::{Arc, Mutex};

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::{KeyManagerHealth, KeyManagerStatus};

/// Entry for a single epoch in the key store.
struct EpochEntry {
    key: Arc<SystemMasterKey>,
    is_current: bool,
    migration_complete: bool,
}

/// Inner state behind the mutex.
struct Inner {
    epochs: Vec<EpochEntry>,
    status: KeyManagerStatus,
}

/// In-memory key store for testing and development.
pub struct MemKeyStore {
    inner: Mutex<Inner>,
}

impl MemKeyStore {
    /// Create an empty key store and generate the initial epoch (epoch 1).
    pub fn new() -> Result<Self, KeyManagerError> {
        let key_material = generate_master_key()?;
        let inner = Inner {
            epochs: vec![EpochEntry {
                key: Arc::new(SystemMasterKey::new(key_material, KeyEpoch(1))),
                is_current: true,
                migration_complete: true,
            }],
            status: KeyManagerStatus::Healthy,
        };
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Get the health status of this key store.
    #[must_use]
    pub fn health(&self) -> KeyManagerHealth {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        KeyManagerHealth {
            status: inner.status,
            epoch_count: inner.epochs.len(),
            current_epoch: inner
                .epochs
                .iter()
                .find(|e| e.is_current)
                .map(|e| e.key.epoch.0),
        }
    }

    /// Set the status (for testing failure scenarios).
    pub fn set_status(&self, status: KeyManagerStatus) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.status = status;
    }
}

impl Default for MemKeyStore {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            inner: Mutex::new(Inner {
                epochs: Vec::new(),
                status: KeyManagerStatus::Unavailable,
            }),
        })
    }
}

#[tonic::async_trait]
impl KeyManagerOps for MemKeyStore {
    async fn fetch_master_key(
        &self,
        epoch: KeyEpoch,
    ) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        inner
            .epochs
            .iter()
            .find(|e| e.key.epoch == epoch)
            .map(|e| Arc::clone(&e.key))
            .ok_or(KeyManagerError::EpochNotFound(epoch))
    }

    async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        inner
            .epochs
            .iter()
            .find(|e| e.is_current)
            .map(|e| e.key.epoch)
            .ok_or(KeyManagerError::Unavailable)
    }

    async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }

        let next_epoch = inner
            .epochs
            .iter()
            .map(|e| e.key.epoch.0)
            .max()
            .unwrap_or(0)
            + 1;

        let key_material = generate_master_key()?;

        for entry in &mut inner.epochs {
            if entry.is_current {
                entry.is_current = false;
            }
        }

        let new_epoch = KeyEpoch(next_epoch);
        inner.epochs.push(EpochEntry {
            key: Arc::new(SystemMasterKey::new(key_material, new_epoch)),
            is_current: true,
            migration_complete: false,
        });

        Ok(new_epoch)
    }

    async fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = inner
            .epochs
            .iter_mut()
            .find(|e| e.key.epoch == epoch)
            .ok_or(KeyManagerError::EpochNotFound(epoch))?;
        entry.migration_complete = true;
        Ok(())
    }

    async fn list_epochs(&self) -> Vec<EpochInfo> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .epochs
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
        f.debug_struct("MemKeyStore").finish_non_exhaustive()
    }
}

/// Generate a 32-byte master key from the system CSPRNG.
fn generate_master_key() -> Result<[u8; 32], KeyManagerError> {
    let mut key = [0u8; 32];
    aws_lc_rs::rand::fill(&mut key).map_err(|_| KeyManagerError::KeyGenerationFailed)?;
    Ok(key)
}
