//! Step definitions for backup-and-restore.feature (ADR-016, Phase 14d).
//!
//! Drives the production [`kiseki_backup::BackupManager`] against either
//! [`FileSystemBackupBackend`] or [`S3BackupBackend`] (talking to an
//! in-process axum mock). No mocks of the manager itself — the trait
//! is the seam, and these scenarios prove it isolates the storage layer
//! the way ADR-016 requires.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, put},
    Router,
};
use cucumber::{given, then, when};
use kiseki_backup::s3::{S3BackendConfig, S3BackupBackend};
use kiseki_backup::{
    BackupConfig, BackupError, BackupManager, FileSystemBackupBackend, ObjectBackupBackend,
    ShardSnapshot,
};
use tokio::net::TcpListener;

use crate::KisekiWorld;

// ---------------------------------------------------------------------------
// In-process axum-backed mock S3 — matches the unit-test mock in
// `kiseki_backup::s3::tests`. Only the verbs we exercise: PUT, GET,
// DELETE per key, GET ?list-type=2 for listing.
// ---------------------------------------------------------------------------

type MockStore = Arc<Mutex<HashMap<String, Vec<u8>>>>;

async fn handle_put(
    State(store): State<MockStore>,
    Path((_bucket, key)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> StatusCode {
    store.lock().unwrap().insert(key, body.to_vec());
    StatusCode::OK
}

async fn handle_get(
    State(store): State<MockStore>,
    Path((_bucket, key)): Path<(String, String)>,
) -> Result<Vec<u8>, StatusCode> {
    store
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

async fn handle_delete(
    State(store): State<MockStore>,
    Path((_bucket, key)): Path<(String, String)>,
) -> StatusCode {
    if store.lock().unwrap().remove(&key).is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn handle_list(
    State(store): State<MockStore>,
    Path(_bucket): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> String {
    use std::fmt::Write as _;
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let keys: Vec<String> = store
        .lock()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    let mut xml = String::from(
        r#"<?xml version="1.0"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
    );
    for k in keys {
        let _ = write!(xml, "<Contents><Key>{k}</Key></Contents>");
    }
    xml.push_str("</ListBucketResult>");
    xml
}

async fn spawn_mock_s3() -> (String, tokio::task::JoinHandle<()>) {
    let store: MockStore = Arc::new(Mutex::new(HashMap::new()));
    let app = Router::new()
        .route("/{bucket}", get(handle_list))
        .route(
            "/{bucket}/{*key}",
            put(handle_put).get(handle_get).delete(handle_delete),
        )
        .with_state(store);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

// ---------------------------------------------------------------------------
// Background
// ---------------------------------------------------------------------------

#[given("a backup-capable cluster")]
async fn given_capable_cluster(w: &mut KisekiWorld) {
    // Reset backup state — defensive; cucumber should give a fresh world per
    // scenario but this makes step ordering robust.
    w.backup.manager = None;
    w.backup.backend = None;
    w.backup.fs_dir = None;
    w.backup.s3_endpoint = None;
    w.backup.staged_shards.clear();
    w.backup.last_snapshot = None;
    w.backup.last_restored_shards = None;
    w.backup.last_snapshot_listing.clear();
    w.backup.last_error = None;
}

// ---------------------------------------------------------------------------
// Backend setup
// ---------------------------------------------------------------------------

#[given("a filesystem backup backend is configured")]
async fn given_fs_backend(w: &mut KisekiWorld) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend: Arc<dyn ObjectBackupBackend> =
        Arc::new(FileSystemBackupBackend::new(dir.path().to_path_buf()).expect("fs backend"));
    let mgr = Arc::new(BackupManager::new(
        Arc::clone(&backend),
        BackupConfig {
            include_data: true,
            retention_days: 7,
        },
    ));
    w.backup.fs_dir = Some(dir);
    w.backup.backend = Some(backend);
    w.backup.manager = Some(mgr);
}

#[given("an S3-compatible backup backend is configured")]
async fn given_s3_backend(w: &mut KisekiWorld) {
    let (endpoint, handle) = spawn_mock_s3().await;
    let backend: Arc<dyn ObjectBackupBackend> = Arc::new(
        S3BackupBackend::new(S3BackendConfig {
            endpoint: endpoint.clone(),
            region: "us-east-1".into(),
            bucket: "kiseki-backups".into(),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnEHK/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        })
        .expect("s3 backend"),
    );
    let mgr = Arc::new(BackupManager::new(
        Arc::clone(&backend),
        BackupConfig {
            include_data: true,
            retention_days: 7,
        },
    ));
    w.backup.s3_endpoint = Some(endpoint);
    w.backup.s3_task = Some(handle);
    w.backup.backend = Some(backend);
    w.backup.manager = Some(mgr);
}

// ---------------------------------------------------------------------------
// Stage shards
// ---------------------------------------------------------------------------

#[given(regex = r#"^shard "([^"]+)" with (\d+) bytes of metadata$"#)]
async fn given_shard_with_metadata(w: &mut KisekiWorld, name: String, n: usize) {
    w.backup.staged_shards.push(ShardSnapshot {
        shard_id: name,
        metadata: vec![0xab; n],
        data: None,
    });
}

#[given(regex = r#"^shard "([^"]+)" with (\d+) bytes of metadata and chunk data$"#)]
async fn given_shard_with_metadata_and_data(w: &mut KisekiWorld, name: String, n: usize) {
    w.backup.staged_shards.push(ShardSnapshot {
        shard_id: name,
        metadata: vec![0xab; n],
        data: Some(b"chunk-bytes".to_vec()),
    });
}

// ---------------------------------------------------------------------------
// Concurrency setup
// ---------------------------------------------------------------------------

#[given("a backup is already in progress")]
async fn given_backup_in_progress(w: &mut KisekiWorld) {
    let mgr = w
        .backup
        .manager
        .as_ref()
        .expect("backup manager configured");
    mgr.force_in_progress(true);
}

#[given(regex = r"^the operator created (\d+) snapshots$")]
async fn given_operator_created_snapshots(w: &mut KisekiWorld, n: usize) {
    let mgr = Arc::clone(
        w.backup
            .manager
            .as_ref()
            .expect("backup manager configured"),
    );
    let shards = vec![ShardSnapshot {
        shard_id: "s-fixture".into(),
        metadata: br#"{"v":1}"#.to_vec(),
        data: None,
    }];
    for _ in 0..n {
        mgr.create_snapshot(&shards).await.expect("seed snapshot");
    }
}

// ---------------------------------------------------------------------------
// When — operator triggers actions
// ---------------------------------------------------------------------------

#[when("the operator triggers a backup")]
async fn when_trigger_backup(w: &mut KisekiWorld) {
    let mgr = Arc::clone(
        w.backup
            .manager
            .as_ref()
            .expect("backup manager configured"),
    );
    let shards = std::mem::take(&mut w.backup.staged_shards);
    match mgr.create_snapshot(&shards).await {
        Ok(snap) => {
            w.backup.last_snapshot = Some(snap);
        }
        Err(e) => {
            // Re-stage so a follow-up When can retry from the same shards.
            w.backup.staged_shards = shards;
            w.backup.last_error = Some(match e {
                BackupError::InProgress => "InProgress".into(),
                other => format!("{other:?}"),
            });
        }
    }
}

#[when("the operator restores the most recent snapshot")]
async fn when_restore_last(w: &mut KisekiWorld) {
    let mgr = Arc::clone(
        w.backup
            .manager
            .as_ref()
            .expect("backup manager configured"),
    );
    let id = w
        .backup
        .last_snapshot
        .as_ref()
        .expect("a snapshot was created")
        .snapshot_id
        .clone();
    let shards = mgr
        .restore_snapshot(&id)
        .await
        .expect("restore should succeed");
    w.backup.last_restored_shards = Some(shards);
}

#[when(regex = r"^retention is enforced with a (\d+)-day window$")]
async fn when_enforce_retention(w: &mut KisekiWorld, days: u32) {
    let mgr = Arc::clone(
        w.backup
            .manager
            .as_ref()
            .expect("backup manager configured"),
    );
    let n = mgr.cleanup_old(days).await;
    w.backup.last_snapshot = None; // legitimately gone now
    w.backup.last_snapshot_listing = mgr.list_snapshots().await;
    // Stash the deleted count in the error slot so a Then can compare.
    w.backup.last_error = Some(format!("cleaned={n}"));
}

// ---------------------------------------------------------------------------
// Then — assertions
// ---------------------------------------------------------------------------

#[then("a snapshot tarball lands in the backup directory")]
async fn then_tarball_lands(w: &mut KisekiWorld) {
    let snap = w
        .backup
        .last_snapshot
        .as_ref()
        .expect("a snapshot was created");
    let backend = w.backup.backend.as_ref().expect("backend configured");
    let blob = backend
        .get_blob(&snap.tarball_key)
        .await
        .expect("backend get")
        .expect("tarball must exist");
    assert!(!blob.is_empty(), "tarball should have bytes");
}

#[then(regex = r"^the snapshot manifest records (\d+) shards$")]
async fn then_manifest_shard_count(w: &mut KisekiWorld, n: usize) {
    let snap = w
        .backup
        .last_snapshot
        .as_ref()
        .expect("a snapshot was created");
    assert_eq!(snap.shard_count, n, "snapshot.shard_count");
}

#[then(regex = r"^(\d+) shard is recovered with metadata and chunk data intact$")]
async fn then_shard_recovered(w: &mut KisekiWorld, n: usize) {
    let restored = w
        .backup
        .last_restored_shards
        .as_ref()
        .expect("restore was issued");
    assert_eq!(restored.len(), n);
    let s = &restored[0];
    assert_eq!(s.shard_id, "alpha");
    assert!(!s.metadata.is_empty());
    assert_eq!(s.data.as_deref(), Some(&b"chunk-bytes"[..]));
}

#[then("the snapshot tarball is reachable through the S3 backend")]
async fn then_tarball_reachable_s3(w: &mut KisekiWorld) {
    let snap = w
        .backup
        .last_snapshot
        .as_ref()
        .expect("a snapshot was created");
    let backend = w.backup.backend.as_ref().expect("backend configured");
    let blob = backend
        .get_blob(&snap.tarball_key)
        .await
        .expect("backend get")
        .expect("tarball must exist on S3");
    assert!(!blob.is_empty());
}

#[then("the manifest is reachable through the S3 backend")]
async fn then_manifest_reachable_s3(w: &mut KisekiWorld) {
    let snap = w
        .backup
        .last_snapshot
        .as_ref()
        .expect("a snapshot was created");
    let backend = w.backup.backend.as_ref().expect("backend configured");
    let blob = backend
        .get_blob(&snap.manifest_key)
        .await
        .expect("backend get")
        .expect("manifest must exist on S3");
    let s = std::str::from_utf8(&blob).expect("manifest is utf8");
    assert!(s.contains(&snap.snapshot_id), "manifest references id");
}

#[then(regex = r"^listing snapshots returns (\d+) entries$")]
async fn then_listing_returns_n(w: &mut KisekiWorld, n: usize) {
    let mgr = Arc::clone(
        w.backup
            .manager
            .as_ref()
            .expect("backup manager configured"),
    );
    let list = mgr.list_snapshots().await;
    assert_eq!(list.len(), n, "list_snapshots count");
    w.backup.last_snapshot_listing = list;
}

#[then("the second backup is rejected with InProgress")]
async fn then_second_rejected(w: &mut KisekiWorld) {
    let err = w
        .backup
        .last_error
        .as_deref()
        .expect("a backup attempt failed");
    assert_eq!(err, "InProgress");
}

#[then("the in-progress flag can be cleared and a new backup succeeds")]
async fn then_clear_and_retry(w: &mut KisekiWorld) {
    let mgr = Arc::clone(
        w.backup
            .manager
            .as_ref()
            .expect("backup manager configured"),
    );
    mgr.force_in_progress(false);
    let shards = std::mem::take(&mut w.backup.staged_shards);
    let snap = mgr
        .create_snapshot(&shards)
        .await
        .expect("retry must succeed");
    w.backup.last_snapshot = Some(snap);
}

#[then(regex = r"^all (\d+) snapshots are deleted$")]
async fn then_all_deleted(w: &mut KisekiWorld, expected: usize) {
    let cleaned = w
        .backup
        .last_error
        .as_deref()
        .and_then(|s| s.strip_prefix("cleaned="))
        .and_then(|s| s.parse::<usize>().ok())
        .expect("retention step recorded a cleaned count");
    assert_eq!(cleaned, expected, "cleanup count");
    assert!(
        w.backup.last_snapshot_listing.is_empty(),
        "listing should be empty after retention 0",
    );
}
