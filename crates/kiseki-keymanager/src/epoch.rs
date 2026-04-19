//! Epoch management — create, rotate, retain master keys.
//!
//! The key manager stores one master key per epoch. During rotation,
//! two epochs coexist (I-K6). Old epoch keys are retained until all
//! chunks encrypted under that epoch have been re-wrapped.

use std::sync::Arc;

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
/// All methods are async to support Raft-backed implementations where
/// writes go through `raft.client_write().await`. In-memory
/// implementations trivially wrap sync code.
///
/// All methods take `&self` — implementations use interior mutability.
///
/// `fetch_master_key` returns `Arc<SystemMasterKey>` (not a reference)
/// because Raft-backed stores cannot return borrows into replicated
/// state.
///
/// Implementations: `MemKeyStore` (in-memory, for testing),
/// `RaftKeyStore` (Raft-backed, production).
#[tonic::async_trait]
pub trait KeyManagerOps: Send + Sync {
    /// Fetch the master key for a given epoch.
    async fn fetch_master_key(
        &self,
        epoch: KeyEpoch,
    ) -> Result<Arc<SystemMasterKey>, KeyManagerError>;

    /// Get the current (latest) epoch.
    async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError>;

    /// Rotate the system master key — creates a new epoch with a fresh
    /// CSPRNG-generated master key.
    async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError>;

    /// Mark an old epoch's migration as complete.
    async fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError>;

    /// List all epochs and their status.
    async fn list_epochs(&self) -> Vec<EpochInfo>;
}
