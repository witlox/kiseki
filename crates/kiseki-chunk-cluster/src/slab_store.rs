//! Placement-aware [`SlabStore`] implementation — ADR-048 §"Slab
//! structure" + §"Read path".
//!
//! Strategy: a slab is a "giant chunk on the chunk fabric". The
//! encoder serialises the [`Slab`] (header + extent table + data)
//! into one byte buffer, packs it into a synthetic [`Envelope`] whose
//! `chunk_id` is `(slab_id.uuid bytes // 16 zero bytes)`, and fans it
//! to placement nodes via the existing
//! [`ClusteredChunkStore::write_chunk_ec`] path. Read path is the
//! mirror: [`read_chunk_ec`] reconstructs the buffer, the decoder
//! parses the slab back.
//!
//! Per-extent refcount + slab GC live in a node-local fjall keyspace
//! at `<data_dir>/slabs/refs`. The compactor maintains it as part of
//! the migration commit (apply path), so on restart the live extents
//! are recovered from durable state.
//!
//! Replication semantics: the slab's EC fragments are scattered
//! across placement nodes exactly like a regular EC chunk. EC's
//! min_acks guarantees the slab is reconstructible from the placement
//! set even with `parity_shards` failures (I-SE5).

#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::manual_let_else,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::question_mark,
    clippy::unused_async
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kiseki_chunk::error::ChunkError;
use kiseki_chunk::slab::{Slab, SlabBacklog, SlabExtent, SlabHeader, SlabStore};
use kiseki_chunk::EcEncoded;
use kiseki_common::ids::ChunkId;
use kiseki_common::SlabId;
use kiseki_crypto::envelope::Envelope;

use crate::ClusteredChunkStore;

/// Serialise a [`Slab`] into the byte stream a [`FabricSlabStore`]
/// fans across placement nodes. Layout:
///
/// ```text
/// [slab header     : 32 ]
/// [extent count u32 LE : 4 ]   <- duplicates header.extent_count for parser robustness
/// [extents         : 48 × N]
/// [data            : header.original_len bytes]
/// ```
///
/// Stable for the slab's lifetime; bumping the wire shape requires a
/// new `SLAB_FORMAT_VERSION`.
#[must_use]
fn serialise_slab(slab: &Slab) -> Vec<u8> {
    let extents_bytes_per = 48; // chunk_id(32) + offset(8) + length(4) + refcount(4)
    let cap = 32 + 4 + extents_bytes_per * slab.extents.len() + slab.data.len();
    let mut out = Vec::with_capacity(cap);
    // Header (32 B).
    out.extend_from_slice(&slab.header.version.to_le_bytes());
    out.extend_from_slice(&slab.header.data_shards.to_le_bytes());
    out.extend_from_slice(&slab.header.parity_shards.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&slab.header.byte_size.to_le_bytes());
    out.extend_from_slice(&slab.header.extent_count.to_le_bytes());
    out.extend_from_slice(&slab.header.original_len.to_le_bytes());
    // Extent count (redundant safety net).
    #[allow(clippy::cast_possible_truncation)] // bounded by DEFAULT_SLAB_MAX_CHUNKS
    out.extend_from_slice(&(slab.extents.len() as u32).to_le_bytes());
    // Extents.
    for e in &slab.extents {
        out.extend_from_slice(&e.chunk_id.0);
        out.extend_from_slice(&e.offset.to_le_bytes());
        out.extend_from_slice(&e.length.to_le_bytes());
        out.extend_from_slice(&e.refcount.to_le_bytes());
    }
    // Data.
    out.extend_from_slice(&slab.data);
    out
}

/// Deserialise the byte stream [`serialise_slab`] produced. Returns
/// [`ChunkError::SlabNotFound`] when the payload is truncated or
/// malformed — the read path treats this as "fragments survived but
/// metadata corrupt" and surfaces the slab id verbatim so operators
/// can correlate.
fn deserialise_slab(slab_id: SlabId, bytes: &[u8]) -> Result<Slab, ChunkError> {
    if bytes.len() < 32 + 4 {
        return Err(ChunkError::SlabNotFound(slab_id.0));
    }
    let mut pos = 0;
    let version = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
    pos += 2;
    let data_shards = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
    pos += 2;
    let parity_shards = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
    pos += 2;
    pos += 2; // reserved
    let byte_size = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let extent_count = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let original_len = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let count = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    if (count as u64) != extent_count {
        // header / table-count mismatch — defensive bail. Trust the
        // inner count (header is metadata; the table is the truth).
    }
    let extents_size = 48 * count;
    if pos + extents_size + original_len as usize > bytes.len() {
        return Err(ChunkError::SlabNotFound(slab_id.0));
    }
    let mut extents = Vec::with_capacity(count);
    for _ in 0..count {
        let mut cid = [0u8; 32];
        cid.copy_from_slice(&bytes[pos..pos + 32]);
        pos += 32;
        let offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let length = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let refcount = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;
        extents.push(SlabExtent {
            chunk_id: ChunkId(cid),
            offset,
            length,
            refcount,
        });
    }
    let data_end = pos + original_len as usize;
    let data = bytes[pos..data_end].to_vec();
    Ok(Slab {
        id: slab_id,
        header: SlabHeader {
            version,
            data_shards,
            parity_shards,
            byte_size,
            extent_count: count as u64,
            original_len,
        },
        extents,
        data,
    })
}

/// Encode a [`SlabId`] into a [`ChunkId`] for the fabric path. We use
/// the slab's UUID bytes in the high 16 bytes + a fixed marker in the
/// low 16 so a slab id never collides with a content-addressed chunk
/// id (the latter is a `sha256` output spanning the full 32 bytes).
#[must_use]
fn slab_id_as_chunk_id(slab_id: SlabId) -> ChunkId {
    let mut out = [0u8; 32];
    out[0..16].copy_from_slice(slab_id.0.as_bytes());
    // Marker bytes "SLAB" + zeros in the second half; tells the
    // chunk-id inspector this isn't a content-addressed chunk.
    out[16..20].copy_from_slice(b"SLAB");
    ChunkId(out)
}

/// Persistent slab-refcount index. Keyed by `slab_id` (16 B) →
/// concatenated 48 B [`SlabExtent`] records (without the data buffer;
/// the data lives on the fabric fragments). Used by GC and the
/// maintenance pass to decide when to delete a slab outright vs
/// rewrite it. The fjall API matches what the composition store uses
/// (`Database::builder().open()` + `keyspace()`).
struct SlabRefIndex {
    db: fjall::Database,
    table: fjall::Keyspace,
    /// ADR-048 §"Slab GC" rewrite-pass cross-ref: per-slab list of
    /// `(composition_id, chunk_idx, chunk_id)`. The compactor
    /// records this at slab-flush time; the maintenance rewrite pass
    /// reads it to emit `MigrateChunkLocations` deltas against the
    /// actual owning compositions when it rewrites a fragmented
    /// slab.
    owners_table: fjall::Keyspace,
}

const KS_SLAB_REFS: &str = "slab_refs";
const KS_SLAB_OWNERS: &str = "slab_owners";

/// Wire size of one `(composition_id, chunk_idx, chunk_id)` owner
/// record on disk: 16 (UUID) + 4 (chunk_idx u32 LE) + 32 (chunk id).
const SLAB_OWNER_RECORD_LEN: usize = 16 + 4 + 32;

impl SlabRefIndex {
    fn open(data_dir: &Path) -> Result<Self, ChunkError> {
        let path: PathBuf = data_dir.join("slabs");
        std::fs::create_dir_all(&path).map_err(|e| ChunkError::Io(e.to_string()))?;
        let db = fjall::Database::builder(&path)
            .open()
            .map_err(ChunkError::Fjall)?;
        let table = db
            .keyspace(KS_SLAB_REFS, fjall::KeyspaceCreateOptions::default)
            .map_err(ChunkError::Fjall)?;
        let owners_table = db
            .keyspace(KS_SLAB_OWNERS, fjall::KeyspaceCreateOptions::default)
            .map_err(ChunkError::Fjall)?;
        Ok(Self {
            db,
            table,
            owners_table,
        })
    }

    /// Record the per-slab owner index. Idempotent: subsequent calls
    /// with the same `slab_id` overwrite, which is the right semantic
    /// — the compactor only calls this on a fresh slab id.
    fn put_owners(&self, slab_id: SlabId, owners: &[SlabOwnerRecord]) -> Result<(), ChunkError> {
        let mut buf = Vec::with_capacity(owners.len() * SLAB_OWNER_RECORD_LEN);
        for (cid, idx, chunk_id) in owners {
            buf.extend_from_slice(cid.0.as_bytes());
            buf.extend_from_slice(&idx.to_le_bytes());
            buf.extend_from_slice(&chunk_id.0);
        }
        self.owners_table
            .insert(slab_id.0.as_bytes(), buf)
            .map_err(ChunkError::Fjall)?;
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(ChunkError::Fjall)?;
        Ok(())
    }

    /// Read the per-slab owner index. Returns `None` when no record
    /// has been written (pre-amendment slabs or maintenance running
    /// against a slab the compactor didn't tag).
    fn get_owners(&self, slab_id: SlabId) -> Result<Option<Vec<SlabOwnerRecord>>, ChunkError> {
        let raw = self
            .owners_table
            .get(slab_id.0.as_bytes())
            .map_err(ChunkError::Fjall)?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        if raw.len() % SLAB_OWNER_RECORD_LEN != 0 {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(raw.len() / SLAB_OWNER_RECORD_LEN);
        let mut pos = 0;
        while pos + SLAB_OWNER_RECORD_LEN <= raw.len() {
            let comp_uuid =
                uuid::Uuid::from_slice(&raw[pos..pos + 16]).unwrap_or_else(|_| uuid::Uuid::nil());
            pos += 16;
            let idx = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let mut cid = [0u8; 32];
            cid.copy_from_slice(&raw[pos..pos + 32]);
            pos += 32;
            out.push((
                kiseki_common::ids::CompositionId(comp_uuid),
                idx,
                ChunkId(cid),
            ));
        }
        Ok(Some(out))
    }

    /// Remove the per-slab owner index. Called from
    /// [`SlabRefIndex::delete`] so a GC'd slab leaves no orphan
    /// owner rows behind.
    #[allow(dead_code)] // surfaced for symmetry; SlabRefIndex::delete inlines the same removal.
    fn delete_owners(&self, slab_id: SlabId) -> Result<(), ChunkError> {
        self.owners_table
            .remove(slab_id.0.as_bytes())
            .map_err(ChunkError::Fjall)?;
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(ChunkError::Fjall)?;
        Ok(())
    }

    fn put(&self, slab_id: SlabId, extents: &[SlabExtent]) -> Result<(), ChunkError> {
        let mut buf = Vec::with_capacity(extents.len() * 48);
        for e in extents {
            buf.extend_from_slice(&e.chunk_id.0);
            buf.extend_from_slice(&e.offset.to_le_bytes());
            buf.extend_from_slice(&e.length.to_le_bytes());
            buf.extend_from_slice(&e.refcount.to_le_bytes());
        }
        self.table
            .insert(slab_id.0.as_bytes(), buf)
            .map_err(ChunkError::Fjall)?;
        // Persist immediately — slab durability is gated on this.
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(ChunkError::Fjall)?;
        Ok(())
    }

    fn get(&self, slab_id: SlabId) -> Result<Option<Vec<SlabExtent>>, ChunkError> {
        let raw = self
            .table
            .get(slab_id.0.as_bytes())
            .map_err(ChunkError::Fjall)?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        if raw.len() % 48 != 0 {
            return Ok(None);
        }
        let mut extents = Vec::with_capacity(raw.len() / 48);
        let mut pos = 0;
        while pos + 48 <= raw.len() {
            let mut cid = [0u8; 32];
            cid.copy_from_slice(&raw[pos..pos + 32]);
            pos += 32;
            let offset = u64::from_le_bytes(raw[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let length = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let refcount = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap());
            pos += 4;
            extents.push(SlabExtent {
                chunk_id: ChunkId(cid),
                offset,
                length,
                refcount,
            });
        }
        Ok(Some(extents))
    }

    fn delete(&self, slab_id: SlabId) -> Result<(), ChunkError> {
        self.table
            .remove(slab_id.0.as_bytes())
            .map_err(ChunkError::Fjall)?;
        // Clear the owner cross-ref so a GC'd slab leaves no orphan
        // rows in the secondary index.
        self.owners_table
            .remove(slab_id.0.as_bytes())
            .map_err(ChunkError::Fjall)?;
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(ChunkError::Fjall)?;
        Ok(())
    }

    fn ids(&self) -> Result<Vec<SlabId>, ChunkError> {
        let mut ids = Vec::new();
        for entry in self.table.iter() {
            let (k, _v) = entry.into_inner().map_err(ChunkError::Fjall)?;
            if k.len() == 16 {
                let uuid = uuid::Uuid::from_slice(&k).unwrap_or_else(|_| uuid::Uuid::nil());
                ids.push(SlabId(uuid));
            }
        }
        Ok(ids)
    }
}

/// Placement-aware production [`SlabStore`]. Fans slab fragments via
/// the existing [`ClusteredChunkStore`] EC path; persists the
/// per-extent refcount index in `<data_dir>/slabs/refs`.
pub struct FabricSlabStore {
    cluster: Arc<ClusteredChunkStore>,
    placement_nodes: Vec<u64>,
    ec_strategy: crate::ec::EcStrategy,
    refs: SlabRefIndex,
    /// Pool the slab fragments land in. Drives capacity + placement.
    pool: String,
}

impl FabricSlabStore {
    /// Wire a fresh slab store.
    ///
    /// # Errors
    /// Surfaces fjall errors from opening `<data_dir>/slabs/refs`.
    pub fn open(
        cluster: Arc<ClusteredChunkStore>,
        placement_nodes: Vec<u64>,
        ec_strategy: crate::ec::EcStrategy,
        data_dir: &Path,
        pool: String,
    ) -> Result<Self, ChunkError> {
        let refs = SlabRefIndex::open(data_dir)?;
        Ok(Self {
            cluster,
            placement_nodes,
            ec_strategy,
            refs,
            pool,
        })
    }

    /// Snapshot the live slab refcount table. Used by the
    /// maintenance rewrite pass to find fragmented slabs and by the
    /// admin reporting endpoint.
    pub fn refcount_snapshot(&self) -> Result<Vec<(SlabId, Vec<SlabExtent>)>, ChunkError> {
        let ids = self.refs.ids()?;
        let mut out = Vec::with_capacity(ids.len());
        for sid in ids {
            if let Some(ext) = self.refs.get(sid)? {
                out.push((sid, ext));
            }
        }
        Ok(out)
    }

    /// Record the per-slab owner cross-ref. Called by the compactor
    /// after a successful `put_slab` so the maintenance rewrite pass
    /// can emit `MigrateChunkLocations` against the right
    /// compositions when it rewrites a fragmented slab.
    pub fn record_owners(
        &self,
        slab_id: SlabId,
        owners: &[SlabOwnerRecord],
    ) -> Result<(), ChunkError> {
        self.refs.put_owners(slab_id, owners)
    }

    /// Read the per-slab owner cross-ref. Returns an empty vec when
    /// the slab has no recorded owners (pre-amendment record or test
    /// fixture).
    pub fn owners_for(&self, slab_id: SlabId) -> Result<Vec<SlabOwnerRecord>, ChunkError> {
        Ok(self.refs.get_owners(slab_id)?.unwrap_or_default())
    }

    /// Build the synthetic [`Envelope`] the fabric layer expects.
    /// `auth_tag` / `nonce` are zeroed — the slab payload is *not*
    /// AEAD-sealed at this layer (the chunks inside the slab are
    /// already sealed per-chunk during the hot-tier write). The EC
    /// encoder operates on opaque bytes.
    fn synthetic_envelope(slab_id: SlabId, serialised: Vec<u8>) -> Envelope {
        Envelope {
            ciphertext: serialised,
            auth_tag: [0u8; 16],
            nonce: [0u8; 12],
            system_epoch: kiseki_common::KeyEpoch(0),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: slab_id_as_chunk_id(slab_id),
        }
    }
}

/// ADR-048 §"Slab GC" maintenance-pass hook. Splits out the bits the
/// maintenance pass needs (`refcount_snapshot`, `get_slab`,
/// `put_slab_dyn`) so it can run against a `&dyn` without the
/// generic-laden full [`SlabStore`] trait.
/// Owner record on the maintenance-pass cross-ref: `(composition_id,
/// chunk_idx, chunk_id)` — the surviving extents' Composition owners
/// the rewrite pass uses to address `MigrateChunkLocations` deltas.
pub type SlabOwnerRecord = (
    kiseki_common::ids::CompositionId,
    u32,
    kiseki_common::ids::ChunkId,
);

/// ADR-048 §"Slab GC" rewrite-pass surface — the bits the
/// maintenance pass needs behind a `&dyn` so it can run against
/// either the production [`FabricSlabStore`] or a test stub. Splits
/// out from the chunk-store-side [`SlabStore`] trait whose default
/// `record_owners` is a no-op; the maintenance pass needs the real
/// implementation.
pub trait SlabStoreMaintainable: Send + Sync {
    /// Snapshot the live refcount table — `(slab_id, extents)` for
    /// every slab known to the local node.
    fn refcount_snapshot(
        &self,
    ) -> Result<Vec<(kiseki_common::SlabId, Vec<kiseki_chunk::slab::SlabExtent>)>, ChunkError>;

    /// Fetch a slab through the cold-tier path. Same shape as
    /// [`SlabStore::get_slab`] but available behind a `&dyn`.
    fn get_slab(
        &self,
        slab_id: kiseki_common::SlabId,
    ) -> Result<kiseki_chunk::slab::Slab, ChunkError>;

    /// Persist a freshly-encoded slab. Same shape as
    /// [`SlabStore::put_slab`] but available behind a `&dyn`.
    fn put_slab_dyn(
        &self,
        slab: kiseki_chunk::slab::Slab,
        encoded: kiseki_chunk::EcEncoded,
    ) -> Result<(), ChunkError>;

    /// Per-slab owner cross-ref, populated by the compactor's
    /// `record_owners` call after a successful `put_slab`. Empty
    /// vec when no record exists (pre-amendment slab or test fixture).
    /// The maintenance rewrite pass uses this to emit
    /// `MigrateChunkLocations` against the actual owning
    /// compositions.
    fn owners_for(
        &self,
        slab_id: kiseki_common::SlabId,
    ) -> Result<Vec<SlabOwnerRecord>, ChunkError>;

    /// Record the per-slab owner cross-ref. Idempotent overwrite on
    /// the same `slab_id` (compactor only calls this on a fresh
    /// id).
    fn record_owners(
        &self,
        slab_id: kiseki_common::SlabId,
        owners: &[SlabOwnerRecord],
    ) -> Result<(), ChunkError>;
}

impl SlabStoreMaintainable for FabricSlabStore {
    fn refcount_snapshot(
        &self,
    ) -> Result<Vec<(kiseki_common::SlabId, Vec<kiseki_chunk::slab::SlabExtent>)>, ChunkError> {
        FabricSlabStore::refcount_snapshot(self)
    }
    fn get_slab(
        &self,
        slab_id: kiseki_common::SlabId,
    ) -> Result<kiseki_chunk::slab::Slab, ChunkError> {
        <Self as SlabStore>::get_slab(self, slab_id)
    }
    fn put_slab_dyn(
        &self,
        slab: kiseki_chunk::slab::Slab,
        encoded: kiseki_chunk::EcEncoded,
    ) -> Result<(), ChunkError> {
        <Self as SlabStore>::put_slab(self, slab, encoded)
    }
    fn owners_for(
        &self,
        slab_id: kiseki_common::SlabId,
    ) -> Result<Vec<SlabOwnerRecord>, ChunkError> {
        FabricSlabStore::owners_for(self, slab_id)
    }
    fn record_owners(
        &self,
        slab_id: kiseki_common::SlabId,
        owners: &[SlabOwnerRecord],
    ) -> Result<(), ChunkError> {
        FabricSlabStore::record_owners(self, slab_id, owners)
    }
}

impl SlabStore for FabricSlabStore {
    fn put_slab(&self, slab: Slab, _encoded: EcEncoded) -> Result<(), ChunkError> {
        let slab_id = slab.id;
        let extents = slab.extents.clone();
        let payload = serialise_slab(&slab);
        let original_len = payload.len() as u64;
        let env = Self::synthetic_envelope(slab_id, payload);
        // pick_placement against the slab's chunk_id so subsequent
        // GETs deterministically address the same placement set.
        let chunk_id_for_placement = slab_id_as_chunk_id(slab_id);
        let target_copies = match self.ec_strategy {
            crate::ec::EcStrategy::Replication { copies } => copies as usize,
            crate::ec::EcStrategy::Ec { data, parity } => (data + parity) as usize,
        };
        let placement = crate::placement::pick_placement(
            &chunk_id_for_placement,
            &self.placement_nodes,
            target_copies,
        );
        let cluster = Arc::clone(&self.cluster);
        let pool = self.pool.clone();
        let strategy = self.ec_strategy;
        // Bridge into the chunk-cluster's tokio runtime via block_on
        // (SlabStore is a sync trait; same pattern the gateway uses
        // through `SyncBridge`).
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                cluster
                    .write_chunk_ec(env, &placement, strategy, &pool)
                    .await
            })
        });
        if let Err(e) = result {
            return Err(e);
        }
        // Record the per-extent refcount table in the durable index.
        // Stash the original_len in the refcount blob's reserved
        // header? Simpler: encode it in the slab header recovered at
        // read time. Skip for now; original_len is already in the
        // serialised slab payload.
        let _ = original_len;
        self.refs.put(slab_id, &extents)?;
        Ok(())
    }

    fn get_slab(&self, slab_id: SlabId) -> Result<Slab, ChunkError> {
        let chunk_id = slab_id_as_chunk_id(slab_id);
        let target_copies = match self.ec_strategy {
            crate::ec::EcStrategy::Replication { copies } => copies as usize,
            crate::ec::EcStrategy::Ec { data, parity } => (data + parity) as usize,
        };
        let placement =
            crate::placement::pick_placement(&chunk_id, &self.placement_nodes, target_copies);
        let cluster = Arc::clone(&self.cluster);
        let strategy = self.ec_strategy;
        let env = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                cluster
                    .read_chunk_ec(&chunk_id, &placement, strategy, None)
                    .await
            })
        })?;
        let mut slab = deserialise_slab(slab_id, &env.ciphertext)?;
        // Overlay the live refcount index (read-side authority — the
        // fabric copy's refcount snapshot is stale the moment the
        // compactor commits an extent decrement).
        if let Some(live) = self.refs.get(slab_id)? {
            for live_e in live {
                if let Some(e) = slab
                    .extents
                    .iter_mut()
                    .find(|e| e.chunk_id == live_e.chunk_id)
                {
                    e.refcount = live_e.refcount;
                }
            }
        }
        Ok(slab)
    }

    fn decrement_refcount(&self, slab_id: SlabId, chunk_id: ChunkId) -> Result<(), ChunkError> {
        let mut extents = self
            .refs
            .get(slab_id)?
            .ok_or(ChunkError::SlabNotFound(slab_id.0))?;
        if let Some(e) = extents.iter_mut().find(|e| e.chunk_id == chunk_id) {
            e.refcount = e.refcount.saturating_sub(1);
            self.refs.put(slab_id, &extents)?;
        }
        Ok(())
    }

    fn gc_slab(&self, slab_id: SlabId) -> Result<(), ChunkError> {
        let Some(extents) = self.refs.get(slab_id)? else {
            return Ok(());
        };
        let all_zero = extents.iter().all(|e| e.refcount == 0);
        if !all_zero {
            return Ok(());
        }
        // Delete fabric fragments. The chunk store's delete path
        // releases per-fragment storage on every placement node.
        let chunk_id = slab_id_as_chunk_id(slab_id);
        let cluster = Arc::clone(&self.cluster);
        let _ = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                cluster
                    .delete_distributed(&chunk_id, kiseki_common::ids::OrgId(uuid::Uuid::nil()))
                    .await
            })
        });
        self.refs.delete(slab_id)?;
        Ok(())
    }

    fn record_owners(&self, slab_id: SlabId, owners: &[SlabOwnerRecord]) -> Result<(), ChunkError> {
        FabricSlabStore::record_owners(self, slab_id, owners)
    }
}

/// ADR-048 §"Backpressure" + I-SE6 — a per-pool [`SlabBacklog`]
/// surface the runtime hands the gateway so writes can be gated on
/// compactor health. Each compactor task owns its own backlog
/// tracker; the runtime keeps a registry indexed by pool name.
#[derive(Default)]
pub struct SlabBacklogRegistry {
    inner: parking_lot::RwLock<
        std::collections::HashMap<String, Arc<parking_lot::Mutex<SlabBacklog>>>,
    >,
}

impl SlabBacklogRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or insert a backlog tracker for the given pool.
    pub fn get_or_insert(&self, pool: &str) -> Arc<parking_lot::Mutex<SlabBacklog>> {
        if let Some(existing) = self.inner.read().get(pool).cloned() {
            return existing;
        }
        let mut g = self.inner.write();
        g.entry(pool.to_owned())
            .or_insert_with(|| Arc::new(parking_lot::Mutex::new(SlabBacklog::new())))
            .clone()
    }

    /// Snapshot of every pool's current backlog age. Surfaced by the
    /// admin endpoint.
    pub fn snapshot(&self, now: std::time::Instant) -> Vec<(String, std::time::Duration, bool)> {
        let g = self.inner.read();
        g.iter()
            .map(|(name, bp)| {
                let b = bp.lock();
                (name.clone(), b.age(now), b.is_over_threshold(now))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slab_id_as_chunk_id_collision_free_with_content_addressed() {
        // Content-addressed chunk ids are sha256 outputs; ours places
        // the "SLAB" marker at bytes 16..20 — vanishingly unlikely to
        // collide with any sha256 prefix. Verify the marker survives
        // a round-trip.
        let sid = SlabId::new();
        let cid = slab_id_as_chunk_id(sid);
        assert_eq!(&cid.0[16..20], b"SLAB");
        // Slab id is recoverable from the chunk id bytes.
        let uuid = uuid::Uuid::from_slice(&cid.0[0..16]).unwrap();
        assert_eq!(SlabId(uuid), sid);
    }

    #[test]
    fn serialise_then_deserialise_round_trips() {
        use kiseki_chunk::slab::encode_slab;
        let mut chunks = Vec::new();
        for i in 0..4u8 {
            let mut id = [0u8; 32];
            id[31] = i;
            chunks.push((ChunkId(id), vec![i; 32]));
        }
        let (slab, _enc) = encode_slab(&chunks, 4, 2).unwrap();
        let bytes = serialise_slab(&slab);
        let back = deserialise_slab(slab.id, &bytes).unwrap();
        assert_eq!(back.id, slab.id);
        assert_eq!(back.extents, slab.extents);
        assert_eq!(back.data, slab.data);
    }

    #[test]
    fn backlog_registry_returns_same_handle_per_pool() {
        let reg = SlabBacklogRegistry::new();
        let a = reg.get_or_insert("hot");
        let b = reg.get_or_insert("hot");
        assert!(Arc::ptr_eq(&a, &b));
        let c = reg.get_or_insert("cold");
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
