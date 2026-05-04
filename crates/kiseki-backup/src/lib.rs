#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Cluster backup and disaster recovery (ADR-016).
//!
//! Owns the [`ObjectBackupBackend`] trait, the [`BackupManager`], and
//! both reference implementations: [`FileSystemBackupBackend`] (local
//! directory) and [`s3::S3BackupBackend`] (S3-compatible object store).
//!
//! Snapshot layout in the backend:
//!
//! ```text
//! <snapshot_id>/manifest.json   — tiny JSON descriptor, cheap to list
//! <snapshot_id>/snapshot.tar    — single tarball of every shard's metadata + data
//! ```
//!
//! The tarball lets a snapshot be moved between backends as a single
//! object (S3 multipart upload, file copy, etc.) without per-shard
//! roundtrips.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tar::{Archive, Builder, Header};

pub mod s3;

// ---------------------------------------------------------------------------
// ObjectBackupBackend trait
// ---------------------------------------------------------------------------

/// Storage backend for backup snapshots. Implemented by
/// [`FileSystemBackupBackend`] (local directory) and [`s3::S3BackupBackend`]
/// (S3-compatible object store).
///
/// The interface is a deliberate minimum — no presigned URLs, no
/// multipart, no streaming. Snapshots are assumed to be small enough
/// to fit comfortably in memory; if that ever stops being true, add
/// streaming put/get without breaking this trait.
///
/// Async because the production impl is S3 (HTTP). The filesystem impl
/// happens to be cheap-sync but exposes async to keep one trait shape.
#[async_trait]
pub trait ObjectBackupBackend: Send + Sync {
    /// Store `bytes` at `key`, replacing any existing value.
    async fn put_blob(&self, key: &str, bytes: &[u8]) -> io::Result<()>;

    /// Retrieve the bytes at `key`. Returns `None` if no such key.
    async fn get_blob(&self, key: &str) -> io::Result<Option<Vec<u8>>>;

    /// List every key whose name starts with `prefix`. Order unspecified.
    async fn list_keys(&self, prefix: &str) -> io::Result<Vec<String>>;

    /// Delete `key`. Returns `true` if the key existed, `false` otherwise.
    async fn delete_blob(&self, key: &str) -> io::Result<bool>;
}

// ---------------------------------------------------------------------------
// FileSystemBackupBackend
// ---------------------------------------------------------------------------

/// Local-directory implementation of [`ObjectBackupBackend`]. The
/// `<root>` directory is treated as the bucket; keys map to relative
/// paths under it. Slashes in keys create subdirectories.
pub struct FileSystemBackupBackend {
    root: PathBuf,
}

impl FileSystemBackupBackend {
    /// Create a new filesystem backend rooted at `dir`. Creates the
    /// directory if it doesn't exist.
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { root: dir })
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl ObjectBackupBackend for FileSystemBackupBackend {
    async fn put_blob(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)
    }

    async fn get_blob(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        let path = self.path_for(key);
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn list_keys(&self, prefix: &str) -> io::Result<Vec<String>> {
        // The prefix may name an existing directory or be a path
        // fragment. Walk the root and filter — simpler than splitting.
        let mut keys = Vec::new();
        walk(&self.root, &self.root, &mut keys)?;
        Ok(keys.into_iter().filter(|k| k.starts_with(prefix)).collect())
    }

    async fn delete_blob(&self, key: &str) -> io::Result<bool> {
        let path = self.path_for(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            walk(root, &p, out)?;
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// Configuration for a [`BackupManager`].
pub struct BackupConfig {
    /// Whether to include chunk data (vs metadata-only) in snapshots.
    pub include_data: bool,
    /// Maximum snapshot age before [`BackupManager::cleanup_old`] removes it.
    pub retention_days: u32,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            include_data: false,
            retention_days: 7,
        }
    }
}

/// A completed backup snapshot.
#[derive(Clone, Debug)]
pub struct BackupSnapshot {
    /// Unique snapshot identifier (ISO timestamp + UUID).
    pub snapshot_id: String,
    /// Backend key for the tarball blob (`<snapshot_id>/snapshot.tar`).
    pub tarball_key: String,
    /// Backend key for the manifest blob (`<snapshot_id>/manifest.json`).
    pub manifest_key: String,
    /// Total metadata bytes written.
    pub metadata_bytes: u64,
    /// Total data bytes written (0 if metadata-only).
    pub data_bytes: u64,
    /// Number of shards captured.
    pub shard_count: usize,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Time elapsed during snapshot creation.
    pub elapsed: Duration,
}

/// Backup error.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// Another backup is already in progress.
    #[error("backup in progress")]
    InProgress,
    /// Snapshot not found.
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    /// Restore failed.
    #[error("restore failed: {0}")]
    RestoreFailed(String),
    /// I/O error from the backend.
    #[error("backup backend I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Shard metadata + optional data passed to [`BackupManager::create_snapshot`].
#[derive(Clone, Debug)]
pub struct ShardSnapshot {
    /// Shard identifier.
    pub shard_id: String,
    /// Serialised metadata (JSON bytes).
    pub metadata: Vec<u8>,
    /// Optional data payload.
    pub data: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// BackupManager
// ---------------------------------------------------------------------------

/// Backup manager — drives create / restore / list / delete operations
/// against an [`ObjectBackupBackend`].
pub struct BackupManager {
    backend: Arc<dyn ObjectBackupBackend>,
    config: BackupConfig,
    in_progress: AtomicBool,
}

impl BackupManager {
    /// Create a new manager around the given backend.
    #[must_use]
    pub fn new(backend: Arc<dyn ObjectBackupBackend>, config: BackupConfig) -> Self {
        Self {
            backend,
            config,
            in_progress: AtomicBool::new(false),
        }
    }

    /// Create a point-in-time snapshot from `shards`, store it via the
    /// backend as `<snapshot_id>/{manifest.json, snapshot.tar}`, and
    /// return the descriptor.
    ///
    /// Concurrent calls are rejected with [`BackupError::InProgress`].
    #[tracing::instrument(skip(self, shards), fields(shard_count = shards.len()))]
    pub async fn create_snapshot(
        &self,
        shards: &[ShardSnapshot],
    ) -> Result<BackupSnapshot, BackupError> {
        if self
            .in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::warn!("backup: create_snapshot rejected — another snapshot is in progress");
            return Err(BackupError::InProgress);
        }
        tracing::info!("backup: create_snapshot start");
        let result = self.do_create_snapshot(shards).await;
        self.in_progress.store(false, Ordering::SeqCst);
        match &result {
            Ok(s) => tracing::info!(
                snapshot_id = %s.snapshot_id,
                metadata_bytes = s.metadata_bytes,
                data_bytes = s.data_bytes,
                elapsed_ms = u64::try_from(s.elapsed.as_millis()).unwrap_or(u64::MAX),
                "backup: create_snapshot success",
            ),
            Err(e) => tracing::warn!(error = %e, "backup: create_snapshot failed"),
        }
        result
    }

    async fn do_create_snapshot(
        &self,
        shards: &[ShardSnapshot],
    ) -> Result<BackupSnapshot, BackupError> {
        let start = Instant::now();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp = chrono_lite_iso(now);
        let snapshot_id = format!("{timestamp}-{}", uuid::Uuid::new_v4());

        // Build tarball in memory — keeps the backend interface simple.
        let mut tarball: Vec<u8> = Vec::new();
        let mut metadata_bytes: u64 = 0;
        let mut data_bytes: u64 = 0;
        {
            let mut builder = Builder::new(&mut tarball);
            for shard in shards {
                append_blob(
                    &mut builder,
                    &format!("{}.meta.json", shard.shard_id),
                    &shard.metadata,
                )?;
                metadata_bytes += shard.metadata.len() as u64;
                if self.config.include_data {
                    if let Some(ref data) = shard.data {
                        append_blob(&mut builder, &format!("{}.data", shard.shard_id), data)?;
                        data_bytes += data.len() as u64;
                    }
                }
            }
            builder.finish()?;
        }

        let manifest = build_manifest(
            &snapshot_id,
            shards.len(),
            metadata_bytes,
            data_bytes,
            &timestamp,
        );

        let manifest_key = format!("{snapshot_id}/manifest.json");
        let tarball_key = format!("{snapshot_id}/snapshot.tar");

        // Write tarball first; if that fails the manifest never lands,
        // so list_snapshots won't surface a half-written snapshot.
        self.backend.put_blob(&tarball_key, &tarball).await?;
        self.backend
            .put_blob(&manifest_key, manifest.as_bytes())
            .await?;

        Ok(BackupSnapshot {
            snapshot_id,
            tarball_key,
            manifest_key,
            metadata_bytes,
            data_bytes,
            shard_count: shards.len(),
            created_at: timestamp,
            elapsed: start.elapsed(),
        })
    }

    /// Restore a snapshot by id — downloads the tarball, parses every
    /// `<shard>.meta.json` (and `<shard>.data` when `include_data` was
    /// set at create time), and returns a `ShardSnapshot` per recovered
    /// shard.
    #[tracing::instrument(skip(self), fields(snapshot_id))]
    pub async fn restore_snapshot(
        &self,
        snapshot_id: &str,
    ) -> Result<Vec<ShardSnapshot>, BackupError> {
        tracing::info!("backup: restore_snapshot start");
        let key = format!("{snapshot_id}/snapshot.tar");
        let bytes = self
            .backend
            .get_blob(&key)
            .await
            .inspect_err(|e| {
                tracing::warn!(error = %e, "backup: restore — backend get_blob failed");
            })?
            .ok_or_else(|| {
                tracing::warn!("backup: restore — snapshot not found");
                BackupError::SnapshotNotFound(snapshot_id.to_owned())
            })?;

        let mut shards: BTreeMap<String, ShardSnapshot> = BTreeMap::new();
        let mut archive = Archive::new(&bytes[..]);
        for entry in archive
            .entries()
            .map_err(|e| BackupError::RestoreFailed(format!("read entries: {e}")))?
        {
            let mut entry =
                entry.map_err(|e| BackupError::RestoreFailed(format!("read entry: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| BackupError::RestoreFailed(format!("entry path: {e}")))?
                .into_owned();
            let name = path.to_string_lossy().into_owned();
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| BackupError::RestoreFailed(format!("read body: {e}")))?;

            // Names are `<shard>.meta.json` or `<shard>.data`.
            if let Some(shard_id) = name.strip_suffix(".meta.json") {
                shards
                    .entry(shard_id.to_owned())
                    .or_insert_with(|| ShardSnapshot {
                        shard_id: shard_id.to_owned(),
                        metadata: Vec::new(),
                        data: None,
                    })
                    .metadata = buf;
            } else if let Some(shard_id) = name.strip_suffix(".data") {
                shards
                    .entry(shard_id.to_owned())
                    .or_insert_with(|| ShardSnapshot {
                        shard_id: shard_id.to_owned(),
                        metadata: Vec::new(),
                        data: None,
                    })
                    .data = Some(buf);
            }
        }
        tracing::info!(
            recovered_shards = shards.len(),
            "backup: restore_snapshot success",
        );
        Ok(shards.into_values().collect())
    }

    /// List every snapshot known to the backend.
    pub async fn list_snapshots(&self) -> Vec<BackupSnapshot> {
        let Ok(keys) = self.backend.list_keys("").await else {
            return Vec::new();
        };
        let mut snapshots = Vec::new();
        for key in keys {
            if !key.ends_with("/manifest.json") {
                continue;
            }
            let Ok(Some(raw)) = self.backend.get_blob(&key).await else {
                continue;
            };
            let Ok(s) = std::str::from_utf8(&raw) else {
                continue;
            };
            if let Some(snap) = parse_manifest(s, &key) {
                snapshots.push(snap);
            }
        }
        snapshots
    }

    /// Delete a snapshot by id.
    pub async fn delete_snapshot(&self, snapshot_id: &str) -> Result<(), BackupError> {
        let manifest_key = format!("{snapshot_id}/manifest.json");
        let tarball_key = format!("{snapshot_id}/snapshot.tar");
        let existed_m = self.backend.delete_blob(&manifest_key).await?;
        let existed_t = self.backend.delete_blob(&tarball_key).await?;
        if !existed_m && !existed_t {
            return Err(BackupError::SnapshotNotFound(snapshot_id.to_owned()));
        }
        Ok(())
    }

    /// Delete every snapshot whose `created_at` is older than
    /// `retention_days`. Returns the number deleted. `retention_days == 0`
    /// means "delete every snapshot" (useful for tear-down).
    pub async fn cleanup_old(&self, retention_days: u32) -> usize {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let snapshots = self.list_snapshots().await;
        let mut deleted = 0;
        for snap in snapshots {
            let should_delete = if retention_days == 0 {
                true
            } else {
                parse_iso_epoch(&snap.created_at)
                    .is_some_and(|c| c + u64::from(retention_days) * 86_400 < now_secs)
            };
            if should_delete && self.delete_snapshot(&snap.snapshot_id).await.is_ok() {
                deleted += 1;
            }
        }
        deleted
    }

    /// Test-only: forcibly mark a backup in progress to exercise the
    /// concurrency-rejection path. Production callers should never
    /// touch this.
    #[doc(hidden)]
    pub fn force_in_progress(&self, value: bool) {
        self.in_progress.store(value, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Manifest helpers
// ---------------------------------------------------------------------------

fn append_blob(
    builder: &mut Builder<&mut Vec<u8>>,
    path: &str,
    body: &[u8],
) -> Result<(), BackupError> {
    let mut header = Header::new_gnu();
    header.set_path(path).map_err(BackupError::from)?;
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, body).map_err(BackupError::from)?;
    Ok(())
}

fn build_manifest(
    snapshot_id: &str,
    shard_count: usize,
    metadata_bytes: u64,
    data_bytes: u64,
    timestamp: &str,
) -> String {
    format!(
        "{{\"snapshot_id\":\"{snapshot_id}\",\"shard_count\":{shard_count},\"metadata_bytes\":{metadata_bytes},\"data_bytes\":{data_bytes},\"created_at\":\"{timestamp}\"}}"
    )
}

fn chrono_lite_iso(since_epoch: Duration) -> String {
    let secs = since_epoch.as_secs();
    let (y, mo, d) = days_to_ymd(secs / 86_400);
    let h = (secs / 3_600) % 24;
    let mi = (secs / 60) % 60;
    let s = secs % 60;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let mut y = 1970;
    let mut d = days;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if d < dy {
            break;
        }
        d -= dy;
        y += 1;
    }
    let dim = [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1;
    for &m in &dim {
        if d < m {
            break;
        }
        d -= m;
        mo += 1;
    }
    (y, mo, d + 1)
}

fn parse_iso_epoch(iso: &str) -> Option<u64> {
    // Accept "YYYY-MM-DDTHH:MM:SSZ".
    let parts: Vec<&str> = iso
        .split(['-', 'T', ':', 'Z'])
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 6 {
        return None;
    }
    let y: u64 = parts[0].parse().ok()?;
    let mo: u64 = parts[1].parse().ok()?;
    let d: u64 = parts[2].parse().ok()?;
    let h: u64 = parts[3].parse().ok()?;
    let mi: u64 = parts[4].parse().ok()?;
    let s: u64 = parts[5].parse().ok()?;

    let mut days: u64 = 0;
    for yy in 1970..y {
        days += if is_leap(yy) { 366 } else { 365 };
    }
    let dim = [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mo_idx = usize::try_from(mo).ok()?;
    for &m in dim.iter().take(mo_idx - 1) {
        days += m;
    }
    days += d - 1;
    Some(days * 86_400 + h * 3_600 + mi * 60 + s)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn parse_manifest(json: &str, key: &str) -> Option<BackupSnapshot> {
    let snapshot_id = extract_json_string(json, "snapshot_id")?;
    let shard_count = usize::try_from(extract_json_number(json, "shard_count")?).ok()?;
    let metadata_bytes = extract_json_number(json, "metadata_bytes")?;
    let data_bytes = extract_json_number(json, "data_bytes")?;
    let created_at = extract_json_string(json, "created_at")?;
    Some(BackupSnapshot {
        snapshot_id: snapshot_id.clone(),
        tarball_key: format!("{snapshot_id}/snapshot.tar"),
        manifest_key: key.to_owned(),
        metadata_bytes,
        data_bytes,
        shard_count,
        created_at,
        elapsed: Duration::ZERO,
    })
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle)? + needle.len();
    let end = start + json[start..].find('"')?;
    Some(json[start..end].to_owned())
}

fn extract_json_number(json: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn fs_backend() -> (tempfile::TempDir, Arc<dyn ObjectBackupBackend>) {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileSystemBackupBackend::new(dir.path().to_path_buf()).unwrap();
        (dir, Arc::new(backend) as Arc<dyn ObjectBackupBackend>)
    }

    fn sample_shards() -> Vec<ShardSnapshot> {
        vec![
            ShardSnapshot {
                shard_id: "shard-1".into(),
                metadata: br#"{"v":1}"#.to_vec(),
                data: None,
            },
            ShardSnapshot {
                shard_id: "shard-2".into(),
                metadata: br#"{"v":2}"#.to_vec(),
                data: Some(b"chunk-data".to_vec()),
            },
        ]
    }

    fn config(include_data: bool) -> BackupConfig {
        BackupConfig {
            include_data,
            retention_days: 7,
        }
    }

    /// Mock S3-like backend for testing the trait without real network.
    /// Used by Phase 14d BDD steps to assert backend isolation without
    /// standing up a real S3.
    #[derive(Default)]
    struct InMemoryBackend(Mutex<HashMap<String, Vec<u8>>>);

    #[async_trait]
    impl ObjectBackupBackend for InMemoryBackend {
        async fn put_blob(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), bytes.to_vec());
            Ok(())
        }

        async fn get_blob(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        async fn list_keys(&self, prefix: &str) -> io::Result<Vec<String>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }

        async fn delete_blob(&self, key: &str) -> io::Result<bool> {
            Ok(self.0.lock().unwrap().remove(key).is_some())
        }
    }

    #[tokio::test]
    async fn create_snapshot_writes_tarball_and_manifest() {
        let (_tmp, backend) = fs_backend();
        let mgr = BackupManager::new(backend, config(true));

        let snap = mgr.create_snapshot(&sample_shards()).await.unwrap();
        assert_eq!(snap.shard_count, 2);
        assert!(snap.metadata_bytes > 0);
        assert!(
            snap.data_bytes > 0,
            "include_data should populate data_bytes"
        );
        assert!(snap.tarball_key.ends_with("/snapshot.tar"));
        assert!(snap.manifest_key.ends_with("/manifest.json"));
    }

    #[tokio::test]
    async fn restore_recovers_every_shard_metadata_and_data() {
        let (_tmp, backend) = fs_backend();
        let mgr = BackupManager::new(backend, config(true));
        let snap = mgr.create_snapshot(&sample_shards()).await.unwrap();

        let restored = mgr.restore_snapshot(&snap.snapshot_id).await.unwrap();
        assert_eq!(restored.len(), 2);
        let by_id: HashMap<&str, &ShardSnapshot> =
            restored.iter().map(|s| (s.shard_id.as_str(), s)).collect();
        assert_eq!(by_id["shard-1"].metadata, br#"{"v":1}"#);
        assert!(by_id["shard-1"].data.is_none());
        assert_eq!(by_id["shard-2"].metadata, br#"{"v":2}"#);
        assert_eq!(by_id["shard-2"].data.as_deref().unwrap(), b"chunk-data");
    }

    #[tokio::test]
    async fn restore_unknown_snapshot_id_returns_not_found() {
        let (_tmp, backend) = fs_backend();
        let mgr = BackupManager::new(backend, config(false));
        let err = mgr.restore_snapshot("nope").await.unwrap_err();
        assert!(matches!(err, BackupError::SnapshotNotFound(_)));
    }

    #[tokio::test]
    async fn list_snapshots_finds_created_via_manifest() {
        let (_tmp, backend) = fs_backend();
        let mgr = BackupManager::new(backend, config(false));
        let snap = mgr.create_snapshot(&sample_shards()).await.unwrap();
        let list = mgr.list_snapshots().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].snapshot_id, snap.snapshot_id);
    }

    #[tokio::test]
    async fn delete_snapshot_removes_both_blobs() {
        let (_tmp, backend_arc) = fs_backend();
        let backend = Arc::clone(&backend_arc);
        let mgr = BackupManager::new(backend_arc, config(false));
        let snap = mgr.create_snapshot(&sample_shards()).await.unwrap();
        mgr.delete_snapshot(&snap.snapshot_id).await.unwrap();
        assert!(backend.get_blob(&snap.tarball_key).await.unwrap().is_none());
        assert!(backend
            .get_blob(&snap.manifest_key)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn cleanup_removes_expired_snapshots() {
        let (_tmp, backend) = fs_backend();
        let mgr = BackupManager::new(backend, config(false));
        mgr.create_snapshot(&sample_shards()).await.unwrap();
        let deleted = mgr.cleanup_old(0).await;
        assert_eq!(deleted, 1);
    }

    #[tokio::test]
    async fn concurrent_backup_rejected() {
        let (_tmp, backend) = fs_backend();
        let mgr = BackupManager::new(backend, config(false));
        mgr.force_in_progress(true);
        let res = mgr.create_snapshot(&sample_shards()).await;
        assert!(matches!(res, Err(BackupError::InProgress)));
        mgr.force_in_progress(false);
        assert!(mgr.create_snapshot(&sample_shards()).await.is_ok());
    }

    /// Same `BackupManager` works against an in-memory backend with no
    /// changes — proves the trait actually isolates the storage layer.
    #[tokio::test]
    async fn in_memory_backend_round_trip() {
        let backend: Arc<dyn ObjectBackupBackend> = Arc::new(InMemoryBackend::default());
        let mgr = BackupManager::new(backend, config(true));
        let snap = mgr.create_snapshot(&sample_shards()).await.unwrap();
        let restored = mgr.restore_snapshot(&snap.snapshot_id).await.unwrap();
        assert_eq!(restored.len(), 2);
    }
}
