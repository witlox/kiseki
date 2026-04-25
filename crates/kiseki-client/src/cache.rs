//! Two-tier client-side chunk cache (ADR-031).
//!
//! L1: in-memory `HashMap` with `Zeroizing` plaintext, LRU eviction.
//! L2: local `NVMe` files with `CRC32` integrity trailers.
//! Metadata: file-to-chunk-list mappings with bounded TTL.
//!
//! Three modes: `Pinned` (staging-driven), `Organic` (LRU), `Bypass`
//! (no caching). See ADR-031 for invariants I-CC1 through I-CC13.
#![allow(clippy::cast_possible_truncation)] // u64 → usize for file sizes is intentional

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kiseki_common::ids::ChunkId;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// Cache mode
// ---------------------------------------------------------------------------

/// Cache operating mode, selected at session establishment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    /// Staging-driven. Chunks retained against eviction until explicit release.
    Pinned,
    /// LRU with usage-weighted retention. Default for mixed workloads.
    Organic,
    /// No caching. All reads go directly to canonical.
    Bypass,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Cache configuration.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Cache operating mode.
    pub mode: CacheMode,
    /// L1 (in-memory) maximum bytes. Default: 256 MB.
    pub max_memory_bytes: u64,
    /// L2 (`NVMe`) maximum bytes per process. Default: 50 GB.
    pub max_cache_bytes: u64,
    /// Metadata TTL. Default: 5 seconds.
    pub metadata_ttl: Duration,
    /// L2 cache directory. Default: `/tmp/kiseki-cache`.
    pub cache_dir: PathBuf,
    /// Maximum disconnect duration before cache wipe. Default: 300s.
    pub max_disconnect_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            mode: CacheMode::Organic,
            max_memory_bytes: 256 * 1024 * 1024,
            max_cache_bytes: 50 * 1024 * 1024 * 1024,
            metadata_ttl: Duration::from_secs(5),
            cache_dir: PathBuf::from("/tmp/kiseki-cache"),
            max_disconnect_seconds: 300,
        }
    }
}

// ---------------------------------------------------------------------------
// L1 cache (in-memory)
// ---------------------------------------------------------------------------

/// L1 entry: plaintext data with access tracking.
struct L1Entry {
    /// Decrypted plaintext chunk data (zeroized on drop).
    data: Zeroizing<Vec<u8>>,
    /// Last access time (for LRU).
    last_access: Instant,
    /// Access count (for usage-weighted retention in organic mode).
    access_count: u32,
    /// Whether this chunk is pinned (survives LRU eviction).
    pinned: bool,
}

/// In-memory L1 cache with LRU eviction and zeroize-on-drop.
pub struct CacheL1 {
    entries: HashMap<ChunkId, L1Entry>,
    current_bytes: u64,
    max_bytes: u64,
}

impl CacheL1 {
    /// Create a new L1 cache with the given capacity.
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            current_bytes: 0,
            max_bytes,
        }
    }

    /// Get a chunk from L1, updating access stats.
    pub fn get(&mut self, chunk_id: &ChunkId) -> Option<&[u8]> {
        if let Some(entry) = self.entries.get_mut(chunk_id) {
            entry.last_access = Instant::now();
            entry.access_count = entry.access_count.saturating_add(1);
            Some(&entry.data)
        } else {
            None
        }
    }

    /// Insert a chunk into L1. Evicts LRU entries if at capacity.
    pub fn insert(&mut self, chunk_id: ChunkId, data: Vec<u8>, pinned: bool) {
        let data_len = data.len() as u64;

        // Remove existing entry if present (update).
        if let Some(old) = self.entries.remove(&chunk_id) {
            self.current_bytes -= old.data.len() as u64;
        }

        // Evict until we have room (skip pinned entries).
        while self.current_bytes + data_len > self.max_bytes && !self.entries.is_empty() {
            if !self.evict_one_lru() {
                break; // only pinned entries remain
            }
        }

        self.current_bytes += data_len;
        self.entries.insert(
            chunk_id,
            L1Entry {
                data: Zeroizing::new(data),
                last_access: Instant::now(),
                access_count: 1,
                pinned,
            },
        );
    }

    /// Remove a specific chunk.
    pub fn remove(&mut self, chunk_id: &ChunkId) {
        if let Some(entry) = self.entries.remove(chunk_id) {
            self.current_bytes -= entry.data.len() as u64;
            // entry.data is Zeroizing — dropped and zeroed here
        }
    }

    /// Wipe all entries with zeroize (I-CC2).
    pub fn wipe(&mut self) {
        self.entries.clear(); // Zeroizing drops zero all data
        self.current_bytes = 0;
    }

    /// Current memory usage in bytes.
    #[must_use]
    pub fn bytes_used(&self) -> u64 {
        self.current_bytes
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict the least-recently-used non-pinned entry. Returns false if none evictable.
    fn evict_one_lru(&mut self) -> bool {
        let victim = self
            .entries
            .iter()
            .filter(|(_, e)| !e.pinned)
            .min_by_key(|(_, e)| e.last_access)
            .map(|(id, _)| *id);

        if let Some(id) = victim {
            self.remove(&id);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// L2 cache (NVMe)
// ---------------------------------------------------------------------------

/// CRC32 computation for L2 integrity (I-CC13).
fn crc32(data: &[u8]) -> u32 {
    // Simple CRC32 (IEEE polynomial) without pulling in a crate.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// NVMe-backed L2 cache with CRC32 integrity.
pub struct CacheL2 {
    pool_dir: PathBuf,
    current_bytes: u64,
    max_bytes: u64,
}

impl CacheL2 {
    /// Create or open an L2 pool directory.
    pub fn open(pool_dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(pool_dir.join("chunks"))?;
        std::fs::create_dir_all(pool_dir.join("meta"))?;
        std::fs::create_dir_all(pool_dir.join("staging"))?;

        // Calculate current usage.
        let current_bytes = dir_size(&pool_dir.join("chunks"));

        Ok(Self {
            pool_dir,
            current_bytes,
            max_bytes,
        })
    }

    /// Read a chunk from L2, verifying CRC32.
    ///
    /// Returns `None` if not found or CRC mismatch (I-CC7, I-CC13).
    pub fn get(&self, chunk_id: &ChunkId) -> Option<Vec<u8>> {
        let path = self.chunk_path(chunk_id);
        let raw = std::fs::read(&path).ok()?;

        if raw.len() < 4 {
            // Corrupt: too short to have CRC trailer.
            tracing::warn!(chunk_id = %hex_short(chunk_id), "L2 entry too short, deleting");
            let _ = std::fs::remove_file(&path);
            return None;
        }

        let (data, trailer) = raw.split_at(raw.len() - 4);
        let stored_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let computed_crc = crc32(data);

        if stored_crc != computed_crc {
            tracing::warn!(
                chunk_id = %hex_short(chunk_id),
                stored_crc,
                computed_crc,
                "L2 CRC32 mismatch, deleting corrupt entry"
            );
            let _ = std::fs::remove_file(&path);
            return None;
        }

        Some(data.to_vec())
    }

    /// Write a chunk to L2 with CRC32 trailer.
    pub fn put(&mut self, chunk_id: &ChunkId, data: &[u8]) -> std::io::Result<()> {
        let data_len = data.len() as u64 + 4; // +4 for CRC trailer

        if self.current_bytes + data_len > self.max_bytes {
            return Err(std::io::Error::other("L2 cache capacity exceeded"));
        }

        let path = self.chunk_path(chunk_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write data + CRC32 trailer.
        let checksum = crc32(data);
        let mut buf = Vec::with_capacity(data.len() + 4);
        buf.extend_from_slice(data);
        buf.extend_from_slice(&checksum.to_le_bytes());

        // Set file permissions to 0600.
        std::fs::write(&path, &buf)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }

        self.current_bytes += data_len;
        Ok(())
    }

    /// Remove a chunk from L2 with zeroize-before-delete (I-CC2).
    pub fn remove(&mut self, chunk_id: &ChunkId) {
        let path = self.chunk_path(chunk_id);
        if let Ok(metadata) = std::fs::metadata(&path) {
            let size = metadata.len();
            // Overwrite with zeros before unlinking.
            let zeros = vec![0u8; size as usize];
            let _ = std::fs::write(&path, &zeros);
            let _ = std::fs::remove_file(&path);
            self.current_bytes = self.current_bytes.saturating_sub(size);
        }
    }

    /// Whether a chunk exists in L2.
    #[must_use]
    pub fn contains(&self, chunk_id: &ChunkId) -> bool {
        self.chunk_path(chunk_id).exists()
    }

    /// Wipe entire L2 pool with zeroize (I-CC2, I-CC8).
    pub fn wipe(&mut self) {
        wipe_directory(&self.pool_dir.join("chunks"));
        self.current_bytes = 0;
    }

    /// Current disk usage in bytes.
    #[must_use]
    pub fn bytes_used(&self) -> u64 {
        self.current_bytes
    }

    /// Pool directory path.
    #[must_use]
    pub fn pool_dir(&self) -> &Path {
        &self.pool_dir
    }

    /// Path for a given `chunk_id` on disk.
    fn chunk_path(&self, chunk_id: &ChunkId) -> PathBuf {
        let hex = hex_encode_chunk(chunk_id);
        let prefix = &hex[..4.min(hex.len())];
        self.pool_dir.join("chunks").join(prefix).join(&hex)
    }
}

// ---------------------------------------------------------------------------
// Metadata cache
// ---------------------------------------------------------------------------

/// Cached file-to-`chunk_list` mapping with TTL.
struct MetadataEntry {
    chunk_list: Vec<ChunkId>,
    fetched_at: Instant,
}

/// Metadata cache with TTL-based expiry (I-CC3).
pub struct MetadataCache {
    entries: HashMap<String, MetadataEntry>,
    ttl: Duration,
}

impl MetadataCache {
    /// Create a new metadata cache with the given TTL.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// Get a cached chunk list if within TTL.
    #[must_use]
    pub fn get(&self, file_path: &str) -> Option<&[ChunkId]> {
        self.entries.get(file_path).and_then(|e| {
            if e.fetched_at.elapsed() <= self.ttl {
                Some(e.chunk_list.as_slice())
            } else {
                None
            }
        })
    }

    /// Insert or update a metadata entry (write-through).
    pub fn put(&mut self, file_path: String, chunk_list: Vec<ChunkId>) {
        self.entries.insert(
            file_path,
            MetadataEntry {
                chunk_list,
                fetched_at: Instant::now(),
            },
        );
    }

    /// Evict expired entries.
    pub fn evict_expired(&mut self) {
        let ttl = self.ttl;
        self.entries.retain(|_, e| e.fetched_at.elapsed() <= ttl);
    }

    /// Wipe all entries.
    pub fn wipe(&mut self) {
        self.entries.clear();
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Cache metrics
// ---------------------------------------------------------------------------

/// Cache statistics.
#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    /// L1 cache hits.
    pub l1_hits: u64,
    /// L2 cache hits.
    pub l2_hits: u64,
    /// Cache misses (bypassed to canonical).
    pub misses: u64,
    /// Bypass mode reads.
    pub bypasses: u64,
    /// L2 errors (CRC mismatch, I/O failure).
    pub errors: u64,
    /// L1 bytes used.
    pub l1_bytes: u64,
    /// L2 bytes used.
    pub l2_bytes: u64,
    /// Metadata cache hits.
    pub meta_hits: u64,
    /// Metadata cache misses.
    pub meta_misses: u64,
    /// Full cache wipes.
    pub wipes: u64,
}

// ---------------------------------------------------------------------------
// CacheManager — unified cache orchestrator
// ---------------------------------------------------------------------------

/// Unified cache manager orchestrating L1, L2, and metadata cache.
pub struct CacheManager {
    mode: CacheMode,
    l1: CacheL1,
    l2: Option<CacheL2>,
    meta: MetadataCache,
    stats: CacheStats,
}

impl CacheManager {
    /// Create a new cache manager with the given config.
    ///
    /// For `Bypass` mode, no L2 is created.
    pub fn new(config: &CacheConfig) -> std::io::Result<Self> {
        let l2 = if config.mode == CacheMode::Bypass {
            None
        } else {
            let pool_id = gen_pool_id();
            let pool_dir = config
                .cache_dir
                .join("default-tenant") // tenant set later
                .join(&pool_id);
            Some(CacheL2::open(pool_dir, config.max_cache_bytes)?)
        };

        Ok(Self {
            mode: config.mode,
            l1: CacheL1::new(config.max_memory_bytes),
            l2,
            meta: MetadataCache::new(config.metadata_ttl),
            stats: CacheStats::default(),
        })
    }

    /// Get a chunk from cache. Returns None on miss (caller fetches from canonical).
    pub fn get_chunk(&mut self, chunk_id: &ChunkId) -> Option<Vec<u8>> {
        if self.mode == CacheMode::Bypass {
            self.stats.bypasses += 1;
            return None;
        }

        // L1 check.
        if let Some(data) = self.l1.get(chunk_id) {
            self.stats.l1_hits += 1;
            return Some(data.to_vec());
        }

        // L2 check.
        if let Some(ref l2) = self.l2 {
            match l2.get(chunk_id) {
                Some(data) => {
                    self.stats.l2_hits += 1;
                    // Promote to L1.
                    let pinned = self.mode == CacheMode::Pinned;
                    self.l1.insert(*chunk_id, data.clone(), pinned);
                    return Some(data);
                }
                None if l2.contains(chunk_id) => {
                    // CRC mismatch — counted as error, already deleted by L2.get()
                    self.stats.errors += 1;
                }
                None => {}
            }
        }

        self.stats.misses += 1;
        None
    }

    /// Insert a chunk into cache after fetching from canonical.
    ///
    /// The caller is responsible for SHA-256 verification before calling this.
    pub fn put_chunk(&mut self, chunk_id: ChunkId, data: Vec<u8>) {
        if self.mode == CacheMode::Bypass {
            return;
        }

        let pinned = self.mode == CacheMode::Pinned;

        // Insert into L2.
        if let Some(ref mut l2) = self.l2 {
            if let Err(e) = l2.put(&chunk_id, &data) {
                tracing::warn!(error = %e, "L2 cache insert failed");
            }
        }

        // Insert into L1.
        self.l1.insert(chunk_id, data, pinned);
    }

    /// Get cached metadata (file->chunk_list).
    pub fn get_metadata(&mut self, file_path: &str) -> Option<Vec<ChunkId>> {
        if let Some(list) = self.meta.get(file_path) {
            self.stats.meta_hits += 1;
            Some(list.to_vec())
        } else {
            self.stats.meta_misses += 1;
            None
        }
    }

    /// Insert or update metadata (write-through).
    pub fn put_metadata(&mut self, file_path: String, chunk_list: Vec<ChunkId>) {
        self.meta.put(file_path, chunk_list);
    }

    /// Invalidate a specific chunk from all tiers.
    pub fn invalidate_chunk(&mut self, chunk_id: &ChunkId) {
        self.l1.remove(chunk_id);
        if let Some(ref mut l2) = self.l2 {
            l2.remove(chunk_id);
        }
    }

    /// Wipe entire cache — all tiers (I-CC6, I-CC8, I-CC12).
    pub fn wipe(&mut self) {
        self.l1.wipe();
        if let Some(ref mut l2) = self.l2 {
            l2.wipe();
        }
        self.meta.wipe();
        self.stats.wipes += 1;
        tracing::info!("cache wiped");
    }

    /// Current cache statistics.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let mut s = self.stats.clone();
        s.l1_bytes = self.l1.bytes_used();
        s.l2_bytes = self.l2.as_ref().map_or(0, CacheL2::bytes_used);
        s
    }

    /// Current cache mode.
    #[must_use]
    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    /// Evict expired metadata entries.
    pub fn evict_expired_metadata(&mut self) {
        self.meta.evict_expired();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a 128-bit pool ID as hex string.
fn gen_pool_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{nanos:032x}-{pid:08x}")
}

/// Hex-encode a chunk ID (first 16 bytes for brevity in logs).
fn hex_short(chunk_id: &ChunkId) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(16);
    for b in &chunk_id.0[..8] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Full hex encoding of a chunk ID.
fn hex_encode_chunk(chunk_id: &ChunkId) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in &chunk_id.0 {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Recursively compute directory size in bytes.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => match std::fs::metadata(entry.path()) {
                    Ok(m) => m.file_type(),
                    Err(_) => continue,
                },
            };
            if ft.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += entry.metadata().map_or(0, |m| m.len());
            }
        }
    }
    total
}

/// Recursively zeroize and delete all files in a directory (I-CC2).
fn wipe_directory(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                wipe_directory(&p);
                let _ = std::fs::remove_dir(&p);
            } else {
                // Overwrite with zeros before unlinking.
                if let Ok(meta) = std::fs::metadata(&p) {
                    let zeros = vec![0u8; meta.len() as usize];
                    let _ = std::fs::write(&p, &zeros);
                }
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Backward-compatible wrapper (used by acceptance tests)
// ---------------------------------------------------------------------------

/// Simple in-memory cache with TTL — backward-compatible API.
///
/// This wraps the L1 cache with a TTL check using wall-clock timestamps.
/// Used by existing acceptance tests. New code should use `CacheManager`.
pub struct ClientCache {
    entries: HashMap<ChunkId, (Vec<u8>, u64)>, // (data, cached_at_ms)
    ttl_ms: u64,
    max_entries: usize,
}

impl ClientCache {
    /// Create a new cache with the given TTL and max entries.
    #[must_use]
    pub fn new(ttl_ms: u64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ttl_ms,
            max_entries,
        }
    }

    /// Get a cached chunk if not expired.
    #[must_use]
    pub fn get(&self, chunk_id: &ChunkId, now_ms: u64) -> Option<&[u8]> {
        self.entries.get(chunk_id).and_then(|(data, cached_at)| {
            if now_ms.saturating_sub(*cached_at) <= self.ttl_ms {
                Some(data.as_slice())
            } else {
                None
            }
        })
    }

    /// Insert a chunk into the cache.
    pub fn insert(&mut self, chunk_id: ChunkId, data: Vec<u8>, now_ms: u64) {
        if self.entries.len() >= self.max_entries {
            // Evict oldest.
            if let Some(oldest_id) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, ts))| *ts)
                .map(|(id, _)| *id)
            {
                self.entries.remove(&oldest_id);
            }
        }
        self.entries.insert(chunk_id, (data, now_ms));
    }

    /// Invalidate a specific chunk.
    pub fn invalidate(&mut self, chunk_id: &ChunkId) {
        self.entries.remove(chunk_id);
    }

    /// Evict all expired entries.
    pub fn evict_expired(&mut self, now_ms: u64) {
        let ttl = self.ttl_ms;
        self.entries
            .retain(|_, (_, cached_at)| now_ms.saturating_sub(*cached_at) <= ttl);
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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

    // --- L1 tests ---

    #[test]
    fn l1_insert_and_get() {
        let mut l1 = CacheL1::new(1024);
        l1.insert(test_chunk_id(1), vec![10, 20, 30], false);
        assert_eq!(l1.get(&test_chunk_id(1)), Some(&[10u8, 20, 30][..]));
        assert_eq!(l1.len(), 1);
    }

    #[test]
    fn l1_miss_returns_none() {
        let mut l1 = CacheL1::new(1024);
        assert!(l1.get(&test_chunk_id(1)).is_none());
    }

    #[test]
    fn l1_evicts_lru_at_capacity() {
        let mut l1 = CacheL1::new(10); // very small
        l1.insert(test_chunk_id(1), vec![1; 5], false);
        l1.insert(test_chunk_id(2), vec![2; 5], false);
        // At capacity (10 bytes). Next insert evicts oldest.
        l1.insert(test_chunk_id(3), vec![3; 5], false);
        assert!(
            l1.get(&test_chunk_id(1)).is_none(),
            "oldest should be evicted"
        );
        assert!(l1.get(&test_chunk_id(3)).is_some());
    }

    #[test]
    fn l1_pinned_survives_eviction() {
        let mut l1 = CacheL1::new(10);
        l1.insert(test_chunk_id(1), vec![1; 5], true); // pinned
        l1.insert(test_chunk_id(2), vec![2; 5], false);
        // At capacity. Next insert can only evict non-pinned.
        l1.insert(test_chunk_id(3), vec![3; 5], false);
        assert!(l1.get(&test_chunk_id(1)).is_some(), "pinned should survive");
        assert!(l1.get(&test_chunk_id(2)).is_none(), "unpinned evicted");
    }

    #[test]
    fn l1_wipe_clears_all() {
        let mut l1 = CacheL1::new(1024);
        l1.insert(test_chunk_id(1), vec![1; 100], false);
        l1.insert(test_chunk_id(2), vec![2; 100], true);
        l1.wipe();
        assert!(l1.is_empty());
        assert_eq!(l1.bytes_used(), 0);
    }

    // --- CRC32 tests ---

    #[test]
    fn crc32_known_value() {
        // CRC32 of empty string is 0x00000000.
        assert_eq!(crc32(b""), 0x0000_0000);
        // CRC32 of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    // --- L2 tests ---

    #[test]
    fn l2_put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        let mut l2 = CacheL2::open(pool_dir, 1024 * 1024).unwrap();

        let id = test_chunk_id(0xAB);
        l2.put(&id, &[10, 20, 30]).unwrap();

        let data = l2.get(&id).unwrap();
        assert_eq!(data, vec![10, 20, 30]);
    }

    #[test]
    fn l2_crc_mismatch_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        let mut l2 = CacheL2::open(pool_dir, 1024 * 1024).unwrap();

        let id = test_chunk_id(0xCD);
        l2.put(&id, &[1, 2, 3]).unwrap();

        // Corrupt the file: flip a byte.
        let path = l2.chunk_path(&id);
        let mut raw = std::fs::read(&path).unwrap();
        raw[0] ^= 0xFF;
        std::fs::write(&path, &raw).unwrap();

        // Should detect corruption and return None.
        assert!(l2.get(&id).is_none());
        // File should be deleted.
        assert!(!path.exists());
    }

    #[test]
    fn l2_capacity_enforcement() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        let mut l2 = CacheL2::open(pool_dir, 20).unwrap(); // 20 bytes max

        let id1 = test_chunk_id(1);
        l2.put(&id1, &[0; 10]).unwrap(); // 10 + 4 = 14 bytes

        let id2 = test_chunk_id(2);
        let result = l2.put(&id2, &[0; 10]); // 14 + 14 = 28 > 20
        assert!(result.is_err());
    }

    #[test]
    fn l2_remove_zeroizes() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        let mut l2 = CacheL2::open(pool_dir, 1024 * 1024).unwrap();

        let id = test_chunk_id(0xEF);
        l2.put(&id, &[42; 100]).unwrap();
        let path = l2.chunk_path(&id);
        assert!(path.exists());

        l2.remove(&id);
        assert!(!path.exists());
    }

    #[test]
    fn l2_wipe_clears_all() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        let mut l2 = CacheL2::open(pool_dir.clone(), 1024 * 1024).unwrap();

        l2.put(&test_chunk_id(1), &[1; 50]).unwrap();
        l2.put(&test_chunk_id(2), &[2; 50]).unwrap();
        l2.wipe();
        assert_eq!(l2.bytes_used(), 0);
        assert!(l2.get(&test_chunk_id(1)).is_none());
    }

    // --- Metadata cache tests ---

    #[test]
    fn meta_within_ttl() {
        let mut meta = MetadataCache::new(Duration::from_secs(60));
        meta.put("/file.txt".into(), vec![test_chunk_id(1)]);
        let result = meta.get("/file.txt");
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn meta_expired_returns_none() {
        let mut meta = MetadataCache::new(Duration::from_millis(0));
        meta.put("/file.txt".into(), vec![test_chunk_id(1)]);
        // TTL=0 → immediately expired.
        std::thread::sleep(Duration::from_millis(1));
        assert!(meta.get("/file.txt").is_none());
    }

    #[test]
    fn meta_write_through() {
        let mut meta = MetadataCache::new(Duration::from_secs(60));
        meta.put("/file.txt".into(), vec![test_chunk_id(1)]);
        meta.put("/file.txt".into(), vec![test_chunk_id(2), test_chunk_id(3)]);
        let result = meta.get("/file.txt").unwrap();
        assert_eq!(result.len(), 2);
    }

    // --- CacheManager tests ---

    #[test]
    fn manager_bypass_mode() {
        let config = CacheConfig {
            mode: CacheMode::Bypass,
            ..CacheConfig::default()
        };
        let mut mgr = CacheManager::new(&config).unwrap();
        mgr.put_chunk(test_chunk_id(1), vec![1, 2, 3]);
        assert!(mgr.get_chunk(&test_chunk_id(1)).is_none());
        assert_eq!(mgr.stats().bypasses, 1);
    }

    #[test]
    fn manager_organic_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            mode: CacheMode::Organic,
            cache_dir: dir.path().to_path_buf(),
            ..CacheConfig::default()
        };
        let mut mgr = CacheManager::new(&config).unwrap();

        // Miss.
        assert!(mgr.get_chunk(&test_chunk_id(1)).is_none());
        assert_eq!(mgr.stats().misses, 1);

        // Insert + hit.
        mgr.put_chunk(test_chunk_id(1), vec![10, 20, 30]);
        let data = mgr.get_chunk(&test_chunk_id(1)).unwrap();
        assert_eq!(data, vec![10, 20, 30]);
        assert_eq!(mgr.stats().l1_hits, 1);
    }

    #[test]
    fn manager_wipe_clears_stats() {
        let dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            mode: CacheMode::Organic,
            cache_dir: dir.path().to_path_buf(),
            ..CacheConfig::default()
        };
        let mut mgr = CacheManager::new(&config).unwrap();
        mgr.put_chunk(test_chunk_id(1), vec![1; 100]);
        mgr.wipe();
        assert!(mgr.get_chunk(&test_chunk_id(1)).is_none());
        assert_eq!(mgr.stats().wipes, 1);
    }

    #[test]
    fn manager_metadata_hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            mode: CacheMode::Organic,
            cache_dir: dir.path().to_path_buf(),
            ..CacheConfig::default()
        };
        let mut mgr = CacheManager::new(&config).unwrap();

        // Miss.
        assert!(mgr.get_metadata("/file.txt").is_none());
        assert_eq!(mgr.stats().meta_misses, 1);

        // Insert + hit.
        mgr.put_metadata("/file.txt".into(), vec![test_chunk_id(1)]);
        let list = mgr.get_metadata("/file.txt").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(mgr.stats().meta_hits, 1);
    }

    // --- L2 capacity edge case ---

    #[test]
    fn l2_write_at_exact_capacity_succeeds_one_over_fails() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        // Max 14 bytes: 10 data + 4 CRC trailer = exactly one chunk.
        let mut l2 = CacheL2::open(pool_dir, 14).unwrap();

        let id1 = test_chunk_id(0xA1);
        l2.put(&id1, &[0; 10]).unwrap(); // 10 + 4 = 14 bytes = exactly at capacity

        // One byte more should fail.
        let id2 = test_chunk_id(0xA2);
        let result = l2.put(&id2, &[0; 1]); // 1 + 4 = 5 bytes, but 14 + 5 > 14
        assert!(result.is_err(), "write over capacity should fail");
    }

    // --- Bypass mode: put is no-op, get returns None ---

    #[test]
    fn bypass_mode_put_is_noop_get_returns_none() {
        let config = CacheConfig {
            mode: CacheMode::Bypass,
            ..CacheConfig::default()
        };
        let mut mgr = CacheManager::new(&config).unwrap();

        let chunk_id = test_chunk_id(0xBB);
        mgr.put_chunk(chunk_id, vec![1, 2, 3]);

        // get_chunk should return None in bypass mode.
        assert!(mgr.get_chunk(&chunk_id).is_none());

        // Stats should show bypasses, not misses.
        let stats = mgr.stats();
        assert_eq!(stats.bypasses, 1);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.l1_hits, 0);
    }

    /// I-CC8/I-CC12: Crypto-shred wipe clears both L1 and L2, and
    /// increments the wipe counter exactly once.
    #[test]
    fn crypto_shred_wipes_l1_and_l2() {
        let dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            mode: CacheMode::Organic,
            cache_dir: dir.path().to_path_buf(),
            ..CacheConfig::default()
        };
        let mut mgr = CacheManager::new(&config).unwrap();

        // Insert multiple chunks — they go into both L1 and L2.
        let id1 = test_chunk_id(0xC1);
        let id2 = test_chunk_id(0xC2);
        let id3 = test_chunk_id(0xC3);
        mgr.put_chunk(id1, vec![1; 100]);
        mgr.put_chunk(id2, vec![2; 200]);
        mgr.put_chunk(id3, vec![3; 300]);

        // Verify all chunks are readable from L1.
        assert!(mgr.get_chunk(&id1).is_some(), "id1 should be in cache");
        assert!(mgr.get_chunk(&id2).is_some(), "id2 should be in cache");
        assert!(mgr.get_chunk(&id3).is_some(), "id3 should be in cache");

        // Also insert metadata to verify metadata cache is wiped.
        mgr.put_metadata("/crypto-shred-test.dat".into(), vec![id1, id2]);
        assert!(mgr.get_metadata("/crypto-shred-test.dat").is_some());

        // Verify L1 and L2 have data before wipe.
        let pre_stats = mgr.stats();
        assert!(pre_stats.l1_bytes > 0, "L1 should have data before wipe");
        assert!(pre_stats.l2_bytes > 0, "L2 should have data before wipe");
        assert_eq!(pre_stats.wipes, 0, "no wipes yet");

        // Simulate crypto-shred: trigger a full wipe.
        mgr.wipe();

        // L1 must be empty.
        assert!(
            mgr.get_chunk(&id1).is_none(),
            "id1 must be gone after crypto-shred"
        );
        assert!(
            mgr.get_chunk(&id2).is_none(),
            "id2 must be gone after crypto-shred"
        );
        assert!(
            mgr.get_chunk(&id3).is_none(),
            "id3 must be gone after crypto-shred"
        );

        // Metadata must be wiped.
        // Note: get_metadata increments meta_misses, so check is_none.
        assert!(
            mgr.get_metadata("/crypto-shred-test.dat").is_none(),
            "metadata must be wiped after crypto-shred"
        );

        // Stats: L1 and L2 bytes should be 0, wipe counter should be 1.
        let post_stats = mgr.stats();
        assert_eq!(post_stats.l1_bytes, 0, "L1 bytes must be 0 after wipe");
        assert_eq!(post_stats.l2_bytes, 0, "L2 bytes must be 0 after wipe");
        assert_eq!(
            post_stats.wipes, 1,
            "wipe counter must be exactly 1 (I-CC8)"
        );
    }
}
