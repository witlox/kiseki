//! Persistent key store — wraps `RaftKeyStore` + `RedbLogStore`.
//!
//! Every key command is written to both the in-memory state machine
//! (fast reads) and redb (durability). On startup, reloads from redb
//! and replays the command log to rebuild state. Per ADR-007/ADR-022.

use std::path::Path;
use std::sync::Arc;

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_raft::redb_log_store::RedbLogStore;

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::KeyManagerHealth;
use crate::raft_store::{KeyCommand, RaftKeyStore};

/// Persistent key store — in-memory state machine + redb for durability.
pub struct PersistentKeyStore {
    inner: RaftKeyStore,
    redb: RedbLogStore,
}

impl PersistentKeyStore {
    /// Open or create a persistent key store at the given path.
    ///
    /// If the redb database contains existing commands, they are replayed
    /// into the state machine on startup. If no commands exist, a fresh
    /// epoch 1 is generated (bootstrap).
    pub fn open(path: &Path) -> Result<Self, KeyManagerError> {
        let redb = RedbLogStore::open(path).map_err(|_| KeyManagerError::Unavailable)?;

        // Check if we have persisted commands to replay.
        let entries: Vec<(u64, KeyCommand)> = redb.range(1, u64::MAX).unwrap_or_default();

        if entries.is_empty() {
            // Fresh start — bootstrap with new epoch 1.
            let inner = RaftKeyStore::new()?;
            let store = Self { inner, redb };

            // Persist the bootstrap commands.
            let log = store.inner.command_log();
            for (idx, cmd) in &log {
                let _ = store.redb.append(*idx, cmd);
            }

            Ok(store)
        } else {
            // Reload from persisted commands.
            let inner = RaftKeyStore::from_commands(entries.iter().map(|(i, c)| (*i, c.clone())))?;
            Ok(Self { inner, redb })
        }
    }

    /// Persist a command to redb and apply it to the state machine.
    #[allow(clippy::needless_pass_by_value)]
    fn persist_and_apply(&self, cmd: KeyCommand) {
        let idx = self.inner.apply_command(cmd.clone());
        let _ = self.redb.append(idx, &cmd);
    }
}

#[tonic::async_trait]
impl KeyManagerOps for PersistentKeyStore {
    async fn fetch_master_key(
        &self,
        epoch: KeyEpoch,
    ) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
        self.inner.fetch_master_key(epoch).await
    }

    async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        self.inner.current_epoch().await
    }

    async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
        let mut key_material = [0u8; 32];
        aws_lc_rs::rand::fill(&mut key_material)
            .map_err(|_| KeyManagerError::KeyGenerationFailed)?;

        let next_epoch = {
            let epochs = self.inner.list_epochs().await;
            epochs.iter().map(|e| e.epoch.0).max().unwrap_or(0) + 1
        };

        self.persist_and_apply(KeyCommand::CreateEpoch {
            epoch: next_epoch,
            key_material: key_material.to_vec(),
        });

        Ok(KeyEpoch(next_epoch))
    }

    async fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        // Verify epoch exists.
        self.inner.fetch_master_key(epoch).await?;
        self.persist_and_apply(KeyCommand::MarkMigrationComplete { epoch: epoch.0 });
        Ok(())
    }

    async fn list_epochs(&self) -> Vec<EpochInfo> {
        self.inner.list_epochs().await
    }
}

impl PersistentKeyStore {
    /// Get health status.
    #[must_use]
    pub fn health(&self) -> KeyManagerHealth {
        self.inner.health()
    }
}

impl core::fmt::Debug for PersistentKeyStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PersistentKeyStore")
            .field("inner", &self.inner)
            .field("redb", &"RedbLogStore")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentKeyStore::open(&dir.path().join("keys.redb")).unwrap();
        assert_eq!(store.current_epoch().await.unwrap(), KeyEpoch(1));
        assert!(store.fetch_master_key(KeyEpoch(1)).await.is_ok());
    }

    #[tokio::test]
    async fn epochs_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.redb");

        let epoch1_material;
        // Write: bootstrap + rotate to epoch 2.
        {
            let store = PersistentKeyStore::open(&path).unwrap();
            let key1 = store.fetch_master_key(KeyEpoch(1)).await.unwrap();
            epoch1_material = key1.material().to_vec();

            let e2 = store.rotate().await.unwrap();
            assert_eq!(e2, KeyEpoch(2));
            store.mark_migration_complete(KeyEpoch(2)).await.unwrap();
        }

        // Reopen — epochs should survive.
        {
            let store = PersistentKeyStore::open(&path).unwrap();

            // Both epochs present.
            assert!(store.fetch_master_key(KeyEpoch(1)).await.is_ok());
            assert!(store.fetch_master_key(KeyEpoch(2)).await.is_ok());

            // Current epoch is 2.
            assert_eq!(store.current_epoch().await.unwrap(), KeyEpoch(2));

            // Epoch 1 key material is identical.
            let key1 = store.fetch_master_key(KeyEpoch(1)).await.unwrap();
            assert_eq!(key1.material(), &epoch1_material[..]);

            // Migration complete flag preserved.
            let epochs = store.list_epochs().await;
            let e2 = epochs.iter().find(|e| e.epoch == KeyEpoch(2)).unwrap();
            assert!(e2.migration_complete);
        }
    }

    #[tokio::test]
    async fn rotate_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.redb");

        {
            let store = PersistentKeyStore::open(&path).unwrap();
            store.rotate().await.unwrap(); // epoch 2
        }

        {
            let store = PersistentKeyStore::open(&path).unwrap();
            let e3 = store.rotate().await.unwrap();
            assert_eq!(e3, KeyEpoch(3));
            assert_eq!(store.list_epochs().await.len(), 3);
        }
    }
}
