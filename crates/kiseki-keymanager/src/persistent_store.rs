//! Persistent key store — wraps `RaftKeyStore` + `RedbLogStore`.
//!
//! Every key command is written to both the in-memory state machine
//! (fast reads) and redb (durability). On startup, reloads from redb
//! and replays the command log to rebuild state. Per ADR-007/ADR-022.
//!
//! Phase 14e: every command persisted to redb is wrapped in an
//! AES-256-GCM envelope keyed off this node's identity (see
//! `node_identity`). Reads decrypt with the same key — a wrong-key
//! open fails AEAD authentication, surfacing as
//! [`KeyManagerError::Unavailable`]. The in-memory state machine and
//! the inter-node Raft transport remain plaintext (mTLS protects the
//! wire); only the on-disk log is encrypted.

use std::path::Path;
use std::sync::Arc;

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::aead::{Aead, GCM_NONCE_LEN};
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_raft::redb_log_store::RedbLogStore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::KeyManagerHealth;
use crate::node_identity::{derive_at_rest_key, NodeIdentitySource};
use crate::raft_store::{KeyCommand, RaftKeyStore};

/// On-disk envelope around a serialised [`KeyCommand`]. The
/// `ciphertext` includes the GCM authentication tag.
#[derive(Serialize, Deserialize)]
struct EncryptedEntry {
    /// 96-bit GCM nonce, generated per-write via the AWS-LC CSPRNG.
    nonce: [u8; GCM_NONCE_LEN],
    /// AES-256-GCM ciphertext + 128-bit tag.
    ciphertext: Vec<u8>,
}

/// AAD prefix: domain-separates the at-rest envelope from any other
/// AES-GCM use of the same key. The log index is appended per-entry
/// so a swapped-index attack fails AEAD verification.
const AAD_DOMAIN: &[u8] = b"kiseki/raft-key-store/v1";

fn aad_for(index: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(AAD_DOMAIN.len() + 1 + 8);
    out.extend_from_slice(AAD_DOMAIN);
    out.push(b':');
    out.extend_from_slice(&index.to_be_bytes());
    out
}

/// Persistent key store — in-memory state machine + redb for durability.
pub struct PersistentKeyStore {
    inner: RaftKeyStore,
    redb: RedbLogStore,
    at_rest_key: Zeroizing<[u8; 32]>,
    aead: Aead,
}

impl PersistentKeyStore {
    /// Open or create a persistent key store at the given path.
    ///
    /// `identity` supplies the per-node secret used to derive the
    /// at-rest encryption key (HKDF-SHA256 + `salt`). `salt` should
    /// be a per-node value such as the node ID encoded as bytes —
    /// guarantees two nodes never share the derived key.
    ///
    /// If the redb database contains existing commands, they are
    /// decrypted and replayed into the state machine on startup. If no
    /// commands exist, a fresh epoch 1 is generated (bootstrap).
    pub fn open(
        path: &Path,
        identity: &dyn NodeIdentitySource,
        salt: &[u8],
    ) -> Result<Self, KeyManagerError> {
        let at_rest_key =
            derive_at_rest_key(identity, salt).map_err(|_| KeyManagerError::Unavailable)?;
        let redb = RedbLogStore::open(path).map_err(|_| KeyManagerError::Unavailable)?;
        let aead = Aead::new();

        // Decrypt every persisted entry.
        let raw: Vec<(u64, EncryptedEntry)> = redb.range(1, u64::MAX).unwrap_or_default();
        let mut commands: Vec<(u64, KeyCommand)> = Vec::with_capacity(raw.len());
        for (idx, env) in raw {
            let bytes = aead
                .open(&at_rest_key, &env.nonce, &env.ciphertext, &aad_for(idx))
                .map_err(|_| KeyManagerError::Unavailable)?;
            let cmd: KeyCommand =
                serde_json::from_slice(&bytes).map_err(|_| KeyManagerError::Unavailable)?;
            commands.push((idx, cmd));
        }

        if commands.is_empty() {
            // Fresh start — bootstrap with new epoch 1.
            let inner = RaftKeyStore::new()?;
            let store = Self {
                inner,
                redb,
                at_rest_key,
                aead,
            };
            // Persist the bootstrap commands as encrypted envelopes.
            let log = store.inner.command_log();
            for (idx, cmd) in &log {
                store.persist_encrypted(*idx, cmd)?;
            }
            Ok(store)
        } else {
            // Reload from persisted commands.
            let inner = RaftKeyStore::from_commands(commands.into_iter())?;
            Ok(Self {
                inner,
                redb,
                at_rest_key,
                aead,
            })
        }
    }

    fn persist_encrypted(&self, index: u64, cmd: &KeyCommand) -> Result<(), KeyManagerError> {
        let bytes = serde_json::to_vec(cmd).map_err(|_| KeyManagerError::Unavailable)?;
        let (ciphertext, nonce) = self
            .aead
            .seal(&self.at_rest_key, &bytes, &aad_for(index))
            .map_err(|_| KeyManagerError::Unavailable)?;
        self.redb
            .append(index, &EncryptedEntry { nonce, ciphertext })
            .map_err(|_| KeyManagerError::Unavailable)?;
        Ok(())
    }

    /// Persist a command to redb and apply it to the state machine.
    #[allow(clippy::needless_pass_by_value)]
    fn persist_and_apply(&self, cmd: KeyCommand) {
        let idx = self.inner.apply_command(cmd.clone());
        let _ = self.persist_encrypted(idx, &cmd);
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
        // Intentionally elide `at_rest_key` (sensitive) and `aead`
        // (no useful Debug output).
        f.debug_struct("PersistentKeyStore")
            .field("inner", &self.inner)
            .field("redb", &"RedbLogStore")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_identity::TestIdentitySource;

    /// Common test identity. Each test gets a fresh tempdir, so the
    /// salt only needs to be stable across reopen-within-test.
    fn identity() -> TestIdentitySource {
        TestIdentitySource::new(b"test-node-identity-secret".to_vec())
    }

    const SALT: &[u8] = b"node-1";

    #[tokio::test]
    async fn bootstrap_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            PersistentKeyStore::open(&dir.path().join("keys.redb"), &identity(), SALT).unwrap();
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
            let store = PersistentKeyStore::open(&path, &identity(), SALT).unwrap();
            let key1 = store.fetch_master_key(KeyEpoch(1)).await.unwrap();
            epoch1_material = key1.material().to_vec();

            let e2 = store.rotate().await.unwrap();
            assert_eq!(e2, KeyEpoch(2));
            store.mark_migration_complete(KeyEpoch(2)).await.unwrap();
        }

        // Reopen — epochs should survive.
        {
            let store = PersistentKeyStore::open(&path, &identity(), SALT).unwrap();

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
            let store = PersistentKeyStore::open(&path, &identity(), SALT).unwrap();
            store.rotate().await.unwrap(); // epoch 2
        }

        {
            let store = PersistentKeyStore::open(&path, &identity(), SALT).unwrap();
            let e3 = store.rotate().await.unwrap();
            assert_eq!(e3, KeyEpoch(3));
            assert_eq!(store.list_epochs().await.len(), 3);
        }
    }

    /// Phase 14e: opening the redb with a *different* node identity
    /// must fail AEAD authentication — proves the at-rest envelope
    /// actually depends on the node-derived key, not just on our
    /// having any key at all.
    #[tokio::test]
    async fn wrong_node_identity_cannot_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.redb");

        // Bootstrap with identity-A.
        {
            let id_a = TestIdentitySource::new(b"identity-A".to_vec());
            let _ = PersistentKeyStore::open(&path, &id_a, SALT).unwrap();
        }

        // Reopen with identity-B → AEAD fails on the very first entry.
        let id_b = TestIdentitySource::new(b"identity-B".to_vec());
        let err = PersistentKeyStore::open(&path, &id_b, SALT).unwrap_err();
        assert!(matches!(err, KeyManagerError::Unavailable));
    }

    /// Same identity, different salt (e.g. wrong node id) must also
    /// fail — guards against operators sharing keys across nodes.
    #[tokio::test]
    async fn wrong_salt_cannot_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.redb");

        {
            let _ = PersistentKeyStore::open(&path, &identity(), b"node-1").unwrap();
        }
        let err = PersistentKeyStore::open(&path, &identity(), b"node-2").unwrap_err();
        assert!(matches!(err, KeyManagerError::Unavailable));
    }
}
