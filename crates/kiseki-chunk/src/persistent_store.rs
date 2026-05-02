//! Persistent chunk store — wraps `ChunkStore` + `DeviceBackend`.
//!
//! Chunk ciphertext stored on raw block devices (or file-backed for
//! VMs/CI). Chunk metadata (refcount, holds, envelope meta) stored
//! alongside the in-memory store and persisted via the device backend.
//!
//! Per ADR-029: bitmap allocator, per-extent CRC32, crash-safe writes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use kiseki_block::file::FileBackedDevice;
use kiseki_block::{DeviceBackend, Extent};
use kiseki_common::ids::ChunkId;
use kiseki_crypto::envelope::Envelope;

use crate::error::ChunkError;
use crate::pool::AffinityPool;
use crate::store::ChunkOps;

/// Compile-time assertion: `ChunkId` must be exactly 32 bytes.
const _: () = assert!(std::mem::size_of::<ChunkId>() == 32);

/// Metadata for a persisted chunk.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PersistedChunkMeta {
    chunk_id: [u8; 32],
    refcount: u64,
    retention_holds: Vec<String>,
    pool_name: String,
    stored_bytes: u64,
    /// Actual data length in bytes (distinct from extent-aligned `stored_bytes`).
    /// Used for accurate capacity accounting in pool usage.
    #[serde(default)]
    data_bytes: u64,
    /// Device extent where ciphertext + envelope is stored.
    extent_offset: u64,
    extent_length: u64,
    /// Serialized envelope metadata (nonce, `auth_tag`, epochs, etc.)
    /// Ciphertext is on the device; this is just the crypto fields.
    nonce: [u8; 12],
    auth_tag: [u8; 16],
    system_epoch: u64,
    tenant_epoch: Option<u64>,
    tenant_wrapped_material: Option<Vec<u8>>,
}

/// In-memory chunk entry for the persistent store.
struct ChunkEntry {
    envelope_meta: PersistedChunkMeta,
    extent: Extent,
}

/// Metadata for a persisted EC fragment. Distinct from
/// `PersistedChunkMeta` because EC fragments don't carry per-fragment
/// envelope crypto state — they're slices of one chunk's ciphertext
/// addressed by `(chunk_id, fragment_index)`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PersistedFragmentMeta {
    chunk_id: [u8; 32],
    fragment_index: u32,
    extent_offset: u64,
    extent_length: u64,
    data_bytes: u64,
}

struct FragmentEntry {
    meta: PersistedFragmentMeta,
    extent: Extent,
}

/// Persistent chunk store — in-memory index + device backend for data.
pub struct PersistentChunkStore {
    /// In-memory index: `chunk_id` → metadata + extent.
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
    /// Pools (same as in-memory `ChunkStore`).
    pools: Mutex<HashMap<String, AffinityPool>>,
    /// Device backend for chunk data storage.
    device: Box<dyn DeviceBackend>,
    /// Path to metadata file (JSON, for crash recovery).
    meta_path: std::path::PathBuf,
    /// Path to fragment metadata file. Defaults to `meta_path` with
    /// `.frag` appended — kept separate from the chunks file so the
    /// existing on-disk format stays back-compat for chunk-only
    /// deployments.
    frag_meta_path: std::path::PathBuf,
}

fn frag_path_for(meta: &Path) -> std::path::PathBuf {
    let mut s = meta.as_os_str().to_owned();
    s.push(".frag");
    std::path::PathBuf::from(s)
}

impl PersistentChunkStore {
    /// Initialize a new persistent chunk store.
    ///
    /// - `device_path`: path to the block device or file for chunk data
    /// - `meta_path`: path to the metadata JSON file (on system partition)
    /// - `device_size`: total device size in bytes
    pub fn init(
        device_path: &Path,
        meta_path: &Path,
        device_size: u64,
    ) -> Result<Self, ChunkError> {
        let device = FileBackedDevice::init(device_path, device_size)
            .map_err(|e| ChunkError::Io(e.to_string()))?;

        let store = Self {
            chunks: Mutex::new(HashMap::new()),
            fragments: Mutex::new(HashMap::new()),
            pools: Mutex::new(HashMap::new()),
            device: Box::new(device),
            meta_path: meta_path.to_owned(),
            frag_meta_path: frag_path_for(meta_path),
        };
        store.save_meta()?;
        store.save_frag_meta()?;
        Ok(store)
    }

    /// Open an existing persistent chunk store.
    pub fn open(device_path: &Path, meta_path: &Path) -> Result<Self, ChunkError> {
        let device =
            FileBackedDevice::open(device_path).map_err(|e| ChunkError::Io(e.to_string()))?;

        let chunks = if meta_path.exists() {
            let data =
                std::fs::read_to_string(meta_path).map_err(|e| ChunkError::Io(e.to_string()))?;
            let metas: Vec<PersistedChunkMeta> = serde_json::from_str(&data)
                .map_err(|e| ChunkError::Io(format!("metadata parse error: {e}")))?;
            let mut map = HashMap::new();
            for meta in metas {
                let chunk_id = ChunkId(meta.chunk_id);
                let extent = Extent::new(meta.extent_offset, meta.extent_length);
                map.insert(
                    chunk_id,
                    ChunkEntry {
                        envelope_meta: meta,
                        extent,
                    },
                );
            }
            map
        } else {
            HashMap::new()
        };

        let frag_meta_path = frag_path_for(meta_path);
        let fragments = if frag_meta_path.exists() {
            let data = std::fs::read_to_string(&frag_meta_path)
                .map_err(|e| ChunkError::Io(e.to_string()))?;
            let metas: Vec<PersistedFragmentMeta> = serde_json::from_str(&data)
                .map_err(|e| ChunkError::Io(format!("fragment metadata parse error: {e}")))?;
            let mut map = HashMap::new();
            for meta in metas {
                let chunk_id = ChunkId(meta.chunk_id);
                let extent = Extent::new(meta.extent_offset, meta.extent_length);
                let key = (chunk_id, meta.fragment_index);
                map.insert(key, FragmentEntry { meta, extent });
            }
            map
        } else {
            HashMap::new()
        };

        Ok(Self {
            chunks: Mutex::new(chunks),
            fragments: Mutex::new(fragments),
            pools: Mutex::new(HashMap::new()),
            device: Box::new(device),
            meta_path: meta_path.to_owned(),
            frag_meta_path,
        })
    }

    /// Add an affinity pool.
    pub fn add_pool(&self, pool: AffinityPool) {
        self.pools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pool.name.clone(), pool);
    }

    /// Save metadata to JSON file (atomic: write tmp then rename).
    fn save_meta(&self) -> Result<(), ChunkError> {
        let chunks = self.chunks.lock().unwrap_or_else(|e| {
            tracing::warn!("mutex poisoned in save_meta, recovering");
            e.into_inner()
        });
        let metas: Vec<&PersistedChunkMeta> = chunks.values().map(|e| &e.envelope_meta).collect();
        let json = serde_json::to_string(&metas).map_err(|e| ChunkError::Io(e.to_string()))?;
        let tmp_path = self.meta_path.with_extension("tmp");
        std::fs::write(&tmp_path, json).map_err(|e| ChunkError::Io(e.to_string()))?;
        std::fs::rename(&tmp_path, &self.meta_path).map_err(|e| ChunkError::Io(e.to_string()))?;
        Ok(())
    }

    /// Persist the fragment index. Same crash-safe write+rename
    /// pattern as `save_meta` but on `frag_meta_path`.
    fn save_frag_meta(&self) -> Result<(), ChunkError> {
        let fragments = self.fragments.lock().unwrap_or_else(|e| {
            tracing::warn!("mutex poisoned in save_frag_meta, recovering");
            e.into_inner()
        });
        let metas: Vec<&PersistedFragmentMeta> = fragments.values().map(|e| &e.meta).collect();
        let json = serde_json::to_string(&metas).map_err(|e| ChunkError::Io(e.to_string()))?;
        let tmp_path = self.frag_meta_path.with_extension("tmp");
        std::fs::write(&tmp_path, json).map_err(|e| ChunkError::Io(e.to_string()))?;
        std::fs::rename(&tmp_path, &self.frag_meta_path)
            .map_err(|e| ChunkError::Io(e.to_string()))?;
        Ok(())
    }

    /// Reconstruct an Envelope from persisted metadata + device data.
    fn reconstruct_envelope(
        &self,
        meta: &PersistedChunkMeta,
        extent: &Extent,
    ) -> Result<Envelope, ChunkError> {
        let ciphertext = self
            .device
            .read(extent)
            .map_err(|e| ChunkError::Io(e.to_string()))?;

        Ok(Envelope {
            ciphertext,
            auth_tag: meta.auth_tag,
            nonce: meta.nonce,
            system_epoch: kiseki_common::tenancy::KeyEpoch(meta.system_epoch),
            tenant_epoch: meta.tenant_epoch.map(kiseki_common::tenancy::KeyEpoch),
            tenant_wrapped_material: meta.tenant_wrapped_material.clone(),
            chunk_id: ChunkId(meta.chunk_id),
        })
    }
}

impl ChunkOps for PersistentChunkStore {
    fn write_chunk(&mut self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError> {
        let chunk_id = envelope.chunk_id;

        // Hold the chunks lock for the entire operation to prevent a race
        // where two concurrent writes for the same chunk_id both pass the
        // dedup check. The I/O is the bottleneck, not the lock.
        let mut chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Dedup: if chunk already exists, just bump refcount.
        if let Some(entry) = chunks.get_mut(&chunk_id) {
            entry.envelope_meta.refcount = entry
                .envelope_meta
                .refcount
                .checked_add(1)
                .ok_or_else(|| ChunkError::Io("refcount overflow".into()))?;
            drop(chunks);
            self.save_meta()?;
            return Ok(false);
        }

        // Allocate extent on device.
        let data = &envelope.ciphertext;
        let data_bytes = data.len() as u64;
        let extent = self
            .device
            .alloc(data.len() as u64)
            .map_err(|e| ChunkError::Io(e.to_string()))?;

        // Write ciphertext to device (includes CRC32).
        // If crash occurs between device write and metadata persist, the orphan
        // extent is detected and freed by periodic scrub (ADR-029 F-I6).
        self.device
            .write(&extent, data)
            .map_err(|e| ChunkError::Io(e.to_string()))?;

        // Build metadata.
        let meta = PersistedChunkMeta {
            chunk_id: chunk_id.0,
            refcount: 1,
            retention_holds: Vec::new(),
            pool_name: pool.to_owned(),
            stored_bytes: extent.length,
            data_bytes,
            extent_offset: extent.offset,
            extent_length: extent.length,
            nonce: envelope.nonce,
            auth_tag: envelope.auth_tag,
            system_epoch: envelope.system_epoch.0,
            tenant_epoch: envelope.tenant_epoch.map(|e| e.0),
            tenant_wrapped_material: envelope.tenant_wrapped_material.clone(),
        };

        // Update pool usage (use data_bytes for accurate capacity accounting).
        {
            let mut pools = self
                .pools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(p) = pools.get_mut(pool) {
                p.used_bytes += data_bytes;
            }
        }

        // Insert into in-memory index.
        chunks.insert(
            chunk_id,
            ChunkEntry {
                envelope_meta: meta,
                extent,
            },
        );

        drop(chunks);

        // Persist metadata + sync device.
        self.save_meta()?;
        self.device
            .sync()
            .map_err(|e| ChunkError::Io(e.to_string()))?;

        Ok(true)
    }

    fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError> {
        let chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = chunks
            .get(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        self.reconstruct_envelope(&entry.envelope_meta, &entry.extent)
    }

    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let mut chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.envelope_meta.refcount = entry
            .envelope_meta
            .refcount
            .checked_add(1)
            .ok_or_else(|| ChunkError::Io("refcount overflow".into()))?;
        let rc = entry.envelope_meta.refcount;
        drop(chunks);
        self.save_meta()?;
        Ok(rc)
    }

    fn decrement_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let mut chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        if entry.envelope_meta.refcount == 0 {
            return Err(ChunkError::RefcountUnderflow(*chunk_id));
        }
        entry.envelope_meta.refcount -= 1;
        let rc = entry.envelope_meta.refcount;
        drop(chunks);
        self.save_meta()?;
        Ok(rc)
    }

    fn set_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let mut chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        if !entry
            .envelope_meta
            .retention_holds
            .contains(&hold_name.to_owned())
        {
            entry
                .envelope_meta
                .retention_holds
                .push(hold_name.to_owned());
        }
        drop(chunks);
        self.save_meta()?;
        Ok(())
    }

    fn release_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let mut chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry
            .envelope_meta
            .retention_holds
            .retain(|h| h != hold_name);
        drop(chunks);
        self.save_meta()?;
        Ok(())
    }

    fn gc(&mut self) -> u64 {
        let mut chunks = self.chunks.lock().unwrap_or_else(|e| {
            tracing::warn!("mutex poisoned in gc, recovering");
            e.into_inner()
        });

        let to_remove: Vec<(ChunkId, Extent, String, u64)> = chunks
            .iter()
            .filter(|(_, e)| {
                e.envelope_meta.refcount == 0 && e.envelope_meta.retention_holds.is_empty()
            })
            .map(|(id, e)| {
                (
                    *id,
                    e.extent,
                    e.envelope_meta.pool_name.clone(),
                    e.envelope_meta.data_bytes,
                )
            })
            .collect();

        let mut freed_count: u64 = 0;

        for (id, extent, pool_name, data_bytes) in &to_remove {
            // Only remove chunk from metadata if device.free() succeeds.
            // If free fails, skip this chunk (leave it for next GC cycle).
            match self.device.free(extent) {
                Ok(()) => {
                    chunks.remove(id);
                    freed_count += 1;
                    // Update pool usage.
                    let mut pools = self.pools.lock().unwrap_or_else(|e| {
                        tracing::warn!("mutex poisoned in gc pool update, recovering");
                        e.into_inner()
                    });
                    if let Some(p) = pools.get_mut(pool_name.as_str()) {
                        p.used_bytes = p.used_bytes.saturating_sub(*data_bytes);
                    }
                }
                Err(e) => {
                    tracing::warn!(chunk_id = %id, error = %e, "gc free failed, skipping");
                }
            }
        }

        drop(chunks);
        let _ = self.save_meta();
        let _ = self.device.sync();

        freed_count
    }

    fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        chunks
            .get(chunk_id)
            .map(|e| e.envelope_meta.refcount)
            .ok_or(ChunkError::NotFound(*chunk_id))
    }

    /// Enumerate every chunk whose envelope metadata is currently
    /// loaded for this node. Used by the orphan-fragment scrub and by
    /// `/admin/chunk/{id}` to answer "is this chunk present locally?".
    fn list_chunk_ids(&self) -> Vec<ChunkId> {
        let chunks = self
            .chunks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        let old_extent = {
            let mut fragments = self
                .fragments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let old = fragments.remove(&key).map(|e| e.extent);
            let meta = PersistedFragmentMeta {
                chunk_id: chunk_id.0,
                fragment_index,
                extent_offset: extent.offset,
                extent_length: extent.length,
                data_bytes,
            };
            fragments.insert(key, FragmentEntry { meta, extent });
            old
        };
        if let Some(old) = old_extent {
            // Best-effort — if free fails, we leak an extent (the
            // periodic scrub will reclaim). Don't fail the write.
            let _ = self.device.free(&old);
        }
        self.save_frag_meta()?;
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
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            fragments.remove(&key)
        };
        let Some(entry) = removed else {
            return Ok(false);
        };
        let _ = self.device.free(&entry.extent);
        self.save_frag_meta()?;
        Ok(true)
    }

    fn delete_chunk_force(&mut self, chunk_id: &ChunkId) -> Result<bool, ChunkError> {
        let mut anything_removed = false;
        // Whole-envelope path (Replication-N + dedup, server.put_fragment
        // for fragment_index=0). Removes from chunks map AND frees the
        // device extent, bypassing refcount (test-only).
        let chunk_entry = {
            let mut chunks = self
                .chunks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            chunks.remove(chunk_id)
        };
        if let Some(entry) = chunk_entry {
            let _ = self.device.free(&entry.extent);
            anything_removed = true;
        }
        // Per-fragment path (EC, server.put_fragment for fragment_index>0).
        // Drain every (chunk_id, *) tuple.
        let frag_entries: Vec<_> = {
            let mut fragments = self
                .fragments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keys: Vec<_> = fragments
                .keys()
                .filter(|(c, _)| c == chunk_id)
                .copied()
                .collect();
            keys.into_iter()
                .filter_map(|k| fragments.remove(&k).map(|e| e.extent))
                .collect()
        };
        for extent in frag_entries {
            let _ = self.device.free(&extent);
            anything_removed = true;
        }
        if anything_removed {
            self.save_meta()?;
            self.save_frag_meta()?;
        }
        Ok(anything_removed)
    }

    fn list_fragments(&self, chunk_id: &ChunkId) -> Vec<u32> {
        let target = *chunk_id;
        let fragments = self
            .fragments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fragments
            .keys()
            .filter(|(cid, _)| *cid == target)
            .map(|(_, idx)| *idx)
            .collect()
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
}
