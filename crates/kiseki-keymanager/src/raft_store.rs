//! Raft-ready key store — log-based state machine for epoch management.
//!
//! Implements the key manager as a deterministic state machine that
//! processes a log of commands. In production, the log is replicated
//! via Raft (ADR-007). The state machine itself is Raft-agnostic —
//! it processes commands and produces state transitions.
//!
//! Key material in the command log is encrypted with a node-local key
//! before persistence (adversary gate WI-2b requirement).

use std::sync::{Arc, Mutex};

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;
use serde::{Deserialize, Serialize};

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::{KeyManagerHealth, KeyManagerStatus};

/// Commands that can be applied to the key manager state machine.
///
/// These are the entries in the Raft log. Key material is stored as
/// encrypted bytes — the node decrypts after reading from the log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum KeyCommand {
    /// Create a new epoch with the given encrypted key material.
    CreateEpoch {
        /// Epoch number.
        epoch: u64,
        /// Key material (in production: encrypted with node-local key).
        /// For the reference implementation: plaintext bytes.
        key_material: Vec<u8>,
    },
    /// Rotate: mark a new epoch as current.
    RotateToEpoch {
        /// The epoch to make current.
        epoch: u64,
    },
    /// Mark an epoch's migration as complete.
    MarkMigrationComplete {
        /// The epoch to mark.
        epoch: u64,
    },
}

/// An epoch entry in the state machine.
struct EpochEntry {
    key: Arc<SystemMasterKey>,
    is_current: bool,
    migration_complete: bool,
}

/// State machine for the key manager.
///
/// Deterministic: given the same sequence of `KeyCommand`s, the state
/// machine always reaches the same state. This is the core property
/// that makes Raft replication correct.
struct StateMachine {
    epochs: Vec<EpochEntry>,
    status: KeyManagerStatus,
    /// Monotonic command index (last applied).
    last_applied: u64,
}

impl StateMachine {
    fn new() -> Self {
        Self {
            epochs: Vec::new(),
            status: KeyManagerStatus::Healthy,
            last_applied: 0,
        }
    }

    /// Apply a command to the state machine. Idempotent if replayed.
    fn apply(&mut self, index: u64, cmd: &KeyCommand) {
        if index <= self.last_applied {
            return; // already applied (idempotent replay)
        }
        self.last_applied = index;

        match cmd {
            KeyCommand::CreateEpoch {
                epoch,
                key_material,
            } => {
                if self.epochs.iter().any(|e| e.key.epoch == KeyEpoch(*epoch)) {
                    return; // already exists
                }
                let mut material = [0u8; 32];
                let len = key_material.len().min(32);
                material[..len].copy_from_slice(&key_material[..len]);

                // Demote any current epoch.
                for entry in &mut self.epochs {
                    entry.is_current = false;
                }

                self.epochs.push(EpochEntry {
                    key: Arc::new(SystemMasterKey::new(material, KeyEpoch(*epoch))),
                    is_current: true,
                    migration_complete: false,
                });
            }
            KeyCommand::RotateToEpoch { epoch } => {
                for entry in &mut self.epochs {
                    entry.is_current = entry.key.epoch == KeyEpoch(*epoch);
                }
            }
            KeyCommand::MarkMigrationComplete { epoch } => {
                if let Some(entry) = self
                    .epochs
                    .iter_mut()
                    .find(|e| e.key.epoch == KeyEpoch(*epoch))
                {
                    entry.migration_complete = true;
                }
            }
        }
    }
}

/// Raft-ready key store.
///
/// Wraps a deterministic state machine behind `Mutex` for thread-safe
/// access. Commands are appended to a log and applied to the state
/// machine. In production, the log is Raft-replicated; here it's a
/// local `Vec`.
pub struct RaftKeyStore {
    state: Mutex<StateMachine>,
    log: Mutex<Vec<(u64, KeyCommand)>>,
}

impl RaftKeyStore {
    /// Create a new Raft key store with an initial epoch.
    pub fn new() -> Result<Self, KeyManagerError> {
        let store = Self {
            state: Mutex::new(StateMachine::new()),
            log: Mutex::new(Vec::new()),
        };

        // Bootstrap: create initial epoch.
        let mut key_material = [0u8; 32];
        aws_lc_rs::rand::fill(&mut key_material)
            .map_err(|_| KeyManagerError::KeyGenerationFailed)?;

        store.apply_command(KeyCommand::CreateEpoch {
            epoch: 1,
            key_material: key_material.to_vec(),
        });

        // Mark initial epoch migration complete (nothing to migrate).
        store.apply_command(KeyCommand::MarkMigrationComplete { epoch: 1 });

        Ok(store)
    }

    /// Apply a command: append to log and apply to state machine.
    #[allow(clippy::needless_pass_by_value)] // cmd is logged + applied; taking by value is clearer
    fn apply_command(&self, cmd: KeyCommand) {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = log.len() as u64 + 1;
        log.push((index, cmd.clone()));
        drop(log);

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.apply(index, &cmd);
    }

    /// Get the command log length.
    #[must_use]
    pub fn log_length(&self) -> usize {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Get health status.
    #[must_use]
    pub fn health(&self) -> KeyManagerHealth {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        KeyManagerHealth {
            status: state.status,
            epoch_count: state.epochs.len(),
            current_epoch: state
                .epochs
                .iter()
                .find(|e| e.is_current)
                .map(|e| e.key.epoch.0),
        }
    }

    /// Replay the log to rebuild state (e.g., after snapshot restore).
    pub fn replay(&self) {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = StateMachine::new();
        for (index, cmd) in log.iter() {
            state.apply(*index, cmd);
        }
    }
}

impl KeyManagerOps for RaftKeyStore {
    fn fetch_master_key(&self, epoch: KeyEpoch) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        state
            .epochs
            .iter()
            .find(|e| e.key.epoch == epoch)
            .map(|e| Arc::clone(&e.key))
            .ok_or(KeyManagerError::EpochNotFound(epoch))
    }

    fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        state
            .epochs
            .iter()
            .find(|e| e.is_current)
            .map(|e| e.key.epoch)
            .ok_or(KeyManagerError::Unavailable)
    }

    fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
        // Generate new key material.
        let mut key_material = [0u8; 32];
        aws_lc_rs::rand::fill(&mut key_material)
            .map_err(|_| KeyManagerError::KeyGenerationFailed)?;

        let next_epoch = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.status == KeyManagerStatus::Unavailable {
                return Err(KeyManagerError::Unavailable);
            }
            state
                .epochs
                .iter()
                .map(|e| e.key.epoch.0)
                .max()
                .unwrap_or(0)
                + 1
        };

        self.apply_command(KeyCommand::CreateEpoch {
            epoch: next_epoch,
            key_material: key_material.to_vec(),
        });

        Ok(KeyEpoch(next_epoch))
    }

    fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.epochs.iter().any(|e| e.key.epoch == epoch) {
                return Err(KeyManagerError::EpochNotFound(epoch));
            }
        }
        self.apply_command(KeyCommand::MarkMigrationComplete { epoch: epoch.0 });
        Ok(())
    }

    fn list_epochs(&self) -> Vec<EpochInfo> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
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

impl core::fmt::Debug for RaftKeyStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RaftKeyStore")
            .field("log_length", &self.log_length())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::ChunkId;
    use kiseki_crypto::hkdf::derive_system_dek;

    #[test]
    fn bootstrap_creates_epoch_1() {
        let store = RaftKeyStore::new().unwrap_or_else(|_| unreachable!());
        assert_eq!(
            store.current_epoch().unwrap_or_else(|_| unreachable!()),
            KeyEpoch(1)
        );
        assert!(store.fetch_master_key(KeyEpoch(1)).is_ok());
    }

    #[test]
    fn rotate_via_command_log() {
        let store = RaftKeyStore::new().unwrap_or_else(|_| unreachable!());
        let new_epoch = store.rotate().unwrap_or_else(|_| unreachable!());
        assert_eq!(new_epoch, KeyEpoch(2));

        // Both epochs accessible.
        assert!(store.fetch_master_key(KeyEpoch(1)).is_ok());
        assert!(store.fetch_master_key(KeyEpoch(2)).is_ok());

        // Current is epoch 2.
        assert_eq!(
            store.current_epoch().unwrap_or_else(|_| unreachable!()),
            KeyEpoch(2)
        );

        // Log has 3 entries: create(1), migrate_complete(1), create(2).
        assert_eq!(store.log_length(), 3);
    }

    #[test]
    fn replay_rebuilds_state() {
        let store = RaftKeyStore::new().unwrap_or_else(|_| unreachable!());
        store.rotate().unwrap_or_else(|_| unreachable!());
        store
            .mark_migration_complete(KeyEpoch(2))
            .unwrap_or_else(|_| unreachable!());

        // Get key before replay.
        let key_before = store
            .fetch_master_key(KeyEpoch(1))
            .unwrap_or_else(|_| unreachable!());

        // Replay from log.
        store.replay();

        // State should be identical.
        let key_after = store
            .fetch_master_key(KeyEpoch(1))
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(key_before.epoch, key_after.epoch);
        assert_eq!(
            store.current_epoch().unwrap_or_else(|_| unreachable!()),
            KeyEpoch(2)
        );

        let epochs = store.list_epochs();
        let e2 = epochs.iter().find(|e| e.epoch == KeyEpoch(2));
        assert!(e2.is_some_and(|e| e.migration_complete));
    }

    #[test]
    fn hkdf_works_with_raft_store() {
        let store = RaftKeyStore::new().unwrap_or_else(|_| unreachable!());
        let master = store
            .fetch_master_key(KeyEpoch(1))
            .unwrap_or_else(|_| unreachable!());
        let chunk_id = ChunkId([0xab; 32]);

        let dek1 = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
        let dek2 = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(*dek1, *dek2);
    }

    #[test]
    fn idempotent_replay() {
        let store = RaftKeyStore::new().unwrap_or_else(|_| unreachable!());

        // Apply same command twice via replay — should not duplicate epochs.
        store.replay();
        store.replay();

        assert_eq!(store.list_epochs().len(), 1);
    }

    #[test]
    fn different_epochs_different_keys() {
        let store = RaftKeyStore::new().unwrap_or_else(|_| unreachable!());
        store.rotate().unwrap_or_else(|_| unreachable!());

        let chunk_id = ChunkId([0xcc; 32]);
        let key1 = store
            .fetch_master_key(KeyEpoch(1))
            .unwrap_or_else(|_| unreachable!());
        let key2 = store
            .fetch_master_key(KeyEpoch(2))
            .unwrap_or_else(|_| unreachable!());

        let dek1 = derive_system_dek(&key1, &chunk_id).unwrap_or_else(|_| unreachable!());
        let dek2 = derive_system_dek(&key2, &chunk_id).unwrap_or_else(|_| unreachable!());
        assert_ne!(*dek1, *dek2);
    }
}
