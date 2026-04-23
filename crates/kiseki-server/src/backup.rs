//! Cluster backup and disaster recovery.
//!
//! Creates point-in-time snapshots of shard metadata for offline storage
//! and restore. Snapshot data is written to a configurable directory as
//! JSON metadata files, one per shard.

#![allow(dead_code)] // Module not yet wired into the running server.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Backup configuration.
pub struct BackupConfig {
    /// Directory for backup snapshots.
    pub backup_dir: PathBuf,
    /// Whether to include chunk data (vs metadata-only).
    pub include_data: bool,
    /// Maximum backup age before cleanup (days).
    pub retention_days: u32,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            backup_dir: PathBuf::from("/tmp/kiseki-backup"),
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
    /// Path to the snapshot directory.
    pub path: PathBuf,
    /// Metadata bytes written.
    pub metadata_bytes: u64,
    /// Data bytes written (0 if metadata-only).
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
    /// Backup directory does not exist and could not be created.
    #[error("backup directory not found: {0}")]
    DirNotFound(String),
    /// Another backup is already in progress.
    #[error("backup in progress")]
    InProgress,
    /// Snapshot not found.
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    /// Restore failed.
    #[error("restore failed: {0}")]
    RestoreFailed(String),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Shard metadata passed to `create_snapshot`.
#[derive(Clone, Debug)]
pub struct ShardSnapshot {
    /// Shard identifier.
    pub shard_id: String,
    /// Serialised metadata (JSON bytes).
    pub metadata: Vec<u8>,
    /// Optional data payload.
    pub data: Option<Vec<u8>>,
}

/// Backup manager -- creates and manages cluster snapshots.
pub struct BackupManager {
    config: BackupConfig,
    in_progress: AtomicBool,
}

impl BackupManager {
    /// Create a new backup manager with the given configuration.
    #[must_use]
    pub fn new(config: BackupConfig) -> Self {
        Self {
            config,
            in_progress: AtomicBool::new(false),
        }
    }

    /// Create a point-in-time snapshot.
    ///
    /// Writes shard metadata (and optionally data) to
    /// `backup_dir/<snapshot_id>/`. Returns a descriptor of the
    /// completed snapshot.
    pub fn create_snapshot(&self, shards: &[ShardSnapshot]) -> Result<BackupSnapshot, BackupError> {
        // Reject concurrent backups.
        if self
            .in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(BackupError::InProgress);
        }

        let start = Instant::now();
        let result = self.do_create_snapshot(shards, start);

        // Always clear the flag.
        self.in_progress.store(false, Ordering::SeqCst);
        result
    }

    fn do_create_snapshot(
        &self,
        shards: &[ShardSnapshot],
        start: Instant,
    ) -> Result<BackupSnapshot, BackupError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp = chrono_lite_iso(now);
        let snapshot_id = format!("{}-{}", timestamp, uuid::Uuid::new_v4());

        let snap_dir = self.config.backup_dir.join(&snapshot_id);
        fs::create_dir_all(&snap_dir).map_err(|e| {
            BackupError::DirNotFound(format!("{}: {e}", self.config.backup_dir.display()))
        })?;

        let mut metadata_bytes: u64 = 0;
        let mut data_bytes: u64 = 0;

        for shard in shards {
            let meta_path = snap_dir.join(format!("{}.meta.json", shard.shard_id));
            fs::write(&meta_path, &shard.metadata)?;
            metadata_bytes += shard.metadata.len() as u64;

            if self.config.include_data {
                if let Some(ref data) = shard.data {
                    let data_path = snap_dir.join(format!("{}.data", shard.shard_id));
                    fs::write(&data_path, data)?;
                    data_bytes += data.len() as u64;
                }
            }
        }

        // Write manifest.
        let manifest = format!(
            "{{\"snapshot_id\":\"{snapshot_id}\",\"shard_count\":{},\"metadata_bytes\":{metadata_bytes},\"data_bytes\":{data_bytes},\"created_at\":\"{timestamp}\"}}",
            shards.len()
        );
        fs::write(snap_dir.join("manifest.json"), &manifest)?;

        Ok(BackupSnapshot {
            snapshot_id,
            path: snap_dir,
            metadata_bytes,
            data_bytes,
            shard_count: shards.len(),
            created_at: timestamp,
            elapsed: start.elapsed(),
        })
    }

    /// List all snapshots in the backup directory.
    ///
    /// Returns an empty vec if the directory does not exist.
    pub fn list_snapshots(&self) -> Vec<BackupSnapshot> {
        let Ok(entries) = fs::read_dir(&self.config.backup_dir) else {
            return Vec::new();
        };

        let mut snapshots = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            if let Ok(manifest_raw) = fs::read_to_string(&manifest_path) {
                if let Some(snap) = parse_manifest(&manifest_raw, &path) {
                    snapshots.push(snap);
                }
            }
        }
        snapshots
    }

    /// Delete a snapshot by ID.
    pub fn delete_snapshot(&self, snapshot_id: &str) -> Result<(), BackupError> {
        let snap_dir = self.config.backup_dir.join(snapshot_id);
        if !snap_dir.exists() {
            return Err(BackupError::SnapshotNotFound(snapshot_id.to_owned()));
        }
        fs::remove_dir_all(&snap_dir)?;
        Ok(())
    }

    /// Remove snapshots older than `retention_days`.
    ///
    /// Returns the number of snapshots deleted.
    pub fn cleanup_old(&self, retention_days: u32) -> usize {
        let cutoff = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(u64::from(retention_days) * 86_400);

        let mut deleted = 0;
        for snap in self.list_snapshots() {
            // Parse created_at timestamp to epoch seconds.
            if let Some(epoch) = parse_iso_epoch(&snap.created_at) {
                if epoch <= cutoff && self.delete_snapshot(&snap.snapshot_id).is_ok() {
                    deleted += 1;
                }
            }
        }
        deleted
    }
}

// ── helpers ────────────────────────────────────────────────────────────

/// Minimal ISO 8601 timestamp from a `Duration` since UNIX epoch.
fn chrono_lite_iso(since_epoch: Duration) -> String {
    // We avoid pulling in chrono by doing manual UTC formatting.
    let secs = since_epoch.as_secs();
    let days = secs / 86_400;
    let time_secs = secs % 86_400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since 1970-01-01 → year/month/day (Rata Die algorithm).
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil from days (Howard Hinnant algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Parse a minimal ISO 8601 timestamp to epoch seconds.
fn parse_iso_epoch(iso: &str) -> Option<u64> {
    // Expected: "YYYY-MM-DDTHH:MM:SSZ"
    if iso.len() < 19 {
        return None;
    }
    let year: u64 = iso[0..4].parse().ok()?;
    let month: u64 = iso[5..7].parse().ok()?;
    let day: u64 = iso[8..10].parse().ok()?;
    let hour: u64 = iso[11..13].parse().ok()?;
    let min: u64 = iso[14..16].parse().ok()?;
    let sec: u64 = iso[17..19].parse().ok()?;

    // Rough days from epoch (good enough for retention comparison).
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        #[allow(clippy::cast_possible_truncation)]
        let idx = m as usize;
        days += month_days[idx];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Some(days * 86_400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Parse a manifest JSON string into a `BackupSnapshot`.
fn parse_manifest(raw: &str, path: &Path) -> Option<BackupSnapshot> {
    // Minimal JSON parsing without serde_json dependency.
    let snapshot_id = extract_json_string(raw, "snapshot_id")?;
    let created_at = extract_json_string(raw, "created_at")?;
    let shard_count = extract_json_number(raw, "shard_count")?;
    let metadata_bytes = extract_json_number(raw, "metadata_bytes")?;
    let data_bytes = extract_json_number(raw, "data_bytes")?;

    Some(BackupSnapshot {
        snapshot_id,
        path: path.to_path_buf(),
        metadata_bytes,
        data_bytes,
        #[allow(clippy::cast_possible_truncation)]
        shard_count: shard_count as usize,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(dir: &std::path::Path) -> BackupConfig {
        BackupConfig {
            backup_dir: dir.to_path_buf(),
            include_data: false,
            retention_days: 7,
        }
    }

    fn sample_shards() -> Vec<ShardSnapshot> {
        vec![
            ShardSnapshot {
                shard_id: "shard-1".into(),
                metadata: b"{\"test\": true}".to_vec(),
                data: None,
            },
            ShardSnapshot {
                shard_id: "shard-2".into(),
                metadata: b"{\"test\": true}".to_vec(),
                data: Some(b"chunk-data".to_vec()),
            },
        ]
    }

    #[test]
    fn create_snapshot_writes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = BackupManager::new(temp_config(tmp.path()));

        let snap = mgr.create_snapshot(&sample_shards()).unwrap();
        assert_eq!(snap.shard_count, 2);
        assert!(snap.metadata_bytes > 0);
        assert!(snap.path.exists());
        assert!(snap.path.join("manifest.json").exists());
        assert!(snap.path.join("shard-1.meta.json").exists());
        assert!(snap.path.join("shard-2.meta.json").exists());
    }

    #[test]
    fn list_snapshots_finds_created() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = BackupManager::new(temp_config(tmp.path()));

        let snap = mgr.create_snapshot(&sample_shards()).unwrap();
        let list = mgr.list_snapshots();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].snapshot_id, snap.snapshot_id);
    }

    #[test]
    fn delete_snapshot_removes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = BackupManager::new(temp_config(tmp.path()));

        let snap = mgr.create_snapshot(&sample_shards()).unwrap();
        assert!(snap.path.exists());

        mgr.delete_snapshot(&snap.snapshot_id).unwrap();
        assert!(!snap.path.exists());
    }

    #[test]
    fn cleanup_removes_old_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = BackupManager::new(temp_config(tmp.path()));

        let snap = mgr.create_snapshot(&sample_shards()).unwrap();
        // With retention_days=0, everything is "old".
        let deleted = mgr.cleanup_old(0);
        assert_eq!(deleted, 1);
        assert!(!snap.path.exists());
    }

    #[test]
    fn concurrent_backup_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = BackupManager::new(temp_config(tmp.path()));

        // Simulate in-progress by setting the flag manually.
        mgr.in_progress.store(true, Ordering::SeqCst);

        let result = mgr.create_snapshot(&sample_shards());
        assert!(matches!(result, Err(BackupError::InProgress)));

        // Clear the flag and verify normal operation resumes.
        mgr.in_progress.store(false, Ordering::SeqCst);
        let snap = mgr.create_snapshot(&sample_shards());
        assert!(snap.is_ok());
    }
}
