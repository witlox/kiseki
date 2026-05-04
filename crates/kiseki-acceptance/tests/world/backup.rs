#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Backup/restore state (ADR-016, Phase 14d).

pub struct BackupState {
    pub manager: Option<std::sync::Arc<kiseki_backup::BackupManager>>,
    pub fs_dir: Option<tempfile::TempDir>,
    pub backend: Option<std::sync::Arc<dyn kiseki_backup::ObjectBackupBackend>>,
    pub s3_endpoint: Option<String>,
    pub staged_shards: Vec<kiseki_backup::ShardSnapshot>,
    pub last_snapshot: Option<kiseki_backup::BackupSnapshot>,
    pub last_restored_shards: Option<Vec<kiseki_backup::ShardSnapshot>>,
    pub last_snapshot_listing: Vec<kiseki_backup::BackupSnapshot>,
    pub last_error: Option<String>,
    pub s3_task: Option<tokio::task::JoinHandle<()>>,
}

impl BackupState {
    pub fn new() -> Self {
        Self {
            manager: None,
            fs_dir: None,
            backend: None,
            s3_endpoint: None,
            staged_shards: Vec::new(),
            last_snapshot: None,
            last_restored_shards: None,
            last_snapshot_listing: Vec::new(),
            last_error: None,
            s3_task: None,
        }
    }
}

impl Drop for BackupState {
    fn drop(&mut self) {
        if let Some(h) = self.s3_task.take() {
            h.abort();
        }
    }
}
