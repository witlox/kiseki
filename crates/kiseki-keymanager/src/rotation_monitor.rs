//! Background key rotation monitor.
//!
//! Polls the current epoch's age and triggers rotation when the TTL
//! expires. After rotation, starts a background rewrap of all chunks
//! encrypted under the old epoch.

use std::sync::Arc;
use std::time::Duration;

use crate::epoch::KeyManagerOps;
use crate::rewrap_worker::RewrapProgress;

/// Configuration for the rotation monitor.
#[derive(Clone, Debug)]
pub struct RotationConfig {
    /// How long an epoch lives before rotation. Default: 90 days.
    pub epoch_ttl: Duration,
    /// How often to check epoch age. Default: 60 seconds.
    pub check_interval: Duration,
}

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            epoch_ttl: Duration::from_secs(90 * 24 * 3600), // 90 days
            check_interval: Duration::from_secs(60),
        }
    }
}

/// Run the rotation monitor as a background task.
///
/// This function runs forever (until the task is cancelled). It
/// checks the current epoch's age every `check_interval` and
/// triggers rotation when `epoch_ttl` is exceeded.
///
/// After rotation, logs the event and returns the new epoch.
/// Rewrap orchestration is handled by the caller (server runtime).
pub async fn run_rotation_monitor<K: KeyManagerOps>(
    key_manager: Arc<K>,
    config: RotationConfig,
    on_rotation: impl Fn(kiseki_common::tenancy::KeyEpoch, kiseki_common::tenancy::KeyEpoch)
        + Send
        + Sync
        + 'static,
) {
    // Track when we last rotated (or when the monitor started).
    let mut last_rotation = std::time::Instant::now();
    let mut current_epoch = match key_manager.current_epoch().await {
        Ok(e) => {
            tracing::info!(epoch = e.0, "rotation monitor started");
            e
        }
        Err(e) => {
            tracing::error!(error = %e, "rotation monitor: failed to get current epoch");
            return;
        }
    };

    loop {
        tokio::time::sleep(config.check_interval).await;

        let age = last_rotation.elapsed();
        if age < config.epoch_ttl {
            continue;
        }

        // TTL exceeded — rotate.
        tracing::info!(
            old_epoch = current_epoch.0,
            age_secs = age.as_secs(),
            ttl_secs = config.epoch_ttl.as_secs(),
            "epoch TTL exceeded, rotating key"
        );

        match key_manager.rotate().await {
            Ok(new_epoch) => {
                tracing::info!(
                    old_epoch = current_epoch.0,
                    new_epoch = new_epoch.0,
                    "key rotation complete"
                );
                on_rotation(current_epoch, new_epoch);
                current_epoch = new_epoch;
                last_rotation = std::time::Instant::now();
            }
            Err(e) => {
                tracing::error!(error = %e, "key rotation failed");
                // Retry on next check interval.
            }
        }
    }
}

/// Placeholder for rewrap progress tracking after rotation.
///
/// The actual rewrap is performed by `run_rewrap_batch` from
/// `rewrap_worker.rs`. This function creates a progress tracker
/// and logs completion.
#[must_use]
pub fn new_rewrap_progress() -> Arc<RewrapProgress> {
    Arc::new(RewrapProgress::new(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::{EpochInfo, KeyManagerOps};
    use crate::error::KeyManagerError;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_crypto::keys::SystemMasterKey;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Mock key manager that tracks rotation calls.
    struct MockKeyManager {
        epoch: AtomicU64,
    }

    impl MockKeyManager {
        fn new() -> Self {
            Self {
                epoch: AtomicU64::new(1),
            }
        }
    }

    #[tonic::async_trait]
    impl KeyManagerOps for MockKeyManager {
        async fn fetch_master_key(
            &self,
            epoch: KeyEpoch,
        ) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
            Err(KeyManagerError::EpochNotFound(epoch))
        }
        async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
            Ok(KeyEpoch(self.epoch.load(Ordering::Relaxed)))
        }
        async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
            let new = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(KeyEpoch(new))
        }
        async fn mark_migration_complete(&self, _epoch: KeyEpoch) -> Result<(), KeyManagerError> {
            Ok(())
        }
        async fn list_epochs(&self) -> Vec<EpochInfo> {
            vec![]
        }
    }

    #[tokio::test]
    async fn rotation_triggers_after_ttl() {
        let km = Arc::new(MockKeyManager::new());
        let rotated = Arc::new(AtomicU64::new(0));
        let rotated_clone = Arc::clone(&rotated);

        let config = RotationConfig {
            epoch_ttl: Duration::from_millis(50),
            check_interval: Duration::from_millis(20),
        };

        let handle = tokio::spawn(run_rotation_monitor(
            Arc::clone(&km),
            config,
            move |old, new| {
                assert_eq!(old.0, 1);
                assert_eq!(new.0, 2);
                rotated_clone.fetch_add(1, Ordering::Relaxed);
            },
        ));

        // Wait for at least one rotation.
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.abort();

        assert!(
            rotated.load(Ordering::Relaxed) >= 1,
            "should have rotated at least once"
        );
    }

    #[tokio::test]
    async fn no_rotation_before_ttl() {
        let km = Arc::new(MockKeyManager::new());
        let rotated = Arc::new(AtomicU64::new(0));
        let rotated_clone = Arc::clone(&rotated);

        let config = RotationConfig {
            epoch_ttl: Duration::from_secs(3600), // 1 hour — won't trigger
            check_interval: Duration::from_millis(20),
        };

        let handle = tokio::spawn(run_rotation_monitor(
            Arc::clone(&km),
            config,
            move |_, _| {
                rotated_clone.fetch_add(1, Ordering::Relaxed);
            },
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();

        assert_eq!(
            rotated.load(Ordering::Relaxed),
            0,
            "should not rotate before TTL"
        );
    }

    #[test]
    fn rewrap_progress_creation() {
        let progress = new_rewrap_progress();
        assert_eq!(progress.total.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn multiple_rotations_over_time() {
        // Verify that the monitor can trigger multiple rotations when
        // the TTL is very short.
        let km = Arc::new(MockKeyManager::new());
        let rotated = Arc::new(AtomicU64::new(0));
        let rotated_clone = Arc::clone(&rotated);

        let config = RotationConfig {
            epoch_ttl: Duration::from_millis(30),
            check_interval: Duration::from_millis(10),
        };

        let handle = tokio::spawn(run_rotation_monitor(
            Arc::clone(&km),
            config,
            move |_old, _new| {
                rotated_clone.fetch_add(1, Ordering::Relaxed);
            },
        ));

        // Wait long enough for at least 2 rotations.
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.abort();

        let count = rotated.load(Ordering::Relaxed);
        assert!(
            count >= 2,
            "should have rotated at least twice, got {count}"
        );
    }
}
