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
use kiseki_block::{DeviceBackend, Extent, MAX_EXTENT_PAYLOAD_BYTES};
use kiseki_common::ids::ChunkId;
use kiseki_crypto::envelope::Envelope;

use crate::error::ChunkError;
use crate::pool::AffinityPool;
use crate::store::ChunkOps;
use kiseki_common::locks::LockOrDie;

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
    /// First extent (legacy single-extent layout). For chunks that fit
    /// in a single extent, this is the only extent. Kept for
    /// backward-compat with metadata files written before Bug 5's
    /// multi-extent fix landed.
    extent_offset: u64,
    extent_length: u64,
    /// Additional extents holding the rest of the ciphertext, in
    /// order. Empty for single-extent chunks (the common case;
    /// ciphertext ≤ `MAX_EXTENT_PAYLOAD_BYTES`). Bug 5
    /// (GCP 2026-05-04): chunks larger than the per-extent cap
    /// silently corrupted; the fix splits oversize chunks across
    /// multiple extents.
    #[serde(default)]
    extra_extents: Vec<(u64, u64)>,
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
    /// All extents holding this chunk's ciphertext, in order.
    /// `extents[0]` is the legacy single extent; for chunks that
    /// exceed the per-extent cap, additional extents follow.
    extents: Vec<Extent>,
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
                let mut extents = vec![Extent::new(meta.extent_offset, meta.extent_length)];
                for &(off, len) in &meta.extra_extents {
                    extents.push(Extent::new(off, len));
                }
                map.insert(
                    chunk_id,
                    ChunkEntry {
                        envelope_meta: meta,
                        extents,
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
            .lock_or_die("persistent_store.pools")
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
    ///
    /// Reads each extent in order and concatenates the ciphertext.
    /// Single-extent chunks (the common case) read one extent; chunks
    /// that exceeded the per-extent cap at write time read all of them.
    fn reconstruct_envelope(
        &self,
        meta: &PersistedChunkMeta,
        extents: &[Extent],
    ) -> Result<Envelope, ChunkError> {
        let mut ciphertext: Vec<u8> =
            Vec::with_capacity(usize::try_from(meta.data_bytes).unwrap_or(0));
        for extent in extents {
            let part = self
                .device
                .read(extent)
                .map_err(|e| ChunkError::Io(e.to_string()))?;
            ciphertext.extend_from_slice(&part);
        }

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
        let mut chunks = self
            .chunks
            .lock()
            .lock_or_die("persistent_store.chunks");

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

        // Allocate + write ciphertext, splitting across multiple
        // extents if it exceeds the per-extent cap (Bug 5 fix). On
        // crash between writes and metadata persist, orphan extents
        // are reclaimed by periodic scrub (ADR-029 F-I6).
        let data = &envelope.ciphertext;
        let data_bytes = data.len() as u64;
        let extents = self.alloc_and_write_chunked(data)?;
        let stored_bytes: u64 = extents.iter().map(|e| e.length).sum();

        // Build metadata. The first extent goes into the legacy
        // `extent_offset/extent_length` pair; any additional extents
        // go into `extra_extents`. Old metadata files (single extent
        // only) deserialize unchanged.
        let first = &extents[0];
        let extra_extents: Vec<(u64, u64)> = extents[1..]
            .iter()
            .map(|e| (e.offset, e.length))
            .collect();
        let meta = PersistedChunkMeta {
            chunk_id: chunk_id.0,
            refcount: 1,
            retention_holds: Vec::new(),
            pool_name: pool.to_owned(),
            stored_bytes,
            data_bytes,
            extent_offset: first.offset,
            extent_length: first.length,
            extra_extents,
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
                .lock_or_die("persistent_store.pools");
            if let Some(p) = pools.get_mut(pool) {
                p.used_bytes += data_bytes;
            }
        }

        // Insert into in-memory index.
        chunks.insert(
            chunk_id,
            ChunkEntry {
                envelope_meta: meta,
                extents,
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
            .lock_or_die("persistent_store.chunks");
        let entry = chunks
            .get(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        self.reconstruct_envelope(&entry.envelope_meta, &entry.extents)
    }

    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let mut chunks = self
            .chunks
            .lock()
            .lock_or_die("persistent_store.chunks");
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
            .lock_or_die("persistent_store.chunks");
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
            .lock_or_die("persistent_store.chunks");
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
            .lock_or_die("persistent_store.chunks");
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

        let to_remove: Vec<(ChunkId, Vec<Extent>, String, u64)> = chunks
            .iter()
            .filter(|(_, e)| {
                e.envelope_meta.refcount == 0 && e.envelope_meta.retention_holds.is_empty()
            })
            .map(|(id, e)| {
                (
                    *id,
                    e.extents.clone(),
                    e.envelope_meta.pool_name.clone(),
                    e.envelope_meta.data_bytes,
                )
            })
            .collect();

        let mut freed_count: u64 = 0;

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
            .lock_or_die("persistent_store.chunks");
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
            .lock_or_die("persistent_store.chunks");
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
                .lock_or_die("persistent_store.fragments");
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
                .lock_or_die("persistent_store.chunks");
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
        let frag_entries: Vec<_> = {
            let mut fragments = self
                .fragments
                .lock()
                .lock_or_die("persistent_store.fragments");
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
            .lock_or_die("persistent_store.fragments");
        fragments
            .keys()
            .filter(|(cid, _)| *cid == target)
            .map(|(_, idx)| *idx)
            .collect()
    }

    fn snapshot_pools(&self) -> Vec<crate::pool::AffinityPool> {
        self.pools
            .lock()
            .lock_or_die("persistent_store.pools")
            .values()
            .cloned()
            .collect()
    }

    fn add_pool_checked(&mut self, pool: crate::pool::AffinityPool) -> Result<(), String> {
        let mut g = self
            .pools
            .lock()
            .lock_or_die("persistent_store.pools");
        if g.contains_key(&pool.name) {
            return Err(format!("pool {} already exists", pool.name));
        }
        g.insert(pool.name.clone(), pool);
        Ok(())
    }

    fn add_device(
        &mut self,
        pool_name: &str,
        device: crate::pool::PoolDevice,
    ) -> Result<(), String> {
        let mut g = self
            .pools
            .lock()
            .lock_or_die("persistent_store.pools");
        let pool = g
            .get_mut(pool_name)
            .ok_or_else(|| format!("pool {pool_name} not found"))?;
        if pool.devices.iter().any(|d| d.id == device.id) {
            return Err(format!("device {} already in pool {pool_name}", device.id));
        }
        pool.devices.push(device);
        Ok(())
    }

    fn remove_device(&mut self, device_id: &str) -> Result<(), String> {
        let mut g = self
            .pools
            .lock()
            .lock_or_die("persistent_store.pools");
        for pool in g.values_mut() {
            if let Some(idx) = pool.devices.iter().position(|d| d.id == device_id) {
                pool.devices.remove(idx);
                return Ok(());
            }
        }
        Err(format!("device {device_id} not found"))
    }

    fn set_pool_durability(
        &mut self,
        pool_name: &str,
        strategy: crate::pool::DurabilityStrategy,
    ) -> Result<(), String> {
        let mut g = self
            .pools
            .lock()
            .lock_or_die("persistent_store.pools");
        let pool = g
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
