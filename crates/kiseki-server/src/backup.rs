//! Runtime wiring for the cluster backup manager (ADR-016).
//!
//! The implementation lives in the `kiseki-backup` crate so the BDD
//! suite can drive it directly. This module owns only the bits that
//! are server-specific:
//!
//! - parsing [`BackupSettings`] from the env-driven [`crate::config`]
//! - the process-wide `OnceLock<Arc<BackupManager>>` that the admin
//!   gRPC service reads from
//! - the periodic `cleanup_old` task

use std::io;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

pub use kiseki_backup::s3::{S3BackendConfig, S3BackupBackend};
pub use kiseki_backup::{
    BackupConfig, BackupError, BackupManager, FileSystemBackupBackend, ObjectBackupBackend,
    ShardSnapshot,
};

use crate::config::{BackupBackend as CfgBackend, BackupSettings};

static RUNTIME_MANAGER: OnceLock<Arc<BackupManager>> = OnceLock::new();

/// The process-wide [`BackupManager`], if backups are enabled.
///
/// Returns `None` when `KISEKI_BACKUP_BACKEND` is unset (default), or
/// before [`init_runtime_backup_manager`] has been called.
#[must_use]
pub fn runtime_backup_manager() -> Option<Arc<BackupManager>> {
    RUNTIME_MANAGER.get().cloned()
}

/// Build the runtime [`BackupManager`] from the parsed [`BackupSettings`]
/// and stash it for [`runtime_backup_manager`]. Spawns a periodic
/// `cleanup_old` task on the current tokio runtime.
///
/// Returns the manager so the caller can also use it directly.
pub fn init_runtime_backup_manager(settings: &BackupSettings) -> io::Result<Arc<BackupManager>> {
    let backend: Arc<dyn ObjectBackupBackend> = match &settings.backend {
        CfgBackend::FileSystem { dir } => Arc::new(FileSystemBackupBackend::new(dir.clone())?),
        CfgBackend::S3 {
            endpoint,
            region,
            bucket,
            access_key_id,
            secret_access_key,
        } => Arc::new(S3BackupBackend::new(S3BackendConfig {
            endpoint: endpoint.clone(),
            region: region.clone(),
            bucket: bucket.clone(),
            access_key_id: access_key_id.clone(),
            secret_access_key: secret_access_key.clone(),
        })?),
    };
    let mgr = Arc::new(BackupManager::new(
        backend,
        BackupConfig {
            include_data: settings.include_data,
            retention_days: settings.retention_days,
        },
    ));

    // Idempotent: a second call with a different backend would be a
    // misconfiguration; we keep the first one and warn so the operator
    // sees the conflict in logs.
    if RUNTIME_MANAGER.set(Arc::clone(&mgr)).is_err() {
        tracing::warn!("backup: init_runtime_backup_manager called twice — keeping the first");
        return Ok(runtime_backup_manager().expect("set returned Err so a value exists"));
    }

    let cleanup_mgr = Arc::clone(&mgr);
    let interval = settings.cleanup_interval_secs;
    let retention = settings.retention_days;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval));
        // Skip the first immediate tick — we just started, nothing to clean.
        tick.tick().await;
        loop {
            tick.tick().await;
            let n = cleanup_mgr.cleanup_old(retention).await;
            if n > 0 {
                tracing::info!(deleted = n, "backup: cleanup removed expired snapshots");
            }
        }
    });

    Ok(mgr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runtime wiring: `init_runtime_backup_manager` builds a working
    /// manager from a parsed FS config, registers it in the `OnceLock`,
    /// and the cleanup task is reachable. Round-tripping a snapshot via
    /// the returned manager proves the full chain is alive.
    #[tokio::test]
    async fn runtime_wiring_builds_working_fs_manager() {
        let dir = tempfile::tempdir().unwrap();
        let settings = BackupSettings {
            backend: CfgBackend::FileSystem {
                dir: dir.path().to_path_buf(),
            },
            retention_days: 7,
            include_data: false,
            cleanup_interval_secs: 86_400,
        };
        let mgr = init_runtime_backup_manager(&settings).expect("init");
        let shards = vec![ShardSnapshot {
            shard_id: "s1".into(),
            metadata: br#"{"v":1}"#.to_vec(),
            data: None,
        }];
        let snap = mgr.create_snapshot(&shards).await.unwrap();
        let restored = mgr.restore_snapshot(&snap.snapshot_id).await.unwrap();
        assert_eq!(restored.len(), 1);
        let from_handle = runtime_backup_manager().expect("handle");
        assert!(Arc::ptr_eq(&mgr, &from_handle));
    }
}
