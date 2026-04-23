//! Dataset staging API for pre-populating the cache (ADR-031 §6).
//!
//! Pull-based: the client fetches chunks from canonical into the L2
//! cache with pinned retention. Works with Slurm (prolog/epilog),
//! Lattice (parallel dispatch), or manual invocation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kiseki_common::ids::ChunkId;

/// Result of a staging operation.
#[derive(Clone, Debug)]
pub struct StageResult {
    /// Namespace path that was staged.
    pub namespace_path: String,
    /// Number of compositions (files) staged.
    pub compositions: usize,
    /// Number of chunks staged (fetched + already cached).
    pub chunks_total: usize,
    /// Number of chunks that were already in cache.
    pub chunks_cached: usize,
    /// Total bytes staged.
    pub bytes_total: u64,
    /// Elapsed time.
    pub elapsed: std::time::Duration,
}

/// A staged dataset tracked by the cache.
#[derive(Clone, Debug)]
pub struct StagedDataset {
    /// Namespace path.
    pub namespace_path: String,
    /// Chunk IDs in the dataset.
    pub chunk_ids: Vec<ChunkId>,
    /// Total bytes.
    pub bytes: u64,
    /// When staging completed.
    pub staged_at: Instant,
}

/// Staging configuration limits.
#[derive(Clone, Debug)]
pub struct StagingConfig {
    /// Maximum directory recursion depth. Default: 10.
    pub max_depth: usize,
    /// Maximum files to stage. Default: 100,000.
    pub max_files: usize,
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_files: 100_000,
        }
    }
}

/// Staging manager — tracks staged datasets and their manifests.
pub struct StagingManager {
    /// Active staged datasets.
    datasets: HashMap<String, StagedDataset>,
    /// Pool directory for manifest files.
    pool_dir: Option<PathBuf>,
    /// Configuration limits.
    config: StagingConfig,
}

impl StagingManager {
    /// Create a new staging manager.
    #[must_use]
    pub fn new(pool_dir: Option<PathBuf>, config: StagingConfig) -> Self {
        let mut mgr = Self {
            datasets: HashMap::new(),
            pool_dir,
            config,
        };
        // Load existing manifests from disk if pool_dir exists.
        mgr.load_manifests();
        mgr
    }

    /// Stage a dataset: record its chunks as pinned.
    ///
    /// The caller is responsible for fetching chunks from canonical
    /// and inserting them into the cache before calling this.
    /// This method records the manifest and marks the chunks as staged.
    pub fn record_staged(&mut self, namespace_path: String, chunk_ids: &[ChunkId], bytes: u64) {
        let dataset = StagedDataset {
            namespace_path: namespace_path.clone(),
            chunk_ids: chunk_ids.to_vec(),
            bytes,
            staged_at: Instant::now(),
        };

        // Write manifest to disk.
        if let Some(ref pool_dir) = self.pool_dir {
            let manifest_path = staging_manifest_path(pool_dir, &namespace_path);
            if let Some(parent) = manifest_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let manifest = StagingManifest {
                namespace_path: namespace_path.clone(),
                chunk_ids: chunk_ids.iter().map(hex_encode_chunk).collect(),
                bytes,
            };
            if let Ok(json) = serde_json::to_string_pretty(&manifest) {
                let _ = std::fs::write(&manifest_path, json);
            }
        }

        self.datasets.insert(namespace_path, dataset);
    }

    /// Check if a dataset is staged.
    #[must_use]
    pub fn is_staged(&self, namespace_path: &str) -> bool {
        self.datasets.contains_key(namespace_path)
    }

    /// Get the chunk IDs for a staged dataset.
    #[must_use]
    pub fn get_staged_chunks(&self, namespace_path: &str) -> Option<&[ChunkId]> {
        self.datasets
            .get(namespace_path)
            .map(|d| d.chunk_ids.as_slice())
    }

    /// List all staged datasets.
    #[must_use]
    pub fn list(&self) -> Vec<&StagedDataset> {
        self.datasets.values().collect()
    }

    /// Release a specific dataset (unpin its chunks).
    ///
    /// Returns the chunk IDs that were released (for cache eviction).
    pub fn release(&mut self, namespace_path: &str) -> Vec<ChunkId> {
        let chunks = self
            .datasets
            .remove(namespace_path)
            .map(|d| d.chunk_ids)
            .unwrap_or_default();

        // Remove manifest from disk.
        if let Some(ref pool_dir) = self.pool_dir {
            let manifest_path = staging_manifest_path(pool_dir, namespace_path);
            let _ = std::fs::remove_file(&manifest_path);
        }

        chunks
    }

    /// Release all staged datasets.
    pub fn release_all(&mut self) -> Vec<ChunkId> {
        let all_chunks: Vec<ChunkId> = self
            .datasets
            .values()
            .flat_map(|d| d.chunk_ids.iter().copied())
            .collect();

        // Remove all manifest files.
        if let Some(ref pool_dir) = self.pool_dir {
            let staging_dir = pool_dir.join("staging");
            if staging_dir.exists() {
                let _ = std::fs::remove_dir_all(&staging_dir);
                let _ = std::fs::create_dir_all(&staging_dir);
            }
        }

        self.datasets.clear();
        all_chunks
    }

    /// Number of staged datasets.
    #[must_use]
    pub fn count(&self) -> usize {
        self.datasets.len()
    }

    /// Total bytes across all staged datasets.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.datasets.values().map(|d| d.bytes).sum()
    }

    /// Maximum staging depth.
    #[must_use]
    pub fn max_depth(&self) -> usize {
        self.config.max_depth
    }

    /// Maximum staging files.
    #[must_use]
    pub fn max_files(&self) -> usize {
        self.config.max_files
    }

    /// Load manifests from disk (for pool adoption).
    fn load_manifests(&mut self) {
        let Some(ref pool_dir) = self.pool_dir else {
            return;
        };
        let staging_dir = pool_dir.join("staging");
        let Ok(entries) = std::fs::read_dir(&staging_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "manifest") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(manifest) = serde_json::from_str::<StagingManifest>(&content) {
                        let chunk_ids: Vec<ChunkId> = manifest
                            .chunk_ids
                            .iter()
                            .filter_map(|hex| hex_decode_chunk(hex))
                            .collect();
                        self.datasets.insert(
                            manifest.namespace_path.clone(),
                            StagedDataset {
                                namespace_path: manifest.namespace_path,
                                chunk_ids,
                                bytes: manifest.bytes,
                                staged_at: Instant::now(),
                            },
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pool handoff
// ---------------------------------------------------------------------------

/// Check if a cache pool exists and can be adopted.
///
/// Returns `true` if the pool directory exists and the `pool.lock`
/// flock can be acquired (no live owner).
#[must_use]
pub fn can_adopt_pool(cache_dir: &Path, tenant_id: &str, pool_id: &str) -> bool {
    let pool_dir = cache_dir.join(tenant_id).join(pool_id);
    let lock_path = pool_dir.join("pool.lock");
    if !lock_path.exists() {
        return false;
    }

    // Try non-blocking flock. If we get it, the pool is orphaned
    // (or the staging daemon released it for us).
    try_flock_adoptable(&lock_path)
}

#[cfg(unix)]
fn try_flock_adoptable(lock_path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(lock_path) else {
        return false;
    };
    // SAFETY: flock with LOCK_EX|LOCK_NB on a valid fd.
    #[allow(unsafe_code)]
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        // Got the lock — pool is adoptable. Release immediately.
        #[allow(unsafe_code)]
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        return true;
    }
    false
}

#[cfg(not(unix))]
fn try_flock_adoptable(_lock_path: &Path) -> bool {
    false
}

/// Scan for orphaned pools and return their paths.
///
/// An orphaned pool is one whose `pool.lock` has no live flock holder.
#[must_use]
pub fn find_orphaned_pools(cache_dir: &Path, tenant_id: &str) -> Vec<PathBuf> {
    let tenant_dir = cache_dir.join(tenant_id);
    let mut orphans = Vec::new();

    let Ok(entries) = std::fs::read_dir(&tenant_dir) else {
        return orphans;
    };

    for entry in entries.flatten() {
        let pool_dir = entry.path();
        if !pool_dir.is_dir() {
            continue;
        }
        let lock_path = pool_dir.join("pool.lock");
        if !lock_path.exists() {
            // No lock file — definitely orphaned.
            orphans.push(pool_dir);
            continue;
        }

        if try_flock_adoptable(&lock_path) {
            orphans.push(pool_dir);
        }
    }

    orphans
}

// ---------------------------------------------------------------------------
// Manifest serialization
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct StagingManifest {
    namespace_path: String,
    chunk_ids: Vec<String>,
    bytes: u64,
}

fn staging_manifest_path(pool_dir: &Path, namespace_path: &str) -> PathBuf {
    let safe_name = namespace_path.replace(['/', '\\'], "_");
    pool_dir
        .join("staging")
        .join(format!("{safe_name}.manifest"))
}

fn hex_encode_chunk(chunk_id: &ChunkId) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in &chunk_id.0 {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_decode_chunk(hex: &str) -> Option<ChunkId> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        if i >= 32 {
            return None;
        }
        let s = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(ChunkId(bytes))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chunk_id(byte: u8) -> ChunkId {
        ChunkId([byte; 32])
    }

    #[test]
    fn stage_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = StagingManager::new(Some(dir.path().to_path_buf()), StagingConfig::default());

        let chunks = vec![test_chunk_id(1), test_chunk_id(2)];
        mgr.record_staged("/training/imagenet".into(), &chunks, 1024);

        assert!(mgr.is_staged("/training/imagenet"));
        assert_eq!(mgr.count(), 1);
        assert_eq!(mgr.total_bytes(), 1024);

        let released = mgr.release("/training/imagenet");
        assert_eq!(released.len(), 2);
        assert!(!mgr.is_staged("/training/imagenet"));
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn release_all_clears_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = StagingManager::new(Some(dir.path().to_path_buf()), StagingConfig::default());

        mgr.record_staged("/ds1".into(), &[test_chunk_id(1)], 100);
        mgr.record_staged("/ds2".into(), &[test_chunk_id(2)], 200);

        let released = mgr.release_all();
        assert_eq!(released.len(), 2);
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn manifest_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().to_path_buf();

        // Stage.
        {
            let mut mgr = StagingManager::new(Some(pool_dir.clone()), StagingConfig::default());
            mgr.record_staged(
                "/training/data".into(),
                &[test_chunk_id(0xAB), test_chunk_id(0xCD)],
                2048,
            );
        }

        // Reload from manifests.
        let mgr = StagingManager::new(Some(pool_dir), StagingConfig::default());
        assert!(mgr.is_staged("/training/data"));
        assert_eq!(mgr.get_staged_chunks("/training/data").unwrap().len(), 2);
    }

    #[test]
    fn idempotent_re_stage() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = StagingManager::new(Some(dir.path().to_path_buf()), StagingConfig::default());

        let chunks = vec![test_chunk_id(1)];
        mgr.record_staged("/ds".into(), &chunks, 100);
        mgr.record_staged("/ds".into(), &chunks, 100); // re-stage is no-op (overwrite)

        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn hex_roundtrip() {
        let id = test_chunk_id(0xAB);
        let hex = hex_encode_chunk(&id);
        let decoded = hex_decode_chunk(&hex).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn release_nonexistent_returns_empty() {
        let mut mgr = StagingManager::new(None, StagingConfig::default());
        let released = mgr.release("/nonexistent");
        assert!(released.is_empty());
    }

    #[test]
    fn list_staged_datasets() {
        let mut mgr = StagingManager::new(None, StagingConfig::default());
        mgr.record_staged("/ds1".into(), &[test_chunk_id(1)], 100);
        mgr.record_staged("/ds2".into(), &[test_chunk_id(2)], 200);

        let list = mgr.list();
        assert_eq!(list.len(), 2);
    }
}
