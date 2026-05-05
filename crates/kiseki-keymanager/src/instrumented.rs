//! Instrumented `KeyManagerOps` wrapper — records prometheus metrics
//! and emits tracing spans on every call.
//!
//! Same pattern as `kiseki_log::InstrumentedLogOps`: wrap the
//! production `Arc<dyn KeyManagerOps>` so observability lives in
//! one place rather than across every backend (`MemKeyStore`,
//! `PersistentKeyStore`, `RaftKeyStore`).

use std::sync::Arc;
use std::time::Instant;

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::metrics::{outcome, KeyManagerMetrics};

/// `KeyManagerOps` wrapper that records `KeyManagerMetrics` and
/// emits tracing spans on every call.
pub struct InstrumentedKeyManager {
    inner: Arc<dyn KeyManagerOps>,
    metrics: Arc<KeyManagerMetrics>,
}

impl InstrumentedKeyManager {
    /// Build a new wrapper.
    pub fn new(inner: Arc<dyn KeyManagerOps>, metrics: Arc<KeyManagerMetrics>) -> Self {
        Self { inner, metrics }
    }
}

#[tonic::async_trait]
impl KeyManagerOps for InstrumentedKeyManager {
    // Hot path — `fetch_master_key` runs per chunk encrypt/decrypt
    // on the data path. `level = "debug"` short-circuits span +
    // field evaluation at production INFO/WARN, so the only paid
    // cost is the metric record (atomic counter + histogram
    // observe). Rare ops (`rotate`, `mark_migration_complete`)
    // stay at INFO so the production trace stream sees them.
    #[tracing::instrument(level = "debug", skip(self), fields(epoch = epoch.0))]
    async fn fetch_master_key(
        &self,
        epoch: KeyEpoch,
    ) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
        let started = Instant::now();
        let result = self.inner.fetch_master_key(epoch).await;
        let label = match &result {
            Ok(_) => outcome::OK,
            Err(KeyManagerError::EpochNotFound(_)) => outcome::NOT_FOUND,
            Err(_) => outcome::UNAVAILABLE,
        };
        self.metrics.record_fetch(label, started.elapsed());
        result
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        let result = self.inner.current_epoch().await;
        if let Ok(e) = &result {
            self.metrics
                .current_epoch
                .set(i64::try_from(e.0).unwrap_or(i64::MAX));
        }
        result
    }

    #[tracing::instrument(skip(self))]
    async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
        let result = self.inner.rotate().await;
        if let Ok(e) = &result {
            self.metrics.rotation_total.inc();
            self.metrics
                .current_epoch
                .set(i64::try_from(e.0).unwrap_or(i64::MAX));
            tracing::info!(new_epoch = e.0, "key manager: rotation completed",);
        }
        result
    }

    #[tracing::instrument(skip(self), fields(epoch = epoch.0))]
    async fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        let result = self.inner.mark_migration_complete(epoch).await;
        if result.is_ok() {
            self.metrics.migration_complete_total.inc();
        }
        result
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn list_epochs(&self) -> Vec<EpochInfo> {
        let epochs = self.inner.list_epochs().await;
        self.metrics
            .epoch_count
            .set(i64::try_from(epochs.len()).unwrap_or(i64::MAX));
        epochs
    }
}
