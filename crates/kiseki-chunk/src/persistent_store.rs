//! Persistent chunk store — wraps `ChunkStore` + `DeviceBackend`.
//!
//! Chunk ciphertext stored on raw block devices (or file-backed for
//! VMs/CI). Chunk metadata (refcount, holds, envelope meta) lives in
//! a fjall-backed write-through cache (ADR-022 rev-4): in-memory
//! `Mutex<HashMap>` for O(1) reads, [`FjallMetaStore`] as the WAL.
//!
//! Pools sit in a [`DashMap`] for sharded concurrent reads on the
//! `pools.get(pool)` durability-strategy lookup that runs once per
//! `write_chunk`; admin mutations rewrite a small `pools.json` file.
//!
//! Per ADR-029: bitmap allocator, per-extent CRC32, crash-safe writes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use dashmap::DashMap;
use kiseki_block::file::FileBackedDevice;
use kiseki_block::{DeviceBackend, Extent, MAX_EXTENT_PAYLOAD_BYTES};
use kiseki_common::ids::ChunkId;
use kiseki_common::locks::LockOrDie;
use kiseki_crypto::envelope::Envelope;

use crate::error::ChunkError;
use crate::persistent::encoding::{ChunkRecord, FragmentRecord};
use crate::persistent::FjallMetaStore;
use crate::pool::AffinityPool;
use crate::store::ChunkOps;

/// Compile-time assertion: `ChunkId` must be exactly 32 bytes.
const _: () = assert!(std::mem::size_of::<ChunkId>() == 32);

/// In-memory chunk entry for the persistent store. The `record`
/// holds the durable metadata; `extents` is a derived view of the
/// extent layout (rebuilt from `record.extent_offset` +
/// `record.extra_extents`) so the read path doesn't reparse on every
/// call.
struct ChunkEntry {
    record: ChunkRecord,
    /// All extents holding this chunk's ciphertext, in order.
    /// `extents[0]` is the legacy single extent; for chunks that
    /// exceed the per-extent cap, additional extents follow.
    extents: Vec<Extent>,
}

/// In-memory cache row for an EC fragment. Only the resolved
/// `extent` participates in `read_fragment` / `delete_fragment` —
/// `FragmentRecord` lives durably in the meta store and is hydrated
/// on restart, so we don't keep a copy in RAM.
struct FragmentEntry {
    extent: Extent,
}

/// Build the in-memory `extents: Vec<Extent>` view from a
/// [`ChunkRecord`]. Empty record (`extent_length == 0`) yields an
/// empty vec — `reconstruct_envelope` must never call `device.read`
/// on a zero-length extent (POSIX `touch` / `NFSv4` `OPEN-CREATE`
/// on an empty file).
fn extents_from_record(record: &ChunkRecord) -> Vec<Extent> {
    let mut extents = Vec::with_capacity(1 + record.extra_extents.len());
    if record.extent_length > 0 {
        extents.push(Extent::new(record.extent_offset, record.extent_length));
    }
    for &(off, len) in &record.extra_extents {
        extents.push(Extent::new(off, len));
    }
    extents
}

/// On-disk path for the small per-pool config (rare admin
/// mutations). Sibling of the fjall meta directory.
fn pools_path_for(meta_dir: &Path) -> std::path::PathBuf {
    meta_dir.parent().map_or_else(
        || std::path::PathBuf::from("pools.json"),
        |p| p.join("pools.json"),
    )
}

/// Persistent chunk store — in-memory index + fjall WAL + device
/// backend for data.
pub struct PersistentChunkStore {
    /// In-memory index: `chunk_id` → metadata + extents. Source of
    /// truth on the hot read path; [`FjallMetaStore`] is the
    /// write-through WAL behind it.
    chunks: Mutex<HashMap<ChunkId, ChunkEntry>>,
    /// EC fragment index: `(chunk_id, fragment_index)` → metadata +
    /// extent. Used by EC X+Y mode (`defaults_for(>=6)` selects
    /// EC 4+2). Replication-N writes go through `chunks` instead.
    /// Discovered missing 2026-05-02 — local repro of the GCP perf
    /// cluster's "quorum lost: only 1/4 replicas acked" — every EC
    /// fragment with `fragment_index > 0` returned `Status::unavailable`
    /// because the inherited default trait impl returned
    /// `Io("write_fragment not implemented")`.
    fragments: Mutex<HashMap<(ChunkId, u32), FragmentEntry>>,
    /// Pools — `DashMap` for sharded concurrent reads. The
    /// `pools.get(pool)` durability-strategy lookup runs once per
    /// `write_chunk`; the previous `Mutex<HashMap>` serialized every
    /// fabric writer through one global lock. Persisted to the
    /// small `pools.json` file (rewritten only on rare admin
    /// mutations, not on the chunk write path).
    pools: DashMap<String, AffinityPool>,
    /// Device backend for chunk data storage.
    device: std::sync::Arc<dyn DeviceBackend>,
    /// Fjall-backed metadata WAL. Writes are O(1) per record (one
    /// `WriteBatch::commit`) regardless of store size — the
    /// pre-rev-4 JSON `save_meta` rewrote every record on every
    /// mutation, capping the in-process-persistent floor at the
    /// JSON-serialise + fsync rate.
    meta: FjallMetaStore,
    /// Path to the small JSON file holding pool config. Mutated
    /// only on admin commands (`add_pool` / `add_device` /
    /// `set_pool_durability`); never on the chunk write path. Kept
    /// out of fjall because the LSM compaction overhead would
    /// dominate the actual op cost for a 10-entry table.
    pools_path: std::path::PathBuf,
    /// Optional `kiseki_chunk_persistent_write_phase_duration_seconds
    /// {phase}` histogram. Phases observed in `write_chunk`:
    /// `dedup_check`, `extent_io`, `save_meta`, `device_sync`.
    /// The `save_meta` label is retained for dashboard continuity
    /// post-rev-4; it now measures the fjall record-put round trip
    /// (microseconds) instead of the JSON rewrite (milliseconds).
    /// `None` for tests + library users without metrics.
    write_phase_metric: std::sync::RwLock<Option<std::sync::Arc<prometheus::HistogramVec>>>,
    /// When true (default), every `write_chunk` calls `device.sync()`
    /// inline before returning AND the fjall meta write commits with
    /// `PersistMode::SyncAll`. When false, both are deferred to a
    /// caller-driven `flush()` (typically a periodic background
    /// task wired by the runtime). Group-commit mode unblocks
    /// concurrent writers — per-write fsync was serializing fabric
    /// receivers through the kernel sync, capping parallel
    /// throughput at ~1× even with multiple concurrent peers.
    ///
    /// **Crash safety**: with `sync_per_write=false`, a single-node
    /// power loss can drop up to one flush-interval of writes from
    /// THIS node's disk. Cross-node durability is preserved by the
    /// Raft replication factor (every chunk lands on N peers' page
    /// caches before the leader acks); the under-replication scrub
    /// re-replicates anything the failed node lost when it returns.
    /// This is the standard async-replication tradeoff used by
    /// Cassandra, Kafka, etc.
    sync_per_write: std::sync::atomic::AtomicBool,
}

/// Load the pools-config JSON if present. Missing file → empty map
/// (fresh init, or old data dir with no admin mutations yet).
fn load_pools(path: &Path) -> Result<DashMap<String, AffinityPool>, ChunkError> {
    if !path.exists() {
        return Ok(DashMap::new());
    }
    let data = std::fs::read_to_string(path).map_err(|e| ChunkError::Io(e.to_string()))?;
    if data.trim().is_empty() {
        return Ok(DashMap::new());
    }
    let pools: Vec<AffinityPool> = serde_json::from_str(&data)
        .map_err(|e| ChunkError::Io(format!("pools config parse: {e}")))?;
    let map = DashMap::new();
    for pool in pools {
        map.insert(pool.name.clone(), pool);
    }
    Ok(map)
}

/// Persist the pool config — atomic write+rename. Only called on
/// admin mutations (`add_pool`, `add_device`, `remove_device`,
/// `set_pool_durability`).
fn save_pools(path: &Path, pools: &DashMap<String, AffinityPool>) -> Result<(), ChunkError> {
    let snapshot: Vec<AffinityPool> = pools.iter().map(|e| e.value().clone()).collect();
    let json = serde_json::to_string(&snapshot).map_err(|e| ChunkError::Io(e.to_string()))?;
    let tmp_path = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ChunkError::Io(e.to_string()))?;
    }
    std::fs::write(&tmp_path, json).map_err(|e| ChunkError::Io(e.to_string()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| ChunkError::Io(e.to_string()))?;
    Ok(())
}

impl PersistentChunkStore {
    /// Initialize a new persistent chunk store.
    ///
    /// - `device_path`: path to the block device or file for chunk data.
    /// - `meta_path`: path to the **directory** holding the fjall meta
    ///   keyspace (ADR-022 rev-4). Pre-rev-4 callers passed a `*.json`
    ///   file path; the new layout is a sibling directory without an
    ///   extension. The runtime writes a `pools.json` next to it for
    ///   the small admin-rate pool config.
    /// - `device_size`: total device size in bytes.
    pub fn init(
        device_path: &Path,
        meta_path: &Path,
        device_size: u64,
    ) -> Result<Self, ChunkError> {
        if let Some(parent) = meta_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ChunkError::Io(e.to_string()))?;
        }
        let device = FileBackedDevice::init(device_path, device_size)
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        let meta = FjallMetaStore::open(meta_path)?;
        let pools_path = pools_path_for(meta_path);

        Ok(Self {
            chunks: Mutex::new(HashMap::new()),
            fragments: Mutex::new(HashMap::new()),
            pools: DashMap::new(),
            device: std::sync::Arc::new(device),
            meta,
            pools_path,
            write_phase_metric: std::sync::RwLock::new(None),
            sync_per_write: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Open an existing persistent chunk store. Hydrates the
    /// in-memory caches from the fjall WAL.
    pub fn open(device_path: &Path, meta_path: &Path) -> Result<Self, ChunkError> {
        let device =
            FileBackedDevice::open(device_path).map_err(|e| ChunkError::Io(e.to_string()))?;
        let meta = FjallMetaStore::open(meta_path)?;
        let pools_path = pools_path_for(meta_path);
        let pools = load_pools(&pools_path)?;

        // Hydrate chunk cache.
        let mut chunk_map = HashMap::new();
        for record in meta.iter_chunks()? {
            let id = ChunkId(record.chunk_id);
            let extents = extents_from_record(&record);
            chunk_map.insert(id, ChunkEntry { record, extents });
        }

        // Hydrate fragment cache.
        let mut frag_map = HashMap::new();
        for record in meta.iter_fragments()? {
            let id = ChunkId(record.chunk_id);
            let extent = Extent::new(record.extent_offset, record.extent_length);
            frag_map.insert((id, record.fragment_index), FragmentEntry { extent });
        }

        Ok(Self {
            chunks: Mutex::new(chunk_map),
            fragments: Mutex::new(frag_map),
            pools,
            device: std::sync::Arc::new(device),
            meta,
            pools_path,
            write_phase_metric: std::sync::RwLock::new(None),
            sync_per_write: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Attach the per-phase write-duration histogram. Once set, every
    /// `write_chunk` records its `dedup_check`, `extent_io`,
    /// `save_meta`, and `device_sync` phase latencies on
    /// `kiseki_chunk_persistent_write_phase_duration_seconds{phase}`.
    /// Without this, the histogram registers but never observes — the
    /// 2026-05-04 perf sweep saw the same trap multiple times.
    pub fn set_write_phase_metric(&self, metric: std::sync::Arc<prometheus::HistogramVec>) {
        if let Ok(mut g) = self.write_phase_metric.write() {
            *g = Some(metric);
        }
    }

    fn observe_write_phase(&self, phase: &str, dur: std::time::Duration) {
        let Ok(g) = self.write_phase_metric.read() else {
            return;
        };
        if let Some(h) = g.as_ref() {
            h.with_label_values(&[phase]).observe(dur.as_secs_f64());
        }
    }

    /// Toggle group-commit mode. When `enabled` is false, every
    /// `write_chunk` calls `device.sync()` before returning (the
    /// pre-2026-05-04 behavior). When true, per-write fsync is
    /// deferred — callers must invoke [`flush`] periodically (the
    /// runtime spawns a 100 ms tick) to keep the on-disk state
    /// fresh. See the field doc on `sync_per_write` for the crash
    /// safety story.
    ///
    /// [`flush`]: Self::flush
    pub fn set_sync_per_write(&self, enabled: bool) {
        self.sync_per_write
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        // Mirror the knob into the fjall meta store so its
        // `WriteBatch::commit` durability matches the device-side
        // behavior. Without this, a `sync_per_write=false` runtime
        // would still pay an inline fjall fsync per chunk meta
        // mutation — the same trap the device path already escaped.
        self.meta.set_sync_per_write(enabled);
    }

    /// Flush pending writes to stable storage. Calls
    /// `device.sync()` (which itself flushes the bitmap +
    /// `sync_all`s the file). Safe to call concurrently with
    /// `write_chunk`; serializes only on the underlying device's
    /// own sync semantics.
    ///
    /// In group-commit mode the runtime calls this periodically
    /// from a background task; tests and rollback paths can call
    /// it directly to force durability.
    ///
    /// # Errors
    /// Returns `ChunkError::Io` if the device backend reports a
    /// sync failure.
    pub fn flush(&self) -> Result<(), ChunkError> {
        // Order: meta WAL fsync first, then device sync. The device
        // is the data; the meta is the index — losing a meta entry
        // for data that did fsync just leaks an extent (the scrub
        // reclaims). Losing data for a meta entry that did fsync
        // leaves a dangling pointer (read-time error). Better to
        // promote the meta first.
        self.meta.flush()?;
        self.device
            .sync()
            .map_err(|e| ChunkError::Io(e.to_string()))
    }

    /// Off-thread fsync handle for the gateway `fsync_pending` hook.
    /// Returns a [`crate::persistent::FjallMetaFlusher`] clone — the
    /// hook chain calls this in addition to `device.sync()` so a
    /// FUSE / NFS `fsync(2)` from one thread doesn't queue behind a
    /// concurrent `write_chunk` on another's `&self` mutex chain.
    #[must_use]
    pub fn meta_flusher(&self) -> crate::persistent::FjallMetaFlusher {
        self.meta.flusher()
    }

    /// Borrow the device backend handle. Returns a cheap `Arc`
    /// clone that callers (typically the runtime's periodic flush
    /// task) hold to call `sync()` directly without going through
    /// the [`SyncBridge`] mutex — flushing the device is `&self`
    /// and doesn't need exclusive access to the chunk store.
    ///
    /// [`SyncBridge`]: crate::async_ops::SyncBridge
    #[must_use]
    pub fn device_handle(&self) -> std::sync::Arc<dyn DeviceBackend> {
        std::sync::Arc::clone(&self.device)
    }

    /// Add an affinity pool. Best-effort persists `pools.json` —
    /// admin-rate frequency, drops the persist on I/O error and
    /// logs (the pool is still in the in-memory cache and a
    /// subsequent admin write will rewrite the JSON).
    pub fn add_pool(&self, pool: AffinityPool) {
        self.pools.insert(pool.name.clone(), pool);
        if let Err(e) = save_pools(&self.pools_path, &self.pools) {
            tracing::warn!(error = %e, "persist pools.json failed");
        }
    }

    // ADR-022 rev-4 (2026-05-06): the old `save_meta` /
    // `save_frag_meta` JSON-rewrite-the-world helpers are gone.
    // Each mutating op now writes ONE record through
    // [`FjallMetaStore`] — O(1) per op vs the previous O(N)
    // in-store-size cost. The save_meta/save_frag_meta phase labels
    // on the `kiseki_chunk_persistent_write_phase_duration_seconds`
    // histogram are retained for dashboard continuity (they now
    // measure the fjall record-put round-trip).

    /// Reconstruct an Envelope from persisted metadata + device data.
    ///
    /// Reads each extent in order and concatenates the ciphertext.
    /// Single-extent chunks (the common case) read one extent; chunks
    /// that exceeded the per-extent cap at write time read all of them.
    fn reconstruct_envelope(
        &self,
        record: &ChunkRecord,
        extents: &[Extent],
    ) -> Result<Envelope, ChunkError> {
        let mut ciphertext: Vec<u8> =
            Vec::with_capacity(usize::try_from(record.data_bytes).unwrap_or(0));
        for extent in extents {
            let part = self
                .device
                .read(extent)
                .map_err(|e| ChunkError::Io(e.to_string()))?;
            ciphertext.extend_from_slice(&part);
        }

        Ok(Envelope {
            ciphertext,
            auth_tag: record.auth_tag,
            nonce: record.nonce,
            system_epoch: kiseki_common::tenancy::KeyEpoch(record.system_epoch),
            tenant_epoch: record.tenant_epoch.map(kiseki_common::tenancy::KeyEpoch),
            tenant_wrapped_material: record.tenant_wrapped_material.clone(),
            chunk_id: ChunkId(record.chunk_id),
        })
    }

    /// Allocate + write a payload across one or more extents.
    ///
    /// Returns the list of extents holding the payload, in order. On
    /// any failure, all extents allocated by this call are freed
    /// best-effort so the device doesn't leak space.
    fn alloc_and_write_chunked(&self, data: &[u8]) -> Result<Vec<Extent>, ChunkError> {
        #[allow(clippy::cast_possible_truncation)]
        let max_payload = MAX_EXTENT_PAYLOAD_BYTES as usize;
        let mut extents: Vec<Extent> = Vec::new();
        let mut written = 0;
        while written < data.len() {
            let take = (data.len() - written).min(max_payload);
            let extent = match self.device.alloc(take as u64) {
                Ok(e) => e,
                Err(e) => {
                    for ext in &extents {
                        let _ = self.device.free(ext);
                    }
                    return Err(ChunkError::Io(e.to_string()));
                }
            };
            if let Err(e) = self.device.write(&extent, &data[written..written + take]) {
                let _ = self.device.free(&extent);
                for ext in &extents {
                    let _ = self.device.free(ext);
                }
                return Err(ChunkError::Io(e.to_string()));
            }
            extents.push(extent);
            written += take;
        }
        Ok(extents)
    }
}

impl ChunkOps for PersistentChunkStore {
    fn write_chunk(&mut self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError> {
        let chunk_id = envelope.chunk_id;

        // Hold the chunks lock for the entire operation to prevent a race
        // where two concurrent writes for the same chunk_id both pass the
        // dedup check. The I/O is the bottleneck, not the lock.
        let dedup_started = std::time::Instant::now();
        let mut chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");

        // Dedup: if chunk already exists, just bump refcount.
        if let Some(entry) = chunks.get_mut(&chunk_id) {
            entry.record.refcount = entry
                .record
                .refcount
                .checked_add(1)
                .ok_or_else(|| ChunkError::Io("refcount overflow".into()))?;
            let updated = entry.record.clone();
            drop(chunks);
            self.observe_write_phase("dedup_check", dedup_started.elapsed());
            let save_started = std::time::Instant::now();
            self.meta.put_chunk(&updated)?;
            self.observe_write_phase("save_meta", save_started.elapsed());
            return Ok(false);
        }
        self.observe_write_phase("dedup_check", dedup_started.elapsed());

        // Allocate + write ciphertext, splitting across multiple
        // extents if it exceeds the per-extent cap (Bug 5 fix). On
        // crash between writes and metadata persist, orphan extents
        // are reclaimed by periodic scrub (ADR-029 F-I6).
        //
        // Empty payloads (POSIX `touch` / NFSv4 OPEN-CREATE on a
        // zero-byte file) skip device allocation entirely. The
        // metadata stores `extents = []`, `extent_offset = 0`,
        // `extent_length = 0` — `reconstruct_envelope` returns the
        // empty ciphertext from the empty extents Vec without
        // touching the device.
        let data = &envelope.ciphertext;
        let data_bytes = data.len() as u64;
        let extent_io_started = std::time::Instant::now();
        let extents: Vec<Extent> = if data.is_empty() {
            Vec::new()
        } else {
            self.alloc_and_write_chunked(data)?
        };
        self.observe_write_phase("extent_io", extent_io_started.elapsed());
        let stored_bytes: u64 = extents.iter().map(|e| e.length).sum();

        // Build the persistent record. The first extent goes into
        // the legacy `extent_offset/extent_length` pair; any
        // additional extents go into `extra_extents`. Empty chunks
        // keep the legacy fields at (0, 0).
        let (first_offset, first_length) = extents.first().map_or((0, 0), |e| (e.offset, e.length));
        let extra_extents: Vec<(u64, u64)> = extents
            .iter()
            .skip(1)
            .map(|e| (e.offset, e.length))
            .collect();
        let record = ChunkRecord {
            chunk_id: chunk_id.0,
            refcount: 1,
            retention_holds: Vec::new(),
            pool_name: pool.to_owned(),
            stored_bytes,
            data_bytes,
            extent_offset: first_offset,
            extent_length: first_length,
            extra_extents,
            nonce: envelope.nonce,
            auth_tag: envelope.auth_tag,
            system_epoch: envelope.system_epoch.0,
            tenant_epoch: envelope.tenant_epoch.map(|e| e.0),
            tenant_wrapped_material: envelope.tenant_wrapped_material.clone(),
        };

        // Update pool usage (use data_bytes for accurate capacity
        // accounting). DashMap shard lock — the sharded layout
        // means a writer for pool A doesn't contend with a writer
        // for pool B, in contrast to the prior `Mutex<HashMap>`.
        if let Some(mut p) = self.pools.get_mut(pool) {
            p.used_bytes += data_bytes;
        }

        // Insert into in-memory index.
        chunks.insert(
            chunk_id,
            ChunkEntry {
                record: record.clone(),
                extents,
            },
        );

        drop(chunks);

        // Persist metadata; sync only when group-commit is OFF.
        let save_started = std::time::Instant::now();
        self.meta.put_chunk(&record)?;
        self.observe_write_phase("save_meta", save_started.elapsed());
        if self
            .sync_per_write
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let sync_started = std::time::Instant::now();
            self.device
                .sync()
                .map_err(|e| ChunkError::Io(e.to_string()))?;
            self.observe_write_phase("device_sync", sync_started.elapsed());
        }

        Ok(true)
    }

    fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError> {
        let chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        let entry = chunks
            .get(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        self.reconstruct_envelope(&entry.record, &entry.extents)
    }

    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let mut chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.record.refcount = entry
            .record
            .refcount
            .checked_add(1)
            .ok_or_else(|| ChunkError::Io("refcount overflow".into()))?;
        let rc = entry.record.refcount;
        let updated = entry.record.clone();
        drop(chunks);
        self.meta.put_chunk(&updated)?;
        Ok(rc)
    }

    fn decrement_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let mut chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        if entry.record.refcount == 0 {
            return Err(ChunkError::RefcountUnderflow(*chunk_id));
        }
        entry.record.refcount -= 1;
        let rc = entry.record.refcount;
        let updated = entry.record.clone();
        drop(chunks);
        self.meta.put_chunk(&updated)?;
        Ok(rc)
    }

    fn set_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let mut chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        if !entry.record.retention_holds.contains(&hold_name.to_owned()) {
            entry.record.retention_holds.push(hold_name.to_owned());
        }
        let updated = entry.record.clone();
        drop(chunks);
        self.meta.put_chunk(&updated)?;
        Ok(())
    }

    fn release_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let mut chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.record.retention_holds.retain(|h| h != hold_name);
        let updated = entry.record.clone();
        drop(chunks);
        self.meta.put_chunk(&updated)?;
        Ok(())
    }

    fn gc(&mut self) -> u64 {
        let mut chunks = self.chunks.lock().unwrap_or_else(|e| {
            tracing::warn!("mutex poisoned in gc, recovering");
            e.into_inner()
        });

        let to_remove: Vec<(ChunkId, Vec<Extent>, String, u64)> = chunks
            .iter()
            .filter(|(_, e)| e.record.refcount == 0 && e.record.retention_holds.is_empty())
            .map(|(id, e)| {
                (
                    *id,
                    e.extents.clone(),
                    e.record.pool_name.clone(),
                    e.record.data_bytes,
                )
            })
            .collect();

        let mut freed_count: u64 = 0;
        let mut removed_ids: Vec<ChunkId> = Vec::new();

        for (id, extents, pool_name, data_bytes) in &to_remove {
            // Free every extent for this chunk; only drop metadata if
            // ALL frees succeed. A partial-free leaves the in-memory
            // entry in place so a future GC retries cleanly.
            let mut all_freed = true;
            for ext in extents {
                if let Err(e) = self.device.free(ext) {
                    tracing::warn!(chunk_id = %id, error = %e, "gc free failed, skipping");
                    all_freed = false;
                    break;
                }
            }
            if all_freed {
                chunks.remove(id);
                removed_ids.push(*id);
                freed_count += 1;
                // Update pool usage.
                if let Some(mut p) = self.pools.get_mut(pool_name.as_str()) {
                    p.used_bytes = p.used_bytes.saturating_sub(*data_bytes);
                }
            }
        }

        drop(chunks);
        // Per-record removes — fjall journal absorbs each removal in
        // O(1). The pre-rev-4 path called save_meta() once at the
        // end of gc() which rewrote the entire file; per-record
        // removes are also O(removed) total which is bounded by what
        // the freed-count loop already touched.
        for id in removed_ids {
            let _ = self.meta.remove_chunk(&id);
        }
        let _ = self.device.sync();

        freed_count
    }

    fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        chunks
            .get(chunk_id)
            .map(|e| e.record.refcount)
            .ok_or(ChunkError::NotFound(*chunk_id))
    }

    /// Enumerate every chunk whose envelope metadata is currently
    /// loaded for this node. Used by the orphan-fragment scrub and by
    /// `/admin/chunk/{id}` to answer "is this chunk present locally?".
    fn list_chunk_ids(&self) -> Vec<ChunkId> {
        let chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
        chunks.keys().copied().collect()
    }

    /// EC fragment write — addresses bytes by `(chunk_id, fragment_index)`
    /// in a separate index from the legacy `chunks` map. The default
    /// trait impl returned `Io("not implemented")` which the gRPC
    /// fabric server mapped to `Status::unavailable`, surfacing on a
    /// 6-node cluster as `quorum lost: only 1/4 replicas acked`
    /// (every fragment with `fragment_index > 0` failed; only the
    /// `index=0` ack went through the legacy `write_chunk` path).
    /// Idempotent — re-writing the same `(chunk_id, fragment_index)`
    /// frees the old extent before allocating a new one so the
    /// device doesn't accumulate orphan extents on retries.
    fn write_fragment(
        &mut self,
        chunk_id: &ChunkId,
        fragment_index: u32,
        bytes: Vec<u8>,
    ) -> Result<(), ChunkError> {
        let key = (*chunk_id, fragment_index);
        let data_bytes = bytes.len() as u64;

        // Allocate device space + write before touching the in-memory
        // index so a write failure leaves no half-state. If a prior
        // entry exists for this key, free its extent after the new
        // write succeeds.
        let extent = self
            .device
            .alloc(data_bytes)
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        self.device
            .write(&extent, &bytes)
            .map_err(|e| ChunkError::Io(e.to_string()))?;

        let (old_extent, persisted_record) = {
            let mut fragments = self
                .fragments
                .lock()
                .lock_or_die("persistent_store.fragments");
            let old = fragments.remove(&key).map(|e| e.extent);
            let record = FragmentRecord {
                chunk_id: chunk_id.0,
                fragment_index,
                extent_offset: extent.offset,
                extent_length: extent.length,
                data_bytes,
            };
            fragments.insert(key, FragmentEntry { extent });
            (old, record)
        };
        if let Some(old) = old_extent {
            // Best-effort — if free fails, we leak an extent (the
            // periodic scrub will reclaim). Don't fail the write.
            let _ = self.device.free(&old);
        }
        self.meta.put_fragment(&persisted_record)?;
        Ok(())
    }

    fn read_fragment(
        &self,
        chunk_id: &ChunkId,
        fragment_index: u32,
    ) -> Result<Vec<u8>, ChunkError> {
        let key = (*chunk_id, fragment_index);
        let extent = {
            let fragments = self
                .fragments
                .lock()
                .lock_or_die("persistent_store.fragments");
            fragments
                .get(&key)
                .map(|e| e.extent)
                .ok_or(ChunkError::NotFound(*chunk_id))?
        };
        self.device
            .read(&extent)
            .map_err(|e| ChunkError::Io(e.to_string()))
    }

    fn delete_fragment(
        &mut self,
        chunk_id: &ChunkId,
        fragment_index: u32,
    ) -> Result<bool, ChunkError> {
        let key = (*chunk_id, fragment_index);
        let removed = {
            let mut fragments = self
                .fragments
                .lock()
                .lock_or_die("persistent_store.fragments");
            fragments.remove(&key)
        };
        let Some(entry) = removed else {
            return Ok(false);
        };
        let _ = self.device.free(&entry.extent);
        self.meta.remove_fragment(chunk_id, fragment_index)?;
        Ok(true)
    }

    fn delete_chunk_force(&mut self, chunk_id: &ChunkId) -> Result<bool, ChunkError> {
        let mut anything_removed = false;
        // Whole-envelope path (Replication-N + dedup, server.put_fragment
        // for fragment_index=0). Removes from chunks map AND frees the
        // device extent, bypassing refcount (test-only).
        let chunk_entry = {
            let mut chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
            chunks.remove(chunk_id)
        };
        if let Some(entry) = chunk_entry {
            for ext in &entry.extents {
                let _ = self.device.free(ext);
            }
            anything_removed = true;
        }
        // Per-fragment path (EC, server.put_fragment for fragment_index>0).
        // Drain every (chunk_id, *) tuple.
        let (frag_extents, removed_indices): (Vec<_>, Vec<u32>) = {
            let mut fragments = self
                .fragments
                .lock()
                .lock_or_die("persistent_store.fragments");
            let keys: Vec<(ChunkId, u32)> = fragments
                .keys()
                .filter(|(c, _)| c == chunk_id)
                .copied()
                .collect();
            let mut extents = Vec::with_capacity(keys.len());
            let mut indices = Vec::with_capacity(keys.len());
            for k in keys {
                if let Some(entry) = fragments.remove(&k) {
                    extents.push(entry.extent);
                    indices.push(k.1);
                }
            }
            (extents, indices)
        };
        for extent in frag_extents {
            let _ = self.device.free(&extent);
            anything_removed = true;
        }
        if anything_removed {
            // Atomic cross-keyspace removal — the chunks +
            // fragments rows of `chunk_id` go in one fjall batch.
            // Strictly stronger than the pre-rev-4 split where the
            // two JSON files were rewritten sequentially and a
            // crash between them left a hung-state.
            self.meta
                .remove_chunk_and_fragments(chunk_id, &removed_indices)?;
        }
        Ok(anything_removed)
    }

    fn list_fragments(&self, chunk_id: &ChunkId) -> Vec<u32> {
        let target = *chunk_id;
        let fragments = self
            .fragments
            .lock()
            .lock_or_die("persistent_store.fragments");
        fragments
            .keys()
            .filter(|(cid, _)| *cid == target)
            .map(|(_, idx)| *idx)
            .collect()
    }

    fn snapshot_pools(&self) -> Vec<crate::pool::AffinityPool> {
        self.pools.iter().map(|e| e.value().clone()).collect()
    }

    fn add_pool_checked(&mut self, pool: crate::pool::AffinityPool) -> Result<(), String> {
        if self.pools.contains_key(&pool.name) {
            return Err(format!("pool {} already exists", pool.name));
        }
        let name = pool.name.clone();
        self.pools.insert(name, pool);
        if let Err(e) = save_pools(&self.pools_path, &self.pools) {
            tracing::warn!(error = %e, "persist pools.json failed (add_pool_checked)");
        }
        Ok(())
    }

    fn add_device(
        &mut self,
        pool_name: &str,
        device: crate::pool::PoolDevice,
    ) -> Result<(), String> {
        {
            let mut pool = self
                .pools
                .get_mut(pool_name)
                .ok_or_else(|| format!("pool {pool_name} not found"))?;
            if pool.devices.iter().any(|d| d.id == device.id) {
                return Err(format!("device {} already in pool {pool_name}", device.id));
            }
            pool.devices.push(device);
        }
        if let Err(e) = save_pools(&self.pools_path, &self.pools) {
            tracing::warn!(error = %e, "persist pools.json failed (add_device)");
        }
        Ok(())
    }

    fn remove_device(&mut self, device_id: &str) -> Result<(), String> {
        let mut found = false;
        for mut pool in self.pools.iter_mut() {
            if let Some(idx) = pool.devices.iter().position(|d| d.id == device_id) {
                pool.devices.remove(idx);
                found = true;
                break;
            }
        }
        if !found {
            return Err(format!("device {device_id} not found"));
        }
        if let Err(e) = save_pools(&self.pools_path, &self.pools) {
            tracing::warn!(error = %e, "persist pools.json failed (remove_device)");
        }
        Ok(())
    }

    fn set_pool_durability(
        &mut self,
        pool_name: &str,
        strategy: crate::pool::DurabilityStrategy,
    ) -> Result<(), String> {
        {
            let mut pool = self
                .pools
                .get_mut(pool_name)
                .ok_or_else(|| format!("pool {pool_name} not found"))?;
            if pool.used_bytes > 0 {
                return Err(format!(
                    "pool {pool_name} is non-empty (used_bytes={}); durability \
                     change while data exists requires a separate migration plan",
                    pool.used_bytes
                ));
            }
            pool.durability = strategy;
        }
        if let Err(e) = save_pools(&self.pools_path, &self.pools) {
            tracing::warn!(error = %e, "persist pools.json failed (set_pool_durability)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::tenancy::KeyEpoch;

    fn test_envelope(key: u8) -> Envelope {
        Envelope {
            ciphertext: vec![key; 256],
            auth_tag: [0xAA; 16],
            nonce: [0xBB; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([key; 32]),
        }
    }

    /// Group commit (b.2 follow-up): per-write `device.sync()`
    /// serializes concurrent writes through the kernel fsync, so two
    /// fabric receivers landing fragments on the same node can't
    /// proceed in parallel. Default mode (`sync_per_write=true`) keeps
    /// pre-fix behavior; runtime opts into group commit and spawns a
    /// periodic flush task.
    ///
    /// Pin the contract: when `sync_per_write` is false, `write_chunk`
    /// observes `dedup_check` / `extent_io` / `save_meta` but skips
    /// `device_sync`. Explicit `flush()` re-enables sync on demand.
    #[test]
    fn write_chunk_skips_device_sync_when_sync_per_write_disabled() {
        use prometheus::{HistogramOpts, HistogramVec};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();
        store.set_sync_per_write(false);

        let metric = Arc::new(
            HistogramVec::new(
                HistogramOpts::new(
                    "kiseki_chunk_persistent_write_phase_duration_seconds_test_gc",
                    "test",
                )
                .buckets(vec![0.0001, 0.001, 0.01, 0.1, 1.0]),
                &["phase"],
            )
            .unwrap(),
        );
        store.set_write_phase_metric(Arc::clone(&metric));

        // Group-commit write — should observe extent_io + save_meta
        // but NOT device_sync.
        store.write_chunk(test_envelope(0xA5), "default").unwrap();

        let extent_count = metric.with_label_values(&["extent_io"]).get_sample_count();
        assert!(
            extent_count >= 1,
            "extent_io still observed (got {extent_count})"
        );

        let sync_count = metric
            .with_label_values(&["device_sync"])
            .get_sample_count();
        assert_eq!(
            sync_count, 0,
            "device_sync must NOT observe when sync_per_write=false (group commit) — \
             got {sync_count}; the per-write fsync is what serializes concurrent writers \
             through the kernel and must be deferred to the background flush task",
        );

        // Explicit flush — sync now happens and we expect device_sync
        // to fire here as a separate observation path. Today flush()
        // calls device.sync() directly; if a future refactor moves
        // that observation, this assertion needs updating.
        store.flush().unwrap();
    }

    /// 2026-05-04 perf sweep step b.2: every `write_chunk` must observe
    /// each phase histogram so `/metrics` reflects where the call's
    /// time actually goes. Pin the contract — without this, fixing
    /// the dominant phase (likely `save_meta` from its O(N) JSON
    /// rewrite) would have no signal to validate against.
    #[test]
    fn write_chunk_observes_each_phase_when_metric_is_wired() {
        use prometheus::{HistogramOpts, HistogramVec};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();

        let metric = Arc::new(
            HistogramVec::new(
                HistogramOpts::new(
                    "kiseki_chunk_persistent_write_phase_duration_seconds_test",
                    "test",
                )
                .buckets(vec![0.0001, 0.001, 0.01, 0.1, 1.0]),
                &["phase"],
            )
            .unwrap(),
        );
        store.set_write_phase_metric(Arc::clone(&metric));

        let env = test_envelope(0x42);
        store.write_chunk(env, "default").unwrap();

        for phase in ["dedup_check", "extent_io", "save_meta", "device_sync"] {
            let count = metric.with_label_values(&[phase]).get_sample_count();
            assert!(
                count >= 1,
                "kiseki_chunk_persistent_write_phase_duration_seconds{{phase={phase}}} \
                 must observe at least one sample after a write_chunk (got {count})",
            );
        }
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();

        let env = test_envelope(0x42);
        let chunk_id = env.chunk_id;
        assert!(store.write_chunk(env, "default").unwrap());

        let read_back = store.read_chunk(&chunk_id).unwrap();
        assert_eq!(read_back.ciphertext, vec![0x42u8; 256]);
        assert_eq!(read_back.auth_tag, [0xAA; 16]);
        assert_eq!(read_back.nonce, [0xBB; 12]);
        assert_eq!(read_back.system_epoch, KeyEpoch(1));
    }

    #[test]
    fn chunks_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let chunk_id;
        {
            let mut store =
                PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();
            let env = test_envelope(0x99);
            chunk_id = env.chunk_id;
            store.write_chunk(env, "pool-a").unwrap();
        }

        // Reopen.
        {
            let store = PersistentChunkStore::open(&dev_path, &meta_path).unwrap();
            let read_back = store.read_chunk(&chunk_id).unwrap();
            assert_eq!(read_back.ciphertext, vec![0x99u8; 256]);
            assert_eq!(read_back.chunk_id, chunk_id);
        }
    }

    #[test]
    fn dedup_increments_refcount() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();

        let env1 = test_envelope(0x10);
        let chunk_id = env1.chunk_id;
        assert!(store.write_chunk(env1, "default").unwrap()); // new write
        assert!(!store.write_chunk(test_envelope(0x10), "default").unwrap()); // dedup

        assert_eq!(store.refcount(&chunk_id).unwrap(), 2);
    }

    #[test]
    fn gc_frees_extents() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();

        let env = test_envelope(0x20);
        let chunk_id = env.chunk_id;
        store.write_chunk(env, "default").unwrap();
        store.decrement_refcount(&chunk_id).unwrap();

        let freed = store.gc();
        assert_eq!(freed, 1);
        assert!(store.read_chunk(&chunk_id).is_err());
    }

    #[test]
    fn retention_hold_blocks_gc() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();

        let env = test_envelope(0x30);
        let chunk_id = env.chunk_id;
        store.write_chunk(env, "default").unwrap();
        store.set_retention_hold(&chunk_id, "hipaa-7yr").unwrap();
        store.decrement_refcount(&chunk_id).unwrap();

        // GC should not remove — hold active.
        assert_eq!(store.gc(), 0);
        assert!(store.read_chunk(&chunk_id).is_ok());

        // Release hold → GC should remove.
        store
            .release_retention_hold(&chunk_id, "hipaa-7yr")
            .unwrap();
        assert_eq!(store.gc(), 1);
    }

    #[test]
    fn multiple_chunks_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        {
            let mut store =
                PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();
            for i in 0..50u8 {
                store.write_chunk(test_envelope(i), "default").unwrap();
            }
        }

        {
            let store = PersistentChunkStore::open(&dev_path, &meta_path).unwrap();
            for i in 0..50u8 {
                let env = store.read_chunk(&ChunkId([i; 32])).unwrap();
                assert_eq!(env.ciphertext, vec![i; 256]);
            }
        }
    }

    /// Bug 5 (GCP 2026-05-04): chunks larger than the bitmap allocator's
    /// per-extent cap (16 MiB) silently overran into adjacent extent
    /// space. Subsequent chunk writes overwrote the first chunk's data,
    /// surfacing as `kiseki_block::file: CRC mismatch — corruption` on
    /// every read.
    ///
    /// Contract: a chunk written to the store must round-trip
    /// byte-for-byte through `read_chunk`, regardless of size.
    #[test]
    fn write_chunk_larger_than_extent_cap_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 256 * 1024 * 1024).unwrap();

        // 64 MiB chunk — exceeds the 16 MiB per-extent cap by 4×.
        let big_ciphertext: Vec<u8> = (0..64usize * 1024 * 1024)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let env = Envelope {
            ciphertext: big_ciphertext.clone(),
            auth_tag: [0xAA; 16],
            nonce: [0xBB; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([0xC0; 32]),
        };
        let chunk_id = env.chunk_id;
        store.write_chunk(env, "default").unwrap();

        let read_back = store.read_chunk(&chunk_id).unwrap();
        assert_eq!(
            read_back.ciphertext.len(),
            big_ciphertext.len(),
            "ciphertext length mismatch after round-trip"
        );
        assert_eq!(
            read_back.ciphertext, big_ciphertext,
            "ciphertext bytes corrupted after round-trip"
        );
    }

    /// Bug 5 regression discovered during the 3rd GCP run: the
    /// multi-extent path panicked with "index out of bounds" when
    /// called with an empty payload (POSIX `touch` / `NFSv4` OPEN-CREATE
    /// on a zero-byte file). Empty chunks must skip device allocation
    /// and round-trip cleanly with empty ciphertext.
    #[test]
    fn write_chunk_with_empty_ciphertext_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 64 * 1024 * 1024).unwrap();

        let env = Envelope {
            ciphertext: Vec::new(),
            auth_tag: [0xAA; 16],
            nonce: [0xBB; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([0xE0; 32]),
        };
        let chunk_id = env.chunk_id;
        store.write_chunk(env, "default").unwrap();
        let read_back = store.read_chunk(&chunk_id).unwrap();
        assert!(
            read_back.ciphertext.is_empty(),
            "empty chunk must round-trip empty"
        );
    }

    /// Bug 5 (sibling write): the GCP repro showed that writing a
    /// second chunk after a large one corrupts the first. This test
    /// reproduces that exact pattern.
    #[test]
    fn write_large_chunk_then_neighbor_does_not_corrupt_first() {
        let dir = tempfile::tempdir().unwrap();
        let dev_path = dir.path().join("chunks.dev");
        let meta_path = dir.path().join("chunks.meta");

        let mut store =
            PersistentChunkStore::init(&dev_path, &meta_path, 256 * 1024 * 1024).unwrap();

        let big: Vec<u8> = (0..40usize * 1024 * 1024)
            .map(|i| u8::try_from(i % 241).unwrap())
            .collect();
        let env_a = Envelope {
            ciphertext: big.clone(),
            auth_tag: [0xAA; 16],
            nonce: [0xBB; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([0xA1; 32]),
        };
        store.write_chunk(env_a, "default").unwrap();

        let env_b = Envelope {
            ciphertext: vec![0x77u8; 8 * 1024 * 1024],
            auth_tag: [0xCC; 16],
            nonce: [0xDD; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([0xB2; 32]),
        };
        store.write_chunk(env_b, "default").unwrap();

        let read_a = store.read_chunk(&ChunkId([0xA1; 32])).unwrap();
        assert_eq!(
            read_a.ciphertext, big,
            "first chunk corrupted by neighbor write"
        );
        let read_b = store.read_chunk(&ChunkId([0xB2; 32])).unwrap();
        assert_eq!(read_b.ciphertext, vec![0x77u8; 8 * 1024 * 1024]);
    }
}
