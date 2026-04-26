//! gRPC `AdminService` implementation (Phase 14d step 4, ADR-016).
//!
//! Surfaces backup operations over gRPC for the `kiseki-admin` CLI.
//! When backups are not configured (no `KISEKI_BACKUP_BACKEND` env
//! var), every RPC returns `UNAVAILABLE` so the operator sees a
//! useful message — not an opaque internal error.

use std::sync::Arc;

use async_trait::async_trait;
use kiseki_proto::v1 as pb;
use kiseki_proto::v1::admin_service_server::AdminService;
use tonic::{Request, Response, Status};

use crate::backup::{runtime_backup_manager, BackupError, BackupManager, ShardSnapshot};

/// Source of [`ShardSnapshot`]s to feed [`BackupManager::create_snapshot`].
///
/// Pluggable so the runtime can swap implementations: in-memory, redb,
/// or test stubs. The default [`EmptyShardProvider`] yields no shards
/// — useful for wiring tests and for runtimes that haven't yet
/// enumerated their state.
#[async_trait]
pub trait ShardSnapshotProvider: Send + Sync {
    /// Snapshot every live shard. Order unspecified.
    async fn collect(&self) -> Vec<ShardSnapshot>;
}

/// Returns no shards. Used until the runtime wires a real shard walker.
pub struct EmptyShardProvider;

#[async_trait]
impl ShardSnapshotProvider for EmptyShardProvider {
    async fn collect(&self) -> Vec<ShardSnapshot> {
        Vec::new()
    }
}

/// gRPC handler. Built from an optional [`BackupManager`] and a
/// [`ShardSnapshotProvider`]. Pass `None` for the manager to model "backups
/// not configured" — every RPC returns `UNAVAILABLE` in that mode.
pub struct AdminGrpc {
    manager: Option<Arc<BackupManager>>,
    shards: Arc<dyn ShardSnapshotProvider>,
}

impl AdminGrpc {
    /// Build from explicit dependencies — most useful for tests.
    #[must_use]
    pub fn new(
        manager: Option<Arc<BackupManager>>,
        shards: Arc<dyn ShardSnapshotProvider>,
    ) -> Self {
        Self { manager, shards }
    }

    /// Build from the process-wide runtime handle, with an empty shard
    /// provider. The runtime calls this once after the backup manager
    /// is initialised.
    #[must_use]
    pub fn from_runtime() -> Self {
        Self::new(runtime_backup_manager(), Arc::new(EmptyShardProvider))
    }

    fn manager_or_unavailable(&self) -> Result<Arc<BackupManager>, Status> {
        self.manager.clone().ok_or_else(|| {
            Status::unavailable(
                "backups are not configured — set KISEKI_BACKUP_BACKEND=fs|s3 on the server",
            )
        })
    }
}

#[async_trait]
impl AdminService for AdminGrpc {
    async fn create_snapshot(
        &self,
        _request: Request<pb::CreateSnapshotRequest>,
    ) -> Result<Response<pb::CreateSnapshotResponse>, Status> {
        let mgr = self.manager_or_unavailable()?;
        let shards = self.shards.collect().await;
        let snap = mgr
            .create_snapshot(&shards)
            .await
            .map_err(backup_error_to_status)?;
        Ok(Response::new(pb::CreateSnapshotResponse {
            snapshot_id: snap.snapshot_id,
            metadata_bytes: snap.metadata_bytes,
            data_bytes: snap.data_bytes,
            shard_count: snap.shard_count as u64,
            created_at: snap.created_at,
            elapsed_millis: u64::try_from(snap.elapsed.as_millis()).unwrap_or(u64::MAX),
        }))
    }

    async fn restore_snapshot(
        &self,
        request: Request<pb::RestoreSnapshotRequest>,
    ) -> Result<Response<pb::RestoreSnapshotResponse>, Status> {
        let mgr = self.manager_or_unavailable()?;
        let id = request.into_inner().snapshot_id;
        if id.is_empty() {
            return Err(Status::invalid_argument("snapshot_id is required"));
        }
        let shards = mgr
            .restore_snapshot(&id)
            .await
            .map_err(backup_error_to_status)?;
        Ok(Response::new(pb::RestoreSnapshotResponse {
            shard_count: shards.len() as u64,
        }))
    }

    async fn list_snapshots(
        &self,
        _request: Request<pb::ListSnapshotsRequest>,
    ) -> Result<Response<pb::ListSnapshotsResponse>, Status> {
        let mgr = self.manager_or_unavailable()?;
        let snapshots = mgr
            .list_snapshots()
            .await
            .into_iter()
            .map(|s| pb::SnapshotInfo {
                snapshot_id: s.snapshot_id,
                metadata_bytes: s.metadata_bytes,
                data_bytes: s.data_bytes,
                shard_count: s.shard_count as u64,
                created_at: s.created_at,
            })
            .collect();
        Ok(Response::new(pb::ListSnapshotsResponse { snapshots }))
    }
}

fn backup_error_to_status(e: BackupError) -> Status {
    match e {
        BackupError::InProgress => Status::failed_precondition("backup already in progress"),
        BackupError::SnapshotNotFound(id) => Status::not_found(format!("snapshot not found: {id}")),
        BackupError::RestoreFailed(msg) => Status::internal(format!("restore failed: {msg}")),
        BackupError::Io(e) => Status::internal(format!("backup I/O: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{BackupConfig, FileSystemBackupBackend, ObjectBackupBackend};

    fn fs_manager() -> (tempfile::TempDir, Arc<BackupManager>) {
        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn ObjectBackupBackend> =
            Arc::new(FileSystemBackupBackend::new(dir.path().to_path_buf()).unwrap());
        let mgr = Arc::new(BackupManager::new(
            backend,
            BackupConfig {
                include_data: false,
                retention_days: 7,
            },
        ));
        (dir, mgr)
    }

    struct StubProvider(Vec<ShardSnapshot>);

    #[async_trait]
    impl ShardSnapshotProvider for StubProvider {
        async fn collect(&self) -> Vec<ShardSnapshot> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn create_then_list_then_restore_round_trips() {
        let (_d, mgr) = fs_manager();
        let provider = Arc::new(StubProvider(vec![ShardSnapshot {
            shard_id: "s1".into(),
            metadata: br#"{"v":1}"#.to_vec(),
            data: None,
        }]));
        let svc = AdminGrpc::new(Some(mgr), provider);

        let create = svc
            .create_snapshot(Request::new(pb::CreateSnapshotRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(create.shard_count, 1);
        assert!(create.metadata_bytes > 0);

        let list = svc
            .list_snapshots(Request::new(pb::ListSnapshotsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(list.snapshots.len(), 1);
        assert_eq!(list.snapshots[0].snapshot_id, create.snapshot_id);

        let restore = svc
            .restore_snapshot(Request::new(pb::RestoreSnapshotRequest {
                snapshot_id: create.snapshot_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(restore.shard_count, 1);
    }

    #[tokio::test]
    async fn create_without_manager_returns_unavailable() {
        let svc = AdminGrpc::new(None, Arc::new(EmptyShardProvider));
        let err = svc
            .create_snapshot(Request::new(pb::CreateSnapshotRequest {}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn restore_unknown_id_returns_not_found() {
        let (_d, mgr) = fs_manager();
        let svc = AdminGrpc::new(Some(mgr), Arc::new(EmptyShardProvider));
        let err = svc
            .restore_snapshot(Request::new(pb::RestoreSnapshotRequest {
                snapshot_id: "nope".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn restore_empty_id_returns_invalid_argument() {
        let (_d, mgr) = fs_manager();
        let svc = AdminGrpc::new(Some(mgr), Arc::new(EmptyShardProvider));
        let err = svc
            .restore_snapshot(Request::new(pb::RestoreSnapshotRequest {
                snapshot_id: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn list_empty_when_no_snapshots() {
        let (_d, mgr) = fs_manager();
        let svc = AdminGrpc::new(Some(mgr), Arc::new(EmptyShardProvider));
        let list = svc
            .list_snapshots(Request::new(pb::ListSnapshotsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(list.snapshots.is_empty());
    }
}
