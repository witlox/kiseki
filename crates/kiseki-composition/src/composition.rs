//! Composition types and operations.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId, ShardId};
use kiseki_log::intent::PerspectiveSeq;
use kiseki_log::traits::LogOps;
use lru::LruCache;

use crate::error::CompositionError;
use crate::multipart::MultipartUpload;
use crate::namespace::Namespace;

/// Default inline data threshold in bytes. Data below this size is
/// stored inline in the delta payload rather than as a separate chunk.
pub const INLINE_DATA_THRESHOLD: u64 = 4096;

// Composition delta payloads (Phase 16f / 17 item 1).
//
// Each operation uses its own fixed-size payload format. The discriminator
// is the delta header's `operation` field (already present, already
// decoded by the hydrator), so the payload layouts don't need a leading
// op byte. Decoders return `None` when the wire shape does not parse —
// the hydrator treats that as a permanent skip so a malformed delta
// doesn't wedge the loop.

/// Wire size of the fixed prefix of the **Create** payload (40 bytes).
///
/// Layout (little-endian where applicable):
///   `[0..16)`  `composition_id` UUID
///   `[16..32)` `namespace_id` UUID
///   `[32..40)` `bytes_written` (u64 LE)
///
/// The Create payload always continues past this prefix with a
/// length-prefixed name + lens + perspective-seq tail; see
/// [`encode_composition_create_payload`] for the full shape.
pub const COMPOSITION_CREATE_PAYLOAD_PREFIX_LEN: usize = 40;

/// Wire size of the **Update** payload (24 bytes).
///
/// Layout (little-endian where applicable):
///   `[0..16)` `composition_id` UUID
///   `[16..24)` `bytes_written` (u64 LE)
///
/// `namespace_id` isn't carried because Update doesn't move a
/// composition between namespaces (rename is its own op). New chunks
/// ride in the delta header's `chunk_refs` field, same as Create.
pub const COMPOSITION_UPDATE_PAYLOAD_LEN: usize = 24;

/// Wire size of the **Delete** payload (16 bytes).
///
/// Layout: `[0..16)` `composition_id` UUID. No other fields needed —
/// the follower's local store has the rest already.
pub const COMPOSITION_DELETE_PAYLOAD_LEN: usize = 16;

/// Wire size of the **`NamespaceCreate`** payload (49 bytes).
///
/// ADR-040 Phase 18 — closes the namespace-replication hop.
///
/// Layout:
///   `[0..16)`  `namespace_id` UUID
///   `[16..32)` `tenant_id` (`OrgId`) UUID
///   `[32..48)` `shard_id` UUID
///   `[48]`     flags byte:
///                 bit 0 = `read_only`
///                 bit 1 = `versioning_enabled`
///                 bits 2-7 reserved (must be 0 for v1)
///
/// `compliance_tags` is intentionally not in this payload — operators
/// who need namespace-scoped compliance tags should configure them
/// via the control plane after the namespace is registered. A future
/// v2 payload can extend the wire with a length-prefixed tag array
/// using the same length-based dispatch as Create's optional name.
pub const NAMESPACE_CREATE_PAYLOAD_LEN: usize = 49;

/// Encode a `NamespaceCreate` delta payload.
#[must_use]
pub fn encode_namespace_create_payload(ns: &crate::namespace::Namespace) -> Vec<u8> {
    let mut out = Vec::with_capacity(NAMESPACE_CREATE_PAYLOAD_LEN);
    out.extend_from_slice(ns.id.0.as_bytes());
    out.extend_from_slice(ns.tenant_id.0.as_bytes());
    out.extend_from_slice(ns.shard_id.0.as_bytes());
    let mut flags = 0u8;
    if ns.read_only {
        flags |= 0b0000_0001;
    }
    if ns.versioning_enabled {
        flags |= 0b0000_0010;
    }
    out.push(flags);
    // ADR-045 §D3 tier policy, appended after the fixed prefix:
    // [count:1]{ [name_len:1][name utf8][quota_bytes:8 LE] }*.
    // A 49-byte payload (no appended section) decodes to an empty
    // policy, so pre-tier records stay readable.
    #[allow(clippy::cast_possible_truncation)] // tier count/name len bounded by CLI parsing
    {
        out.push(ns.tier_policy.len() as u8);
        for t in &ns.tier_policy {
            out.push(t.tier.len() as u8);
            out.extend_from_slice(t.tier.as_bytes());
            out.extend_from_slice(&t.quota_bytes.to_le_bytes());
        }
    }
    // ADR-024 2026-05-31 amendment §"three-tier durability" — append
    // the per-namespace size-band pool selector after the tier policy.
    // Wire shape:
    //   [bands_flag:1]   bits: 0=inline, 1=replicated, 2=ec
    //   for each set bit, in band order:
    //     [name_len:1][name utf8]
    // A truncated payload (no bands section) decodes to empty
    // `size_band_pools`, so namespaces created before the amendment
    // still round-trip cleanly.
    #[allow(clippy::cast_possible_truncation)] // name len bounded by CLI parsing (admin input)
    {
        let mut bands_flag: u8 = 0;
        if ns.size_band_pools.inline.is_some() {
            bands_flag |= 0b0000_0001;
        }
        if ns.size_band_pools.replicated.is_some() {
            bands_flag |= 0b0000_0010;
        }
        if ns.size_band_pools.ec.is_some() {
            bands_flag |= 0b0000_0100;
        }
        out.push(bands_flag);
        for band in [
            ns.size_band_pools.inline.as_deref(),
            ns.size_band_pools.replicated.as_deref(),
            ns.size_band_pools.ec.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            out.push(band.len() as u8);
            out.extend_from_slice(band.as_bytes());
        }
    }
    out
}

/// ADR-048 — encode a `MigrateChunkLocations` delta payload.
///
/// Wire layout:
/// ```text
/// [comp_id          : 16]  composition UUID
/// [pool_name_len u8 : 1 ]
/// [pool_name utf8   : N ]
/// [slab_id          : 16]  shared across every migrated chunk
/// [entry_count u32 LE: 4 ]
///   per entry:
///     [chunk_idx u32 LE    : 4 ]
///     [offset_in_slab u64 LE: 8 ]
///     [length u32 LE       : 4 ]
/// ```
/// All migrated chunks in one payload share the same destination
/// `pool_name + slab_id` — the compactor flushes one slab at a time,
/// so this is the natural batch. A composition that has chunks
/// landing in different slabs gets one delta per slab.
#[must_use]
pub fn encode_migrate_chunk_locations_payload(
    comp_id: CompositionId,
    pool_name: &str,
    slab_id: kiseki_common::SlabId,
    entries: &[(u32, u64, u32)], // (chunk_idx, offset_in_slab, length)
) -> Vec<u8> {
    let name_bytes = pool_name.as_bytes();
    let cap = 16 + 1 + name_bytes.len() + 16 + 4 + entries.len() * 16;
    let mut out = Vec::with_capacity(cap);
    out.extend_from_slice(comp_id.0.as_bytes());
    #[allow(clippy::cast_possible_truncation)] // pool names bounded by admin CLI parsing
    out.push(name_bytes.len() as u8);
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(slab_id.0.as_bytes());
    #[allow(clippy::cast_possible_truncation)]
    // entry counts bounded by per-slab cap (≤ DEFAULT_SLAB_MAX_CHUNKS)
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (idx, off, len) in entries {
        out.extend_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out
}

/// ADR-048 — decoded `MigrateChunkLocations` payload returned by
/// [`decode_migrate_chunk_locations_payload`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedMigrateChunkLocations {
    /// Target composition.
    pub composition_id: CompositionId,
    /// Pool that owns the slab.
    pub pool_name: String,
    /// Slab the migrated chunks now live in.
    pub slab_id: kiseki_common::SlabId,
    /// `(chunk_idx, offset_in_slab, length)` per migrated entry. The
    /// hydrator uses these to overwrite the corresponding entries in
    /// `Composition.chunk_locations`.
    pub entries: Vec<(u32, u64, u32)>,
}

/// Decode a `MigrateChunkLocations` payload. Returns `None` on a
/// truncated or malformed payload — the apply path treats the delta
/// as a no-op rather than crashing the state machine (defense in
/// depth; the encoder shouldn't ever emit a malformed payload, but
/// the hydrator is one of the few places where a corrupt delta
/// reaches user-visible state).
#[must_use]
pub fn decode_migrate_chunk_locations_payload(
    payload: &[u8],
) -> Option<DecodedMigrateChunkLocations> {
    if payload.len() < 16 + 1 {
        return None;
    }
    let comp_uuid = uuid::Uuid::from_slice(&payload[0..16]).ok()?;
    let name_len = payload[16] as usize;
    let mut pos = 17;
    let name_end = pos + name_len;
    if name_end + 16 + 4 > payload.len() {
        return None;
    }
    let pool_name = std::str::from_utf8(&payload[pos..name_end])
        .ok()?
        .to_owned();
    pos = name_end;
    let slab_uuid = uuid::Uuid::from_slice(&payload[pos..pos + 16]).ok()?;
    pos += 16;
    let count = u32::from_le_bytes(payload[pos..pos + 4].try_into().ok()?) as usize;
    pos += 4;
    if pos + count * 16 > payload.len() {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let idx = u32::from_le_bytes(payload[pos..pos + 4].try_into().ok()?);
        pos += 4;
        let off = u64::from_le_bytes(payload[pos..pos + 8].try_into().ok()?);
        pos += 8;
        let len = u32::from_le_bytes(payload[pos..pos + 4].try_into().ok()?);
        pos += 4;
        entries.push((idx, off, len));
    }
    Some(DecodedMigrateChunkLocations {
        composition_id: CompositionId(comp_uuid),
        pool_name,
        slab_id: kiseki_common::SlabId(slab_uuid),
        entries,
    })
}

/// Decode a `NamespaceCreate` delta payload. Returns `None` if the
/// length doesn't match [`NAMESPACE_CREATE_PAYLOAD_LEN`] or any UUID
/// fails to parse. `compliance_tags` is always empty in v1.
#[must_use]
pub fn decode_namespace_create_payload(payload: &[u8]) -> Option<crate::namespace::Namespace> {
    // The fixed prefix is exactly NAMESPACE_CREATE_PAYLOAD_LEN; anything
    // beyond it is the ADR-045 tier-policy section (optional).
    if payload.len() < NAMESPACE_CREATE_PAYLOAD_LEN {
        return None;
    }
    let ns_uuid = uuid::Uuid::from_slice(&payload[0..16]).ok()?;
    let tenant_uuid = uuid::Uuid::from_slice(&payload[16..32]).ok()?;
    let shard_uuid = uuid::Uuid::from_slice(&payload[32..48]).ok()?;
    let flags = payload[48];

    // Parse the appended tier policy, if present. Malformed/truncated
    // trailing bytes degrade to an empty policy rather than failing the
    // whole decode (a namespace without a policy is valid).
    let mut tier_policy = Vec::new();
    let mut pos = NAMESPACE_CREATE_PAYLOAD_LEN;
    if let Some(&count) = payload.get(pos) {
        pos += 1;
        for _ in 0..count {
            let Some(&name_len) = payload.get(pos) else {
                break;
            };
            pos += 1;
            let name_end = pos + name_len as usize;
            let quota_end = name_end + 8;
            if quota_end > payload.len() {
                break;
            }
            let Ok(name) = std::str::from_utf8(&payload[pos..name_end]) else {
                break;
            };
            let mut q = [0u8; 8];
            q.copy_from_slice(&payload[name_end..quota_end]);
            tier_policy.push(crate::namespace::TierQuota {
                tier: name.to_owned(),
                quota_bytes: u64::from_le_bytes(q),
            });
            pos = quota_end;
        }
    }

    // Optional per-namespace size-band pool selector (ADR-024 amendment
    // 2026-05-31). Decoded best-effort — a truncated/malformed bands
    // section degrades to the default (empty) `NamespaceSizeBandPools`,
    // never failing the whole decode.
    let mut size_band_pools = crate::namespace::NamespaceSizeBandPools::default();
    if let Some(&bands_flag) = payload.get(pos) {
        pos += 1;
        for (bit, slot) in [
            (0b0000_0001u8, 0usize),
            (0b0000_0010u8, 1usize),
            (0b0000_0100u8, 2usize),
        ] {
            if bands_flag & bit == 0 {
                continue;
            }
            let Some(&name_len) = payload.get(pos) else {
                break;
            };
            pos += 1;
            let name_end = pos + name_len as usize;
            if name_end > payload.len() {
                break;
            }
            let Ok(name) = std::str::from_utf8(&payload[pos..name_end]) else {
                break;
            };
            let value = Some(name.to_owned());
            match slot {
                0 => size_band_pools.inline = value,
                1 => size_band_pools.replicated = value,
                _ => size_band_pools.ec = value,
            }
            pos = name_end;
        }
    }

    Some(crate::namespace::Namespace {
        id: NamespaceId(ns_uuid),
        tenant_id: kiseki_common::ids::OrgId(tenant_uuid),
        shard_id: kiseki_common::ids::ShardId(shard_uuid),
        read_only: flags & 0b0000_0001 != 0,
        versioning_enabled: flags & 0b0000_0010 != 0,
        compliance_tags: Vec::new(),
        tier_policy,
        size_band_pools,
    })
}

/// Decoded composition-create delta payload — see
/// [`decode_composition_create_payload`].
///
/// Tuple: `(comp_id, namespace_id, size, name, chunk_plaintext_lens,
/// perspective_seq)`. `name` is `Some(_)` when the Create carried a
/// per-key binding (S3 / FUSE / NFS named PUT) and `None` for nameless
/// internal creates. `chunk_plaintext_lens` is `Some(_)` only for
/// multipart uploads (parts have arbitrary plaintext sizes); regular
/// PUTs leave it `None` and the read path uses the
/// `MAX_PLAINTEXT_PER_CHUNK` grid math. `perspective_seq` is `Some(_)`
/// when the async (decoupled-ack) producer minted an ingress HLC for
/// the per-name LWW guard, and `None` for sync surfaces.
pub type DecodedCompositionCreate = (
    CompositionId,
    NamespaceId,
    u64,
    Option<String>,
    Option<Vec<u32>>,
    Option<kiseki_log::intent::PerspectiveSeq>,
);

/// Encode a composition-create delta payload.
///
/// The wire shape is one fixed layout; there is no version dispatch.
/// Every field after the fixed 40-byte prefix is present, with
/// presence-bits / length-prefixes carrying the optional ones:
///
/// ```text
/// [comp_id              : 16]
/// [namespace_id         : 16]
/// [bytes_written u64 LE : 8 ]   <- fixed 40-byte prefix
/// [name_present u8      : 1 ]
///   if 1: [name_len u32 LE : 4][name utf8 : name_len]
/// [lens_count u32 LE    : 4 ]   <- 0 when no per-chunk lens carried
///   for each: [chunk_plaintext_len u32 LE : 4]
/// [seq_present u8       : 1 ]
///   if 1: [physical_ms u64 LE : 8][logical u32 LE : 4][node_id u64 LE : 8]
/// ```
///
/// `chunk_plaintext_lens` is currently set only by
/// `complete_multipart`: regular PUTs follow the
/// `MAX_PLAINTEXT_PER_CHUNK` grid and the read path uses index math,
/// so they pass `&[]` (encoded as `lens_count = 0`).
///
/// **Cross-surface seq contract (ADR-047 MF-9):** the synchronous
/// (POSIX / NFS / FUSE) gateway path passes `perspective_seq = None`
/// — the name-bind for that write is unconditional, Raft-commit-order
/// authoritative. The asynchronous (S3 / native decoupled-ack)
/// producer mints a `PerspectiveSeq` per write and passes
/// `Some(seq)`; the hydrator-side LWW guard then resolves concurrent
/// async same-name binds by `Some(seq) > Some(stored_seq)`. `None`
/// (sync) is treated as `-∞` for comparison so a sync-then-async
/// sequence lets the async write win (it has a real HLC timestamp);
/// an async-then-sync sequence lets the sync write win
/// (unconditional). See per-site doc comments on the four bind /
/// unbind sites for the full rule.
#[must_use]
pub fn encode_composition_create_payload(
    comp_id: CompositionId,
    namespace_id: NamespaceId,
    bytes_written: u64,
    name: Option<&str>,
    chunk_plaintext_lens: &[u32],
    perspective_seq: Option<kiseki_log::intent::PerspectiveSeq>,
) -> Vec<u8> {
    let name_bytes = name.map(str::as_bytes);
    let name_section_len = match name_bytes {
        Some(b) => 1 + 4 + b.len(),
        None => 1,
    };
    let lens_section_len = 4 + 4 * chunk_plaintext_lens.len();
    let seq_section_len = 1 + if perspective_seq.is_some() { 20 } else { 0 };
    let cap = COMPOSITION_CREATE_PAYLOAD_PREFIX_LEN
        + name_section_len
        + lens_section_len
        + seq_section_len;

    let mut out = Vec::with_capacity(cap);
    // Fixed prefix.
    out.extend_from_slice(comp_id.0.as_bytes());
    out.extend_from_slice(namespace_id.0.as_bytes());
    out.extend_from_slice(&bytes_written.to_le_bytes());
    // Name section.
    match name_bytes {
        Some(b) => {
            out.push(1);
            let name_len = u32::try_from(b.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(b);
        }
        None => out.push(0),
    }
    // Lens section.
    let lens_count = u32::try_from(chunk_plaintext_lens.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&lens_count.to_le_bytes());
    for &len in chunk_plaintext_lens {
        out.extend_from_slice(&len.to_le_bytes());
    }
    // Perspective-seq section.
    match perspective_seq {
        Some(seq) => {
            out.push(1);
            out.extend_from_slice(&seq.0.physical_ms.to_le_bytes());
            out.extend_from_slice(&seq.0.logical.to_le_bytes());
            out.extend_from_slice(&seq.0.node_id.0.to_le_bytes());
        }
        None => out.push(0),
    }
    out
}

/// Decode a composition-create delta payload.
///
/// Returns `None` on any structural mismatch — the hydrator treats
/// that as a permanent skip. The wire shape is the single layout
/// produced by [`encode_composition_create_payload`]; any other shape
/// is an error, not a recognised prior version.
#[must_use]
pub fn decode_composition_create_payload(payload: &[u8]) -> Option<DecodedCompositionCreate> {
    if payload.len() < COMPOSITION_CREATE_PAYLOAD_PREFIX_LEN {
        return None;
    }
    let comp_uuid = uuid::Uuid::from_slice(&payload[0..16]).ok()?;
    let ns_uuid = uuid::Uuid::from_slice(&payload[16..32]).ok()?;
    let mut size_bytes = [0u8; 8];
    size_bytes.copy_from_slice(&payload[32..40]);
    let size = u64::from_le_bytes(size_bytes);

    let mut pos = COMPOSITION_CREATE_PAYLOAD_PREFIX_LEN;

    // Name section.
    let name = {
        let present = *payload.get(pos)?;
        pos += 1;
        match present {
            0 => None,
            1 => {
                if payload.len() < pos + 4 {
                    return None;
                }
                let mut len_bytes = [0u8; 4];
                len_bytes.copy_from_slice(&payload[pos..pos + 4]);
                pos += 4;
                let name_len = u32::from_le_bytes(len_bytes) as usize;
                if payload.len() < pos + name_len {
                    return None;
                }
                let name = std::str::from_utf8(&payload[pos..pos + name_len])
                    .ok()?
                    .to_owned();
                pos += name_len;
                Some(name)
            }
            _ => return None,
        }
    };

    // Lens section.
    if payload.len() < pos + 4 {
        return None;
    }
    let mut count_bytes = [0u8; 4];
    count_bytes.copy_from_slice(&payload[pos..pos + 4]);
    pos += 4;
    let lens_count = u32::from_le_bytes(count_bytes) as usize;
    let lens_bytes = lens_count.checked_mul(4)?;
    if payload.len() < pos + lens_bytes {
        return None;
    }
    let mut lens = Vec::with_capacity(lens_count);
    for _ in 0..lens_count {
        let mut b = [0u8; 4];
        b.copy_from_slice(&payload[pos..pos + 4]);
        lens.push(u32::from_le_bytes(b));
        pos += 4;
    }
    let lens = if lens_count == 0 { None } else { Some(lens) };

    // Perspective-seq section.
    let perspective_seq = {
        let present = *payload.get(pos)?;
        pos += 1;
        match present {
            0 => None,
            1 => {
                if payload.len() < pos + 20 {
                    return None;
                }
                let mut phys = [0u8; 8];
                phys.copy_from_slice(&payload[pos..pos + 8]);
                let mut logical = [0u8; 4];
                logical.copy_from_slice(&payload[pos + 8..pos + 12]);
                let mut node = [0u8; 8];
                node.copy_from_slice(&payload[pos + 12..pos + 20]);
                pos += 20;
                Some(kiseki_log::intent::PerspectiveSeq(
                    kiseki_common::time::HybridLogicalClock {
                        physical_ms: u64::from_le_bytes(phys),
                        logical: u32::from_le_bytes(logical),
                        node_id: kiseki_common::ids::NodeId(u64::from_le_bytes(node)),
                    },
                ))
            }
            _ => return None,
        }
    };

    // Trailing bytes after the seq section are a structural error.
    if pos != payload.len() {
        return None;
    }

    Some((
        CompositionId(comp_uuid),
        NamespaceId(ns_uuid),
        size,
        name,
        lens,
        perspective_seq,
    ))
}

/// Encode a composition-update delta payload.
#[must_use]
pub fn encode_composition_update_payload(comp_id: CompositionId, bytes_written: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(COMPOSITION_UPDATE_PAYLOAD_LEN);
    out.extend_from_slice(comp_id.0.as_bytes());
    out.extend_from_slice(&bytes_written.to_le_bytes());
    out
}

/// Decode a composition-update delta payload.
///
/// Returns `None` if the length doesn't match
/// [`COMPOSITION_UPDATE_PAYLOAD_LEN`].
#[must_use]
pub fn decode_composition_update_payload(payload: &[u8]) -> Option<(CompositionId, u64)> {
    if payload.len() != COMPOSITION_UPDATE_PAYLOAD_LEN {
        return None;
    }
    let comp_uuid = uuid::Uuid::from_slice(&payload[0..16]).ok()?;
    let mut size_bytes = [0u8; 8];
    size_bytes.copy_from_slice(&payload[16..24]);
    let size = u64::from_le_bytes(size_bytes);
    Some((CompositionId(comp_uuid), size))
}

/// Encode a composition-delete delta payload.
#[must_use]
pub fn encode_composition_delete_payload(comp_id: CompositionId) -> Vec<u8> {
    comp_id.0.as_bytes().to_vec()
}

/// Decode a composition-delete delta payload.
///
/// Returns `None` if the length doesn't match
/// [`COMPOSITION_DELETE_PAYLOAD_LEN`].
#[must_use]
pub fn decode_composition_delete_payload(payload: &[u8]) -> Option<CompositionId> {
    if payload.len() != COMPOSITION_DELETE_PAYLOAD_LEN {
        return None;
    }
    let comp_uuid = uuid::Uuid::from_slice(&payload[0..16]).ok()?;
    Some(CompositionId(comp_uuid))
}

/// A composition — metadata describing how to assemble chunks into a
/// coherent data unit (file or object).
///
/// Serde derives: ADR-040 stores compositions through the
/// [`crate::persistent::CompositionStorage`] trait (currently
/// fjall-backed; ADR-022 successor) using postcard encoding. All
/// fields are concrete (no `HashMap` / `HashSet`) so postcard's
/// encoding is deterministic across runs.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Composition {
    /// Composition identifier.
    pub id: CompositionId,
    /// Owning tenant (I-X1).
    pub tenant_id: OrgId,
    /// Parent namespace.
    pub namespace_id: NamespaceId,
    /// Shard this composition's deltas live in.
    pub shard_id: ShardId,
    /// Ordered list of chunk references.
    pub chunks: Vec<ChunkId>,
    /// Current version number.
    pub version: u64,
    /// Total size in bytes.
    pub size: u64,
    /// Whether the composition data is inline in the delta (no chunks).
    pub has_inline_data: bool,
    /// Optional Content-Type carried through PUT → GET (RFC 6838).
    /// Stored on the composition so it survives across gateway
    /// instances (per ADV-PA-4: a per-`S3State` `HashMap` loses the
    /// header on multi-gateway deployments).
    pub content_type: Option<String>,
    /// Per-chunk plaintext length, when the chunks do **not** fit the
    /// regular `MAX_PLAINTEXT_PER_CHUNK` grid that the write path
    /// produces. Currently set only by `complete_multipart`: S3
    /// multipart parts are 1:1 with chunks but have arbitrary sizes
    /// (the regular PUT path always splits at `MAX_PLAINTEXT_PER_CHUNK`
    /// boundaries, so for those the read path can derive each chunk's
    /// position by index × `MAX_PLAINTEXT_PER_CHUNK`).
    ///
    /// `Vec::new()` is the "regular grid" sentinel — keep using the
    /// legacy index math. A non-empty vec must have the same length
    /// as `chunks` and gives the read path each chunk's plaintext
    /// length directly, so the offset of chunk `i` in the file is
    /// `chunk_plaintext_lens[0..i].sum()`.
    #[serde(default)]
    pub chunk_plaintext_lens: Vec<u32>,
    /// ADR-048 §"`ChunkRef` extends" per-chunk `Hot`/`Cold` tag. Empty (the
    /// serde default) means every chunk is implicit `Hot` in the
    /// pool the composition was created in — preserves backwards
    /// compatibility with pre-ADR-048 records. A non-empty vec must
    /// have the same length as `chunks`; entries flip to
    /// `ChunkRefLocation::Cold { slab_id, offset_in_slab, length }`
    /// when the slab-EC compactor migrates the chunk to a slab.
    #[serde(default)]
    pub chunk_locations: Vec<kiseki_common::ChunkRefLocation>,
}

impl Composition {
    /// ADR-048 §"Read path" — resolve the storage location of
    /// `chunks[i]`. When `chunk_locations` is empty (the
    /// pre-amendment case), returns an implicit Hot in the empty
    /// pool — callers fall through to the existing single-pool path.
    /// Out-of-range index returns the same implicit Hot.
    #[must_use]
    pub fn location_for(&self, chunk_idx: usize) -> kiseki_common::ChunkRefLocation {
        self.chunk_locations.get(chunk_idx).cloned().unwrap_or(
            kiseki_common::ChunkRefLocation::Hot {
                pool_name: String::new(),
            },
        )
    }
}

/// Result of a delete operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeleteResult {
    /// Composition was removed. Contains chunk IDs whose refcounts should
    /// be decremented.
    Removed(Vec<ChunkId>),
    /// A delete marker (tombstone) was appended because versioning is
    /// enabled. No chunk refcounts are changed.
    DeleteMarker,
}

/// Composition operations trait.
///
/// All methods are sync — they operate on in-memory state only.
/// Log emission (Raft consensus) is handled by the gateway after
/// releasing the composition lock, avoiding lock-across-await
/// serialization (ADR-032).
///
/// **B1 (2026-05-06):** all writes take `&self`. Concurrent callers
/// no longer need an outer `Arc<Mutex<CompositionStore>>` — the
/// `CompositionStore` holds its own short-lived locks per field
/// (storage, namespaces, multiparts), so two writers operating on
/// disjoint state run in parallel.
/// Conditional-check evaluator passed by the gateway into
/// [`CompositionStore::create_with_name_conditional`]. The store holds
/// the storage lock across the lookup + check + write, so the check
/// runs against the SAME forward-binding state the subsequent put
/// uses for its cascade decision — no race window between the
/// conditional evaluation and the binding update.
///
/// Kept as a trait (not a concrete enum) so the protocol layer
/// owns the conditional taxonomy: S3 carries `If-None-Match` /
/// `If-Match`, NFS4 carries `OPEN4_CREATE_GUARDED4`, etc. The
/// composition store doesn't need to know which protocol asked.
pub trait ConditionalCheck {
    /// Returns `Ok(())` when the write may proceed, `Err(reason)`
    /// when the precondition fails. `existing` is the
    /// `(ns, name) → comp_id` lookup result observed under the
    /// storage lock; `None` means no prior binding.
    ///
    /// # Errors
    /// Returns the human-readable reason that surfaces as
    /// `CompositionError::PreconditionFailed` and ultimately as
    /// HTTP 412 (S3) / `NFS4ERR_EXIST` (NFS).
    fn check(&self, existing: Option<CompositionId>) -> Result<(), String>;
}

/// Operations on the in-memory composition store.
///
/// Implementations include the canonical [`CompositionStore`] (lock-
/// per-shard hash maps) and the persistent overlay used by ADR-040
/// rev-3+ deployments. The trait surface is the contract the
/// gateway, hydrator, and Raft apply path all program against.
pub trait CompositionOps {
    /// Create a new composition in a namespace.
    fn create(
        &self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError>;

    /// Read a composition by ID. Returns an owned `Composition`
    /// (ADR-040 — the persistent backend can't lend across a backend
    /// transaction / batch).
    fn get(&self, id: CompositionId) -> Result<Composition, CompositionError>;

    /// Delete a composition. Returns `DeleteMarker` if versioning is
    /// enabled on the namespace.
    fn delete(&self, id: CompositionId) -> Result<DeleteResult, CompositionError>;

    /// Rename a composition. Returns `CrossShardRename` if source and
    /// target are on different shards (I-L8).
    fn rename(
        &self,
        id: CompositionId,
        target_namespace: NamespaceId,
    ) -> Result<(), CompositionError>;

    /// Update a composition — creates a new version with new chunk refs.
    fn update(
        &self,
        id: CompositionId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<u64, CompositionError>;

    /// Start a multipart upload.
    fn start_multipart(&self, namespace_id: NamespaceId) -> Result<String, CompositionError>;

    /// Upload a single part of a multipart upload. `was_new` is the
    /// `is_new` bool returned by the chunk store's `write_chunk` —
    /// `true` when the part's chunk was a fresh write, `false` on
    /// a dedup hit. The complete-multipart path uses this to build
    /// the `new_chunks` list for the Raft Create-delta so followers'
    /// `cluster_chunk_state` is seeded for cross-node fabric reads.
    fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        chunk_id: ChunkId,
        size: u64,
        was_new: bool,
    ) -> Result<(), CompositionError>;

    /// Abort a multipart upload — marks parts for GC.
    fn abort_multipart(&self, upload_id: &str) -> Result<(), CompositionError>;

    /// Finalize a multipart upload — makes the composition visible (I-L5).
    fn finalize_multipart(&self, upload_id: &str) -> Result<CompositionId, CompositionError>;
}

/// Composition store — wraps a `CompositionStorage` backend.
///
/// Phase 17 ADR-040 introduces the `CompositionStorage` seam so the
/// same struct can be backed by either an in-memory `HashMap` (tests,
/// single-node deployments) or a fjall-backed sibling that survives
/// restart (`FjallStorage`, ADR-022 successor). `namespaces` and
/// `multiparts` stay in-memory regardless (ADR-040 §D11). When a
/// `LogOps` is attached via `with_log`, mutations emit deltas to the
/// log shard.
///
/// Method semantics that changed in ADR-040:
///
/// - `get` and `list_by_namespace` now return owned `Composition`
///   values (the persistent backend can't lend references across a
///   backend transaction / batch). All call sites accept this since
///   `Composition` is `Clone` and the field accesses they perform
///   work uniformly on owned + borrowed values.
///
/// **B1 (2026-05-06):** all public methods take `&self`; per-field
/// locks (storage in `Mutex`, namespaces in `RwLock`, multiparts in
/// `Mutex`) so concurrent writers operating on disjoint state
/// proceed in parallel. The previous outer
/// `Arc<Mutex<CompositionStore>>` in `mem_gateway` / hydrator is
/// replaced with `Arc<CompositionStore>` — the lock convoy that
/// regressed PUT throughput at concurrency ≥ 8 is gone.
pub struct CompositionStore {
    /// Storage backend behind a Box for trait-object polymorphism.
    /// `CompositionStorage`'s methods all take `&self` (fjall's
    /// internal journal-mutex serialises commits; the in-memory
    /// backend uses interior mutability) so concurrent writers
    /// don't serialise on an outer Mutex. Cross-call atomicity for
    /// `name_lookup` + `put_with_name` is preserved via per-name
    /// shard locks (`name_locks`) rather than a single store-wide
    /// lock — concurrent PUTs to *different* names parallelise;
    /// concurrent PUTs to the *same* name still serialise to keep
    /// `If-None-Match: *` race-free.
    storage: Box<dyn crate::persistent::CompositionStorage>,
    /// Per-`(ns, name)` shard locks for the create-with-name +
    /// conditional-check critical section. 256 shards is enough for
    /// realistic key distributions; fewer would cause coincidental
    /// contention, more would waste memory with no practical
    /// benefit.
    name_locks: Box<[parking_lot::Mutex<()>; NAME_LOCK_SHARDS]>,
    /// Per-`composition_id` shard locks for read-modify-write
    /// methods (`update`, `delete`, `rename`, `set_content_type`,
    /// `update_at`, `delete_at`). Without these, two concurrent
    /// updates to the same composition could both read v=1, both
    /// write v=2, and one update would silently disappear.
    /// Different `composition_id`s hash to different shards →
    /// concurrent updates to *distinct* compositions parallelise.
    id_locks: Box<[parking_lot::Mutex<()>; ID_LOCK_SHARDS]>,
    namespaces: parking_lot::RwLock<HashMap<NamespaceId, Namespace>>,
    multiparts: parking_lot::Mutex<HashMap<String, (MultipartUpload, NamespaceId)>>,
    log: parking_lot::RwLock<Option<Arc<dyn LogOps + Send + Sync>>>,
    /// Composition read cache (post-V3 ADR-042 perf sweep).
    ///
    /// `CompositionStore::get` was 14.74% of CPU at 64 KiB GET in the
    /// post-V3 native flame because every lookup took the storage
    /// `Mutex` and round-tripped through fjall's LSM. The LRU shaves
    /// the storage hit on hot keys while keeping the storage backend
    /// the source of truth — mutations invalidate the entry under the
    /// same storage lock that publishes the new value, so a hit is
    /// always a value that was committed at the time of insertion.
    ///
    /// Lock-ordering invariant (the only thing that makes this
    /// correct): cache entries are inserted on a get-miss while the
    /// storage `Mutex` is held, and removed by mutators while the
    /// storage `Mutex` is held. Cache hits don't touch storage at
    /// all — they're a snapshot read of a value that was once
    /// published. Subsequent mutations clear the entry, so a read
    /// after a mutation either misses the cache (re-fetches from
    /// storage) or hits a fresh post-mutation entry.
    read_cache: parking_lot::Mutex<LruCache<CompositionId, Composition>>,
    /// ADR-048 — hot-tier eviction hook invoked after a successful
    /// hydration batch commit when `migrated_chunk_evictions` is
    /// non-empty. The gateway wires this to the chunk store's
    /// refcount-decrement path so chunks freshly migrated into a
    /// slab can release their hot-tier copy (I-SE4). `None` on a
    /// store that hasn't been wired (single-node test fixtures); the
    /// commit path no-ops in that case.
    eviction_sink: parking_lot::RwLock<Option<Arc<dyn ChunkEvictionSink + Send + Sync>>>,
    /// P4 (#226) — UNEVICTABLE volatile overlay: composition rows
    /// created at ack time on async-ack surfaces (S3 / Native) via
    /// `create_volatile` / `create_with_name_volatile`, pending
    /// durable materialization by the hydrator (which calls
    /// `drain_pending` after the Create delta's batch commits).
    ///
    /// HARD CONSTRAINT: this overlay must NOT be visible through the
    /// `CompositionStorage` trait (`with_storage_locked`). The
    /// hydrator's `Staging::view` reads raw storage DELIBERATELY —
    /// that bypass is what makes the ingress node's own hydrator
    /// stage the durable fjall put for a row that exists only here.
    /// If the overlay leaked into `CompositionStorage::get`, the
    /// idempotency check in `stage_create` would see the row, skip
    /// the put, and the row would NEVER become durable (and replay
    /// could not recover it). Pinned by the
    /// `volatile_overlay_invisible_to_storage_trait` test.
    ///
    /// Intentionally overlay-blind readers (besides the storage
    /// trait): `count()` — a storage gauge, bounded-stale is fine —
    /// and `list_namespace_compositions()` — feeds the slab-EC
    /// compactor, which must not pick up a row whose durable
    /// materialization is still in flight.
    pending: parking_lot::RwLock<HashMap<CompositionId, Composition>>,
    /// Name bindings for volatile rows: `(ns, name) → (comp_id,
    /// perspective_seq)`. Consulted FIRST by `lookup_by_name` /
    /// `name_for` / `list_names` — a pending bind is strictly newer
    /// than any durable bind on the same name. Drained by `comp_id`,
    /// never by name alone, so a newer pending bind on the same name
    /// survives an older comp's drain (LWW).
    pending_names: parking_lot::RwLock<PendingNames>,
    /// Cap on `pending` entries (`KISEKI_COMPOSITION_PENDING_MAX`,
    /// default [`DEFAULT_COMPOSITION_PENDING_MAX`]). At the cap,
    /// volatile creates fall back to the durable write-through path
    /// (never reject).
    pending_max: usize,
    /// Counter: volatile creates that overflowed to the durable
    /// write-through path. Exposed via `pending_overflows()`.
    pending_overflows: std::sync::atomic::AtomicU64,
}

/// Volatile overlay name bindings (P4 #226), indexed both ways.
///
/// `by_name`: `(ns, name) → (comp_id, perspective_seq)` — the lookup
/// direction (`lookup_by_name`, conditional checks, LIST merge). The
/// optional [`PerspectiveSeq`] is the ingress-minted seq the eventual
/// Create delta carries — recorded for observability and LWW
/// reasoning; drains key off the `comp_id` alone.
///
/// `by_id`: `comp_id → (ns, name)` — the removal direction. Exists so
/// per-id removal (`drain_pending` / `discard_volatile` / volatile
/// delete) and `name_for` are O(1) instead of a full-map scan under
/// the write lock. The scan was #256: every hydrator drain held the
/// lock for O(overlay) per drained id, quantizing every ack-path bind
/// behind drain activity (measured −71% PUT on GCP at the P4
/// boundary).
///
/// Invariant: `by_id[id] == key` ⇔ `by_name[key].0 == id`. A newer
/// bind overwriting `key` evicts the older owner's `by_id` entry in
/// the same critical section (`bind`), so a stale older `comp_id` can
/// never remove the newer bind (LWW preserved).
#[derive(Default)]
struct PendingNames {
    by_name: HashMap<(NamespaceId, String), (CompositionId, Option<PerspectiveSeq>)>,
    by_id: HashMap<CompositionId, (NamespaceId, String)>,
}

impl PendingNames {
    /// Bind `key → id` (newest wins). If the key was bound to an
    /// older comp, that comp's reverse entry is evicted here — after
    /// this, `remove_by_id(old)` is a no-op on `by_name`.
    fn bind(&mut self, key: (NamespaceId, String), id: CompositionId, seq: Option<PerspectiveSeq>) {
        debug_assert!(
            !self.by_id.contains_key(&id),
            "volatile comp bound twice — no production emitter re-binds a pending row"
        );
        if let Some((old_id, _)) = self.by_name.insert(key.clone(), (id, seq)) {
            if old_id != id {
                self.by_id.remove(&old_id);
            }
        }
        // Self-heal the (unreachable-today) re-bind-to-a-different-key
        // case the debug_assert above flags: if `id` previously owned
        // another key, drop that key's `by_name` entry while it still
        // points at `id` — otherwise it would shadow durable reads on
        // that name forever (adversary review of #256, finding 1).
        if let Some(prev_key) = self.by_id.insert(id, key) {
            // `key` moved into the map; read it back for the
            // same-key (re-bind) comparison — rare branch only.
            let rebound_same_key = self.by_id.get(&id) == Some(&prev_key);
            if !rebound_same_key && self.by_name.get(&prev_key).map(|v| v.0) == Some(id) {
                self.by_name.remove(&prev_key);
            }
        }
    }

    /// Remove whatever binding owns `key` (the durable write-through
    /// overflow path: the durable bind is strictly newer, the pending
    /// one is superseded).
    fn remove_by_name(&mut self, key: &(NamespaceId, String)) {
        if let Some((old_id, _)) = self.by_name.remove(key) {
            self.by_id.remove(&old_id);
        }
    }

    /// Remove `id`'s binding — O(1). LWW: only removes the `by_name`
    /// entry if `id` still owns it (a newer bind on the same name
    /// already evicted us from `by_id` in `bind`, making this a
    /// no-op, but the ownership re-check keeps the invariant locally
    /// obvious).
    fn remove_by_id(&mut self, id: CompositionId) {
        if let Some(key) = self.by_id.remove(&id) {
            if self.by_name.get(&key).map(|v| v.0) == Some(id) {
                self.by_name.remove(&key);
            }
        }
    }
}

/// ADR-048 hook the gateway implements to release a migrated chunk's
/// hot-tier copy after the cold-tier slab is durable + the
/// `MigrateChunkLocations` delta has applied.
///
/// `decrement_refcount` is called exactly once per `chunk_id` per
/// committed migration batch on the hydrator-commit thread. The sink
/// MUST be non-blocking — a blocking implementation back-pressures
/// the hydrator loop. The chunk-GC sweep does the actual reclaim
/// when refcount hits zero (I-SE4).
pub trait ChunkEvictionSink: Send + Sync {
    /// Decrement the local chunk store's refcount on every id in
    /// `chunk_ids`. Implementations should treat unknown ids as a
    /// silent no-op (deduped chunks that this node never persisted
    /// surface as unknown here — and that's fine; some other
    /// placement node persisted the bytes).
    fn release_migrated_chunks(&self, chunk_ids: &[kiseki_common::ids::ChunkId]);
}

/// Number of shards in the per-name lock array. 256 keeps the
/// shard-collision probability low for realistic key distributions
/// while costing only 256 × 8 bytes (`parking_lot::Mutex` word size).
pub const NAME_LOCK_SHARDS: usize = 256;

/// Number of shards in the per-id lock array. Same sizing rationale
/// as `NAME_LOCK_SHARDS` — 256 buckets, ~few KiB total.
pub const ID_LOCK_SHARDS: usize = 256;

fn name_shard(ns: NamespaceId, name: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ns.0.hash(&mut h);
    name.hash(&mut h);
    // Modulo in u64 first so the cast lands on a value already
    // bounded by NAME_LOCK_SHARDS — fits in usize on every target.
    let shard = h.finish() % NAME_LOCK_SHARDS as u64;
    usize::try_from(shard).expect("shard < NAME_LOCK_SHARDS fits in usize")
}

fn id_shard(id: CompositionId) -> usize {
    // UUID bytes are already random — fold the high bytes into a
    // shard index without invoking SipHash for a 1-byte modulo.
    let bytes = id.0.as_bytes();
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    (n as usize) % ID_LOCK_SHARDS
}

/// Default composition read-cache capacity (entries). Each entry is
/// ~200 bytes (`Composition` has a small fixed shape + a `Vec<ChunkId>`
/// of 32-byte refs), so 65 536 entries ≈ 12 MiB at typical chunk
/// counts. Override via `KISEKI_COMPOSITION_CACHE_ENTRIES`. Setting
/// to 0 disables the cache entirely (still allocates a 1-entry LRU
/// to keep the type uniform).
pub const DEFAULT_COMPOSITION_CACHE_CAPACITY: usize = 65_536;

fn read_cache_capacity() -> NonZeroUsize {
    let cap = std::env::var("KISEKI_COMPOSITION_CACHE_ENTRIES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_COMPOSITION_CACHE_CAPACITY);
    NonZeroUsize::new(cap.max(1)).expect("capacity at least 1")
}

/// Default cap on the volatile pending overlay (entries). At ~200
/// bytes per `Composition` this bounds the overlay at ≈ 12 MiB —
/// the same sizing rationale as the read cache. The cap exists so an
/// ingress node whose hydrator falls behind can't grow the overlay
/// unboundedly; at the cap, volatile creates fall back to the
/// durable write-through path (they never reject). Override via
/// `KISEKI_COMPOSITION_PENDING_MAX`.
pub const DEFAULT_COMPOSITION_PENDING_MAX: usize = 65_536;

fn pending_max_capacity() -> usize {
    std::env::var("KISEKI_COMPOSITION_PENDING_MAX")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_COMPOSITION_PENDING_MAX)
}

impl CompositionStore {
    /// Create an empty composition store with the in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::with_storage(Box::new(crate::persistent::MemoryStorage::new()))
    }

    /// Create a composition store with an explicit storage backend
    /// (e.g. `FjallStorage` for multi-node deployments).
    #[must_use]
    pub fn with_storage(storage: Box<dyn crate::persistent::CompositionStorage>) -> Self {
        // Box the arrays to keep CompositionStore's stack footprint
        // small. parking_lot::Mutex<()> is one word per shard.
        let name_locks: Box<[parking_lot::Mutex<()>; NAME_LOCK_SHARDS]> =
            Box::new(std::array::from_fn(|_| parking_lot::Mutex::new(())));
        let id_locks: Box<[parking_lot::Mutex<()>; ID_LOCK_SHARDS]> =
            Box::new(std::array::from_fn(|_| parking_lot::Mutex::new(())));
        Self {
            storage,
            name_locks,
            id_locks,
            namespaces: parking_lot::RwLock::new(HashMap::new()),
            multiparts: parking_lot::Mutex::new(HashMap::new()),
            log: parking_lot::RwLock::new(None),
            read_cache: parking_lot::Mutex::new(LruCache::new(read_cache_capacity())),
            eviction_sink: parking_lot::RwLock::new(None),
            pending: parking_lot::RwLock::new(HashMap::new()),
            pending_names: parking_lot::RwLock::new(PendingNames::default()),
            pending_max: pending_max_capacity(),
            pending_overflows: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Override the volatile-overlay cap. Test/bench hook — exercising
    /// the overflow fallback shouldn't require 65 536 inserts.
    #[must_use]
    pub fn with_pending_max(mut self, max: usize) -> Self {
        self.pending_max = max;
        self
    }

    /// ADR-048 — register the gateway-side chunk eviction sink. Called
    /// once at startup. Subsequent registrations replace the previous
    /// sink (useful for tests that swap a stub sink in mid-run).
    pub fn set_eviction_sink(&self, sink: Arc<dyn ChunkEvictionSink + Send + Sync>) {
        *self.eviction_sink.write() = Some(sink);
    }

    /// ADR-048 — list compositions in a namespace. Surfaced from the
    /// storage trait so the slab-EC compactor can scan candidates
    /// without taking a dep on the persistent layer.
    pub fn list_namespace_compositions(&self, ns: NamespaceId) -> Vec<Composition> {
        self.storage.list_in_namespace(ns).unwrap_or_default()
    }

    /// ADR-048 — invoke the eviction sink with the given chunk IDs.
    /// No-op when no sink is wired. Called by the hydrator's commit
    /// path after a successful `apply_hydration_batch`.
    pub fn invoke_eviction_sink(&self, chunk_ids: &[kiseki_common::ids::ChunkId]) {
        if chunk_ids.is_empty() {
            return;
        }
        if let Some(sink) = self.eviction_sink.read().clone() {
            sink.release_migrated_chunks(chunk_ids);
        }
    }

    /// Drop every cached `(CompositionId, Composition)` entry.
    ///
    /// Used by the hydrator after `apply_hydration_batch` — that path
    /// mutates the storage backend through `with_storage_locked` and
    /// the LRU here would otherwise serve pre-batch values for
    /// composition ids the hydrator just rewrote. Hot path on
    /// followers; no-op when the cache is empty.
    pub fn clear_cache(&self) {
        self.read_cache.lock().clear();
    }

    /// Drop a single composition's cache entry. Used by the hydrator
    /// when it knows the exact composition ids touched by an applied
    /// batch (cheaper than `clear_cache` when the hot working set is
    /// large but the batch is small).
    pub fn invalidate_cache(&self, id: CompositionId) {
        self.read_cache.lock().pop(&id);
    }

    /// Run a closure with direct access to the storage backend.
    /// Trait methods all take `&self` (post-Mutex-removal refactor)
    /// so the closure has lock-free access; concurrent writers in
    /// other `CompositionStore` methods are serialized only by their
    /// per-name / per-id shard locks where atomicity is required.
    /// The hydrator uses this for `apply_hydration_batch` (still
    /// the only caller that needs typed access to the storage
    /// handle).
    pub fn with_storage_locked<R>(
        &self,
        f: impl FnOnce(&dyn crate::persistent::CompositionStorage) -> R,
    ) -> R {
        f(&*self.storage)
    }

    /// Attach a log store for delta emission. `&self` so callers can
    /// install the log on a `CompositionStore` shared via `Arc`
    /// (the post-B1 gateway shape).
    pub fn install_log(&self, log: Arc<dyn LogOps + Send + Sync>) {
        *self.log.write() = Some(log);
    }

    /// Builder-style log attachment for callers that still own the
    /// store by value (tests, single-threaded setup).
    #[must_use]
    pub fn with_log(self, log: Arc<dyn LogOps + Send + Sync>) -> Self {
        *self.log.write() = Some(log);
        self
    }

    /// Get a clone of the attached log store (if any). Returns owned
    /// because callers route the log through `Arc::clone` anyway and
    /// holding a reference would force the read-lock guard to live
    /// across `.await`.
    #[must_use]
    pub fn log(&self) -> Option<Arc<dyn LogOps + Send + Sync>> {
        self.log.read().clone()
    }

    /// Register a namespace.
    pub fn add_namespace(&self, ns: Namespace) {
        self.namespaces.write().insert(ns.id, ns);
    }

    /// Remove a namespace registration. Used by `ensure_namespace_exists`
    /// to roll back after a Raft replication failure (ADR-040 Phase 18).
    /// Returns the removed namespace if it was registered.
    pub fn remove_namespace(&self, id: NamespaceId) -> Option<Namespace> {
        self.namespaces.write().remove(&id)
    }

    /// Clear all namespace registrations (gateway crash simulation).
    pub fn clear_namespaces(&self) {
        self.namespaces.write().clear();
    }

    /// Get a namespace, returning an owned clone (so the read-lock
    /// guard doesn't leak to callers).
    #[must_use]
    pub fn namespace(&self, id: NamespaceId) -> Option<Namespace> {
        self.namespaces.read().get(&id).cloned()
    }

    /// Snapshot every registered namespace. Used by
    /// `InMemoryGateway::new` to prime its lock-free
    /// `namespace_meta` cache from a pre-populated store, so
    /// constructing a gateway from a store that already has
    /// namespaces (test setup, restart, hydration) doesn't need a
    /// follow-up `gateway.add_namespace(...)` call to enforce
    /// `read_only` etc.
    #[must_use]
    pub fn list_namespaces(&self) -> Vec<Namespace> {
        self.namespaces.read().values().cloned().collect()
    }

    /// Total composition count.
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::Storage` if the storage backend
    /// fails (rare; only persistent backends fail this).
    pub fn count(&self) -> Result<u64, CompositionError> {
        Ok(self.storage.count()?)
    }

    /// List all compositions in a namespace. Returns owned
    /// `Composition` values (ADR-040 — the persistent backend can't
    /// lend across a backend transaction / batch).
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::Storage` on storage failure.
    pub fn list_by_namespace(
        &self,
        ns_id: NamespaceId,
    ) -> Result<Vec<Composition>, CompositionError> {
        let mut out = self.storage.list_in_namespace(ns_id)?;
        // P4 (#226): include volatile rows so a fresh async write
        // shows up in by-id listings within the hydration lag window.
        // On an id collision (post-apply, pre-drain microwindow) the
        // pending value wins — RMW methods mutate pending first, so
        // it is at least as new.
        let pend = self.pending.read();
        if !pend.is_empty() {
            let pending_ids: std::collections::HashSet<CompositionId> = pend
                .values()
                .filter(|c| c.namespace_id == ns_id)
                .map(|c| c.id)
                .collect();
            if !pending_ids.is_empty() {
                out.retain(|c| !pending_ids.contains(&c.id));
                out.extend(pend.values().filter(|c| c.namespace_id == ns_id).cloned());
            }
        }
        Ok(out)
    }

    /// Attach a Content-Type to an existing composition (RFC 6838
    /// round-trip). Returns `Err(CompositionNotFound)` if the
    /// composition doesn't exist. Idempotent: overwrites any prior
    /// value.
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::CompositionNotFound` if `id` is
    /// not in the store; `CompositionError::Storage` on backend
    /// failure.
    pub fn set_content_type(
        &self,
        id: CompositionId,
        content_type: Option<String>,
    ) -> Result<(), CompositionError> {
        let _g = self.id_locks[id_shard(id)].lock();
        // P4 (#226): metadata ops on a fresh (volatile) row mutate the
        // overlay in place — the durable backend doesn't have the row
        // yet and a storage-only read would 404 within the hydration
        // lag window.
        if let Some(comp) = self.pending.write().get_mut(&id) {
            comp.content_type = content_type;
            return Ok(());
        }
        let mut comp = self
            .storage
            .get(id)?
            .ok_or(CompositionError::CompositionNotFound(id))?;
        comp.content_type = content_type;
        self.storage.put(comp)?;
        self.read_cache.lock().pop(&id);
        Ok(())
    }

    /// Create a composition where the chunks do not fit the regular
    /// `MAX_PLAINTEXT_PER_CHUNK` write-grid (currently used only by
    /// S3 multipart uploads, where parts are 1:1 with chunks but have
    /// arbitrary plaintext sizes). The supplied `chunk_plaintext_lens`
    /// must have the same length as `chunks`.
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::NamespaceNotFound` /
    /// `CompositionError::ReadOnlyNamespace` for the usual reasons,
    /// plus `CompositionError::InvalidArgument` when the lens vec
    /// length doesn't match the chunks vec length.
    pub fn create_with_lens(
        &self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        chunk_plaintext_lens: Vec<u32>,
        size: u64,
    ) -> Result<CompositionId, CompositionError> {
        if chunk_plaintext_lens.len() != chunks.len() {
            return Err(CompositionError::InvalidArgument(format!(
                "chunk_plaintext_lens length {} != chunks length {}",
                chunk_plaintext_lens.len(),
                chunks.len(),
            )));
        }
        let ns_snap = {
            let nss = self.namespaces.read();
            let ns = nss
                .get(&namespace_id)
                .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;
            if ns.read_only {
                return Err(CompositionError::ReadOnlyNamespace(namespace_id));
            }
            (ns.tenant_id, ns.shard_id)
        };

        let id = CompositionId(uuid::Uuid::new_v4());
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id,
            tenant_id: ns_snap.0,
            namespace_id,
            shard_id: ns_snap.1,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
            chunk_plaintext_lens,
            chunk_locations: Vec::new(),
        };
        self.storage.put(comp)?;
        Ok(id)
    }

    /// Install a composition with a leader-assigned id (Phase 16f
    /// follower hydration).
    ///
    /// Mirrors `create()` but uses the supplied `comp_id` instead of
    /// generating a fresh UUID, so a follower can rebuild its store
    /// from the Raft-replicated delta log. Idempotent — a second call
    /// with the same id is a no-op.
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::NamespaceNotFound` if the namespace
    /// hasn't been registered on this node yet (the bootstrap
    /// namespace is registered at server startup; tenant-specific
    /// namespaces would need their own replication path).
    pub fn create_at(
        &self,
        comp_id: CompositionId,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<(), CompositionError> {
        // Snapshot namespace metadata under the namespaces read lock
        // (does NOT hold across the storage write below — the
        // storage Mutex is independent).
        let ns_meta = self
            .namespaces
            .read()
            .get(&namespace_id)
            .map(|ns| (ns.tenant_id, ns.shard_id))
            .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;
        let _g = self.id_locks[id_shard(comp_id)].lock();
        if self.storage.get(comp_id)?.is_some() {
            return Ok(()); // already hydrated — idempotent
        }
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id: comp_id,
            tenant_id: ns_meta.0,
            namespace_id,
            shard_id: ns_meta.1,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
            chunk_plaintext_lens: Vec::new(),
            chunk_locations: Vec::new(),
        };
        self.storage.put(comp)?;
        Ok(())
    }

    // -- Name index facades (S3 per-key naming) --
    //
    // The S3 PUT/GET/DELETE/LIST path needs to address compositions
    // by user-supplied key, not just by composition_id UUID. These
    // facades route through `CompositionStorage`'s name index,
    // keeping the storage trait the single source of truth so
    // followers replay name changes via the hydration batch.

    /// Resolve `(namespace_id, name)` → `composition_id`.
    ///
    /// # Errors
    /// Returns `CompositionError::Storage` on backend failure.
    pub fn lookup_by_name(
        &self,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, CompositionError> {
        // P4 (#226): a pending (volatile) bind is strictly newer than
        // any durable bind on the same name — consult the overlay
        // first.
        if let Some(&(id, _)) = self
            .pending_names
            .read()
            .by_name
            .get(&(namespace_id, name.to_owned()))
        {
            return Ok(Some(id));
        }
        Ok(self.storage.name_lookup(namespace_id, name)?)
    }

    /// Reverse-lookup: `composition_id` → `(namespace_id, name)` if the
    /// composition was created with a name.
    ///
    /// # Errors
    /// Returns `CompositionError::Storage` on backend failure.
    pub fn name_for(
        &self,
        id: CompositionId,
    ) -> Result<Option<(NamespaceId, String)>, CompositionError> {
        // P4 (#226): reverse check on the volatile overlay first — a
        // fresh async write's binding only exists there until the
        // hydrator materializes it. O(1) via the `by_id` index
        // (#256: the previous full scan held the lock for O(overlay)
        // per call).
        if let Some((ns, n)) = self.pending_names.read().by_id.get(&id) {
            return Ok(Some((*ns, n.clone())));
        }
        Ok(self.storage.name_for(id)?)
    }

    /// Bind `name` to `id` in `ns`. Overwrites any existing binding;
    /// the caller is responsible for pre-flight conditional checks.
    ///
    /// # Errors
    /// Returns `CompositionError::Storage` on backend failure.
    pub fn bind_name(
        &self,
        namespace_id: NamespaceId,
        name: String,
        id: CompositionId,
    ) -> Result<(), CompositionError> {
        Ok(self.storage.name_insert(namespace_id, name, id)?)
    }

    /// Atomic create-then-name with an optional S3-style
    /// conditional check. Holds the storage lock once across:
    ///
    /// 1. Forward `(ns, name) → comp_id` lookup (one fjall.get).
    /// 2. Conditional-header evaluation when `cond.is_some()`
    ///    (`If-None-Match: *` rejects when an existing binding is
    ///    present; `If-Match: <etag>` rejects when absent or
    ///    mismatched).
    /// 3. Composition row construction.
    /// 4. `put_with_name(prior_id = step-1-result)` — feeds the
    ///    cascade decision to the backend so it skips its own
    ///    pre-flight read.
    ///
    /// One fjall.get total per call. When `cond.is_some()` this
    /// replaces the prior gateway pattern of `lookup_by_name`
    /// (releases lock) + `create_with_name` (re-acquires lock,
    /// does its own pre-flight) — half the journal-mutex
    /// acquisitions and the race window between the two is closed.
    ///
    /// Strictly stronger atomicity than `create()` +
    /// `bind_name()`: no observable state where the composition
    /// exists but the name is missing, or vice versa.
    ///
    /// # Errors
    /// - `CompositionError::NamespaceNotFound` if the namespace
    ///   isn't registered locally.
    /// - `CompositionError::ReadOnlyNamespace` on read-only
    ///   tenants.
    /// - `CompositionError::PreconditionFailed(reason)` when
    ///   `cond.is_some()` and the conditional check rejects.
    /// - `CompositionError::Storage` on backend failure.
    pub fn create_with_name(
        &self,
        namespace_id: NamespaceId,
        name: String,
        cond: Option<&dyn ConditionalCheck>,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError> {
        let ns_snap = {
            let nss = self.namespaces.read();
            let ns = nss
                .get(&namespace_id)
                .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;
            if ns.read_only {
                return Err(CompositionError::ReadOnlyNamespace(namespace_id));
            }
            (ns.tenant_id, ns.shard_id)
        };

        let id = CompositionId(uuid::Uuid::new_v4());
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id,
            tenant_id: ns_snap.0,
            namespace_id,
            shard_id: ns_snap.1,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
            chunk_plaintext_lens: Vec::new(),
            chunk_locations: Vec::new(),
        };

        // Per-name shard lock holds across the lookup + (optional)
        // conditional check + put. Concurrent PUTs to *different*
        // names hash to different shards and parallelise. PUTs to
        // the *same* name still serialise on the same shard so
        // `If-None-Match: *` stays race-free. The previous design
        // held a store-wide Mutex across the same critical section
        // → measured 1/25µs ≈ 40k op/s ceiling at c=16 (we were at
        // ~33k op/s). Per-name sharding lifts the ceiling for
        // unique-name workloads (NFS, native, FUSE — fresh UUIDs
        // per PUT).
        let _g = self.name_locks[name_shard(namespace_id, &name)].lock();
        let existing = self.storage.name_lookup(namespace_id, &name)?;
        if let Some(cond) = cond {
            cond.check(existing)
                .map_err(CompositionError::PreconditionFailed)?;
        }
        self.storage
            .put_with_name(comp, namespace_id, name, existing)?;
        Ok(id)
    }

    /// Unbind `name` in `ns`. Returns `true` if a binding existed.
    ///
    /// # Errors
    /// Returns `CompositionError::Storage` on backend failure.
    pub fn unbind_name(
        &self,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<bool, CompositionError> {
        Ok(self.storage.name_remove(namespace_id, name)?)
    }

    /// Enumerate `(name, composition_id)` bindings in a namespace,
    /// optionally filtered by `prefix`. Returns alphabetically by name
    /// (S3 LIST ordering).
    ///
    /// # Errors
    /// Returns `CompositionError::Storage` on backend failure.
    pub fn list_names(
        &self,
        namespace_id: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId)>, CompositionError> {
        let mut out = self.storage.name_list(namespace_id, prefix)?;
        // P4 (#226): merge the volatile overlay — a pending bind is
        // strictly newer than any durable bind on the same name, so
        // it wins on duplicates (LIST coherence within the hydration
        // lag window).
        let overlay: Vec<(String, CompositionId)> = {
            let pend = self.pending_names.read();
            let mut v = Vec::new();
            for ((ns, n), (cid, _)) in &pend.by_name {
                if *ns == namespace_id && prefix.is_none_or(|p| n.starts_with(p)) {
                    v.push((n.clone(), *cid));
                }
            }
            v
        };
        if !overlay.is_empty() {
            {
                let shadowed: std::collections::HashSet<&str> =
                    overlay.iter().map(|(n, _)| n.as_str()).collect();
                out.retain(|(n, _)| !shadowed.contains(n.as_str()));
            }
            out.extend(overlay);
            out.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Ok(out)
    }

    // -- P4 (#226): volatile create overlay --------------------------------
    //
    // Async-ack surfaces (S3, Native) create the composition row + name
    // binding here at ack time; the durable fjall materialization rides
    // the hydrator — when this node's hydrator applies the Create delta
    // it stages the put against raw storage (the overlay is invisible
    // there, see the `pending` field doc) and then drains the overlay
    // row via `drain_pending`.

    /// Create a composition in the volatile pending overlay (no
    /// storage write). Mirrors [`CompositionOps::create`] except the
    /// row lands in `pending` instead of the storage backend.
    ///
    /// Falls back to the durable write-through path when the overlay
    /// is at `KISEKI_COMPOSITION_PENDING_MAX` — a volatile create
    /// never rejects on the bound.
    ///
    /// # Errors
    ///
    /// Same surface as `create`: `NamespaceNotFound`,
    /// `ReadOnlyNamespace`; `Storage` only on the overflow fallback.
    pub fn create_volatile(
        &self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError> {
        // Snapshot namespace metadata under the read lock without
        // holding it across the overlay/storage write (same shape as
        // `create`).
        let ns_snap = {
            let nss = self.namespaces.read();
            let ns = nss
                .get(&namespace_id)
                .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;
            if ns.read_only {
                return Err(CompositionError::ReadOnlyNamespace(namespace_id));
            }
            (ns.tenant_id, ns.shard_id)
        };

        let id = CompositionId(uuid::Uuid::new_v4());
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id,
            tenant_id: ns_snap.0,
            namespace_id,
            shard_id: ns_snap.1,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
            chunk_plaintext_lens: Vec::new(),
            chunk_locations: Vec::new(),
        };
        let mut pend = self.pending.write();
        if pend.len() >= self.pending_max {
            // Overlay full — durable write-through (never reject).
            drop(pend);
            self.pending_overflows
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.storage.put(comp)?;
            return Ok(id);
        }
        pend.insert(id, comp);
        Ok(id)
    }

    /// Atomic volatile create-then-name with an optional S3-style
    /// conditional check. Mirrors [`Self::create_with_name`] —
    /// namespace snapshot, the SAME 256-way name-shard critical
    /// section, identical conditional semantics — except the row +
    /// binding land in the volatile overlay instead of
    /// `put_with_name`.
    ///
    /// The existing-binding check is `pending_names` ∪
    /// `storage.name_lookup` (a pending bind is strictly newer, so it
    /// wins for the conditional evaluation).
    ///
    /// `perspective_seq` is the ingress-minted seq the eventual
    /// Create delta will carry (`Some` on the async path).
    ///
    /// Falls back to the durable write-through path when the overlay
    /// is at `KISEKI_COMPOSITION_PENDING_MAX` (never rejects on the
    /// bound); in that case any pending bind on this name is dropped
    /// — it is superseded by the strictly-newer durable bind, and
    /// leaving it would shadow the durable winner until the older
    /// comp's drain.
    ///
    /// # Errors
    ///
    /// Same surface as `create_with_name`: `NamespaceNotFound`,
    /// `ReadOnlyNamespace`, `PreconditionFailed`; `Storage` on the
    /// existing-binding lookup or the overflow fallback.
    pub fn create_with_name_volatile(
        &self,
        namespace_id: NamespaceId,
        name: String,
        cond: Option<&dyn ConditionalCheck>,
        chunks: Vec<ChunkId>,
        size: u64,
        perspective_seq: Option<PerspectiveSeq>,
    ) -> Result<CompositionId, CompositionError> {
        let ns_snap = {
            let nss = self.namespaces.read();
            let ns = nss
                .get(&namespace_id)
                .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;
            if ns.read_only {
                return Err(CompositionError::ReadOnlyNamespace(namespace_id));
            }
            (ns.tenant_id, ns.shard_id)
        };

        let id = CompositionId(uuid::Uuid::new_v4());
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id,
            tenant_id: ns_snap.0,
            namespace_id,
            shard_id: ns_snap.1,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
            chunk_plaintext_lens: Vec::new(),
            chunk_locations: Vec::new(),
        };

        // SAME per-name shard critical section as `create_with_name`:
        // existing-check + conditional + publish serialise per name so
        // `If-None-Match: *` stays race-free — including against
        // concurrent durable writers on the same name (they contend on
        // the same shard).
        let _g = self.name_locks[name_shard(namespace_id, &name)].lock();
        let key = (namespace_id, name.clone());
        let pending_existing = self
            .pending_names
            .read()
            .by_name
            .get(&key)
            .map(|&(cid, _)| cid);
        let storage_existing = self.storage.name_lookup(namespace_id, &name)?;
        // pending ∪ storage with pending (strictly newer) winning —
        // conditional semantics unchanged from the durable path.
        let existing = pending_existing.or(storage_existing);
        if let Some(cond) = cond {
            cond.check(existing)
                .map_err(CompositionError::PreconditionFailed)?;
        }
        let mut pend = self.pending.write();
        if pend.len() >= self.pending_max {
            // Overlay full — durable write-through under the same
            // shard lock (never reject). `put_with_name`'s prior-id
            // cascade wants the STORAGE binding: a pending-only bind
            // has no durable reverse row to cascade-drop.
            drop(pend);
            self.pending_overflows
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.storage
                .put_with_name(comp, namespace_id, name, storage_existing)?;
            self.pending_names.write().remove_by_name(&key);
            return Ok(id);
        }
        // Row before name: a concurrent `lookup_by_name` → `get` must
        // never observe the name without the row.
        pend.insert(id, comp);
        drop(pend);
        self.pending_names.write().bind(key, id, perspective_seq);
        Ok(id)
    }

    /// Drop a volatile-created row + its name bindings — the rollback
    /// path for a failed async fan. Returns `true` when a pending row
    /// was present; `false` means the create either fell back to the
    /// durable path on overlay overflow (the caller deletes durably)
    /// or was already drained by the hydrator.
    ///
    /// Name bindings are dropped by `comp_id` only — NOT by name — so
    /// a newer pending bind on the same name (different `comp_id`)
    /// survives (LWW).
    pub fn discard_volatile(&self, id: CompositionId) -> bool {
        let removed = self.pending.write().remove(&id).is_some();
        self.pending_names.write().remove_by_id(id);
        removed
    }

    /// Hydrator hook: a delta touching `id` just applied durably —
    /// drop the volatile overlay row + its name bindings so the (now
    /// stale) overlay can't shadow later Update/Delete deltas applied
    /// through the storage backend. Same `comp_id`-keyed LWW
    /// semantics as [`Self::discard_volatile`]: a newer pending bind
    /// on the same name survives.
    ///
    /// Pre-P4-parity metadata merge (#226 adversarial blocker):
    /// before the overlay row drops, ingress-local metadata it holds
    /// (`content_type` — see [`Self::merge_drained_overlay_metadata`])
    /// is written through to the freshly-materialized durable row.
    /// Without the merge, a Content-Type set within the hydration lag
    /// window would permanently revert to `None` here — no delta
    /// carries it.
    ///
    /// Locking: takes the per-id shard lock (same order as
    /// `set_content_type`: id-lock → `pending` → storage →
    /// `read_cache`), so a concurrent `set_content_type` either
    /// mutates the overlay before we remove it (we merge its value)
    /// or misses the overlay after we release and takes the durable
    /// path itself (its newer value wins). Safe to call from inside
    /// the hydrator's `with_storage_locked` closure — that closure is
    /// lock-free post-Mutex-removal (the storage round-trip in the
    /// merge cannot deadlock against it), and the hydrator holds no
    /// id-shard locks.
    pub fn drain_pending(&self, id: CompositionId) {
        let _g = self.id_locks[id_shard(id)].lock();
        let overlay = self.pending.write().remove(&id);
        self.pending_names.write().remove_by_id(id);
        if let Some(overlay) = overlay {
            self.merge_drained_overlay_metadata(&overlay);
        }
    }

    /// Pre-P4-parity (#226): write ingress-local overlay metadata
    /// through to the freshly-durable row at drain time.
    ///
    /// `content_type` is the only production field today. No delta
    /// carries it — it was ingress-local even pre-P4 (S3 `PutObject`
    /// calls `set_object_content_type` AFTER the write ack), so the
    /// row `stage_create` materializes always has `content_type:
    /// None` and the overlay row holds the only copy. Pre-P4 the row
    /// was durable at ack, `set_content_type` wrote storage directly,
    /// and `stage_create`'s skip-on-exists preserved it; this merge
    /// restores that parity on the volatile path.
    ///
    /// The other overlay-mutating arms need no merge: `update` /
    /// `delete` convergence rides the Update / Delete deltas the
    /// callers emit (sequence order beats the Create), and per the P4
    /// review `update` / `rename` have no production emitters against
    /// a volatile row today. A future overlay-only (delta-less) field
    /// joins this merge by extending the comparison below.
    ///
    /// Caller must hold the per-id shard lock for `overlay.id`.
    fn merge_drained_overlay_metadata(&self, overlay: &Composition) {
        let Some(ct) = overlay.content_type.as_deref() else {
            // Nothing ingress-local to merge — zero storage I/O on
            // the common (no Content-Type) drain.
            return;
        };
        let id = overlay.id;
        match self.storage.get(id) {
            Ok(Some(mut durable)) => {
                if durable.content_type.as_deref() == Some(ct) {
                    // Already merged (replayed drain) — no extra write.
                    return;
                }
                durable.content_type = Some(ct.to_owned());
                match self.storage.put(durable) {
                    // Same publish discipline as `set_content_type`:
                    // drop the cache entry under the id-shard lock.
                    Ok(()) => {
                        self.read_cache.lock().pop(&id);
                    }
                    Err(e) => tracing::warn!(
                        comp_id = %id.0, error = %e,
                        "drain_pending: content-type merge write failed — \
                         Content-Type reverts to the durable value",
                    ),
                }
            }
            // The batch that drained this id also removed the row
            // (Delete in the same poll) — nothing to merge onto.
            Ok(None) => {}
            Err(e) => tracing::warn!(
                comp_id = %id.0, error = %e,
                "drain_pending: content-type merge read failed — \
                 Content-Type reverts to the durable value",
            ),
        }
    }

    /// Volatile-overlay size (gauge — rows currently pending durable
    /// materialization by the hydrator).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.read().len()
    }

    /// How many volatile creates fell back to the durable
    /// write-through path because the overlay was at
    /// `KISEKI_COMPOSITION_PENDING_MAX` (counter).
    #[must_use]
    pub fn pending_overflows(&self) -> u64 {
        self.pending_overflows
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Return a snapshot of the parts uploaded for a multipart
    /// upload. Returns an empty Vec if the upload doesn't exist.
    /// Used by `complete_multipart` to build the `new_chunks` list
    /// for the Raft Create-delta from each part's tracked
    /// `was_new` bit.
    #[must_use]
    pub fn multipart_parts(&self, upload_id: &str) -> Vec<crate::multipart::MultipartPart> {
        self.multiparts
            .lock()
            .get(upload_id)
            .map(|(u, _)| u.parts.clone())
            .unwrap_or_default()
    }

    /// Apply a leader-emitted Update delta to a follower's local store
    /// (Phase 17 item 1).
    ///
    /// Replaces the composition's chunks + size and bumps `version`.
    /// Idempotent: if the composition already has these exact chunks
    /// and size, this is a no-op (don't double-bump version on
    /// re-applied deltas).
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::CompositionNotFound` if the
    /// composition isn't present locally — Phase 16f's hydrator
    /// applies deltas in sequence order, so a follower seeing an
    /// Update with no prior Create indicates either a missing Create
    /// (data-loss bug) or a hydrator that started past the Create's
    /// sequence (operator error). Both should surface, not silently
    /// swallow.
    pub fn update_at(
        &self,
        comp_id: CompositionId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<(), CompositionError> {
        let _g = self.id_locks[id_shard(comp_id)].lock();
        let mut comp = self
            .storage
            .get(comp_id)?
            .ok_or(CompositionError::CompositionNotFound(comp_id))?;
        if comp.chunks == chunks && comp.size == size {
            return Ok(()); // already at target state — idempotent
        }
        comp.chunks = chunks;
        comp.size = size;
        comp.version += 1;
        comp.has_inline_data =
            comp.chunks.is_empty() && comp.size > 0 && comp.size <= INLINE_DATA_THRESHOLD;
        self.storage.put(comp)?;
        self.read_cache.lock().pop(&comp_id);
        Ok(())
    }

    /// Apply a leader-emitted Delete delta to a follower's local store
    /// (Phase 17 item 1).
    ///
    /// Removes the composition. Idempotent: if the composition is
    /// already absent (e.g. delta re-applied, or follower missed the
    /// Create somehow), returns `Ok(())`. Chunk refcount management
    /// is the leader's responsibility via `decrement_chunk_refcount`
    /// on the per-shard Raft state machine (Phase 16c); the follower
    /// just drops the composition record.
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::Storage` on backend failure.
    pub fn delete_at(&self, comp_id: CompositionId) -> Result<(), CompositionError> {
        let _g = self.id_locks[id_shard(comp_id)].lock();
        self.storage.remove(comp_id)?;
        self.read_cache.lock().pop(&comp_id);
        Ok(())
    }
}

impl Default for CompositionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositionOps for CompositionStore {
    fn create(
        &self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError> {
        // Snapshot namespace metadata under the read lock without
        // holding it across the storage write.
        let ns_snap = {
            let nss = self.namespaces.read();
            let ns = nss
                .get(&namespace_id)
                .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;
            if ns.read_only {
                return Err(CompositionError::ReadOnlyNamespace(namespace_id));
            }
            (ns.tenant_id, ns.shard_id)
        };

        let id = CompositionId(uuid::Uuid::new_v4());
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id,
            tenant_id: ns_snap.0,
            namespace_id,
            shard_id: ns_snap.1,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
            chunk_plaintext_lens: Vec::new(),
            chunk_locations: Vec::new(),
        };
        self.storage.put(comp)?;
        Ok(id)
    }

    fn get(&self, id: CompositionId) -> Result<Composition, CompositionError> {
        // Cache hit: cheap path, returns owned clone of the cached
        // value. `LruCache::get` updates recency, so the lock here is
        // a write-style lock — that's fine, this critical section
        // holds nothing else.
        if let Some(hit) = self.read_cache.lock().get(&id).cloned() {
            return Ok(hit);
        }
        // P4 (#226): volatile overlay — rows created at ack on async
        // surfaces live here until the hydrator materializes them
        // durably. Checked after the LRU (a cache entry implies the
        // row was already durable once) and before storage so a fresh
        // async write doesn't 404 within the hydration lag window.
        // Pending hits are NOT inserted into the read cache — the
        // cache invalidation contract is tied to storage mutations.
        if let Some(hit) = self.pending.read().get(&id).cloned() {
            return Ok(hit);
        }
        // Miss: take the per-id shard lock so we don't race with a
        // concurrent mutation on this same id. The shard lock plays
        // the same lock-ordering role the old global storage Mutex
        // did — any mutation that committed before we acquire it is
        // observable by the storage read below; any mutation
        // queued after waits.
        let _g = self.id_locks[id_shard(id)].lock();
        let comp = self.storage.get(id)?;
        if let Some(ref c) = comp {
            self.read_cache.lock().put(id, c.clone());
        }
        comp.ok_or(CompositionError::CompositionNotFound(id))
    }

    fn update(
        &self,
        id: CompositionId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<u64, CompositionError> {
        let _g = self.id_locks[id_shard(id)].lock();
        // P4 (#226): RMW on a volatile row mutates the overlay in
        // place — a storage-only read would 404 a fresh async object
        // within the hydration lag window. The Update delta the
        // caller emits converges the durable state after the Create
        // delta materializes (deltas apply in sequence order).
        if let Some(comp) = self.pending.write().get_mut(&id) {
            comp.version += 1;
            comp.chunks.clone_from(&chunks);
            comp.size = size;
            return Ok(comp.version);
        }
        let mut comp = self
            .storage
            .get(id)?
            .ok_or(CompositionError::CompositionNotFound(id))?;
        comp.version += 1;
        comp.chunks.clone_from(&chunks);
        comp.size = size;
        let version = comp.version;
        self.storage.put(comp)?;
        // Drop the cache entry while still holding the per-id shard
        // lock so any concurrent reader either sees the new value
        // via the storage backend or waits on the same shard lock.
        self.read_cache.lock().pop(&id);
        Ok(version)
    }

    fn delete(&self, id: CompositionId) -> Result<DeleteResult, CompositionError> {
        let versioning_enabled_for = |ns: NamespaceId| -> bool {
            self.namespaces
                .read()
                .get(&ns)
                .is_some_and(|n| n.versioning_enabled)
        };
        let _g = self.id_locks[id_shard(id)].lock();
        // P4 (#226): delete of a fresh (volatile) row operates on the
        // overlay — same versioning semantics as the durable path.
        // The name bindings drop by comp_id (LWW: a newer pending
        // bind on the same name survives). The durable state
        // converges when the Delete delta applies after the Create.
        {
            let mut pend = self.pending.write();
            if let Some(comp) = pend.get_mut(&id) {
                if versioning_enabled_for(comp.namespace_id) {
                    comp.version += 1;
                    return Ok(DeleteResult::DeleteMarker);
                }
            }
            if let Some(comp) = pend.remove(&id) {
                drop(pend);
                self.pending_names.write().remove_by_id(id);
                return Ok(DeleteResult::Removed(comp.chunks));
            }
        }
        let mut comp = self
            .storage
            .get(id)?
            .ok_or(CompositionError::CompositionNotFound(id))?;

        if versioning_enabled_for(comp.namespace_id) {
            // Versioned delete: keep all versions, just bump version as
            // a tombstone marker. Chunk refcounts are NOT decremented.
            comp.version += 1;
            self.storage.put(comp)?;
            self.read_cache.lock().pop(&id);
            Ok(DeleteResult::DeleteMarker)
        } else {
            self.storage.remove(id)?;
            self.read_cache.lock().pop(&id);
            Ok(DeleteResult::Removed(comp.chunks))
        }
    }

    fn rename(
        &self,
        id: CompositionId,
        target_namespace: NamespaceId,
    ) -> Result<(), CompositionError> {
        let target_shard = self
            .namespaces
            .read()
            .get(&target_namespace)
            .map(|n| n.shard_id)
            .ok_or(CompositionError::NamespaceNotFound(target_namespace))?;

        let _g = self.id_locks[id_shard(id)].lock();
        // P4 (#226): rename of a fresh (volatile) row mutates the
        // overlay in place — same I-L8 cross-shard check as the
        // durable path.
        {
            let mut pend = self.pending.write();
            if let Some(comp) = pend.get_mut(&id) {
                if comp.shard_id != target_shard {
                    return Err(CompositionError::CrossShardRename(
                        comp.shard_id,
                        target_shard,
                    ));
                }
                comp.namespace_id = target_namespace;
                return Ok(());
            }
        }
        let mut comp = self
            .storage
            .get(id)?
            .ok_or(CompositionError::CompositionNotFound(id))?;

        // I-L8: cross-shard rename → EXDEV.
        if comp.shard_id != target_shard {
            return Err(CompositionError::CrossShardRename(
                comp.shard_id,
                target_shard,
            ));
        }

        comp.namespace_id = target_namespace;
        self.storage.put(comp)?;
        self.read_cache.lock().pop(&id);
        Ok(())
    }

    fn start_multipart(&self, namespace_id: NamespaceId) -> Result<String, CompositionError> {
        if !self.namespaces.read().contains_key(&namespace_id) {
            return Err(CompositionError::NamespaceNotFound(namespace_id));
        }
        let upload_id = uuid::Uuid::new_v4().to_string();
        self.multiparts.lock().insert(
            upload_id.clone(),
            (MultipartUpload::new(upload_id.clone()), namespace_id),
        );
        Ok(upload_id)
    }

    fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        chunk_id: ChunkId,
        size: u64,
        was_new: bool,
    ) -> Result<(), CompositionError> {
        let mut multiparts = self.multiparts.lock();
        let (upload, _ns_id) = multiparts
            .get_mut(upload_id)
            .ok_or_else(|| CompositionError::MultipartNotFound(upload_id.to_owned()))?;

        if !upload.add_part(crate::multipart::MultipartPart {
            part_number,
            chunk_id,
            size,
            was_new,
        }) {
            return Err(CompositionError::MultipartNotFinalized(
                upload_id.to_owned(),
            ));
        }
        Ok(())
    }

    fn abort_multipart(&self, upload_id: &str) -> Result<(), CompositionError> {
        let mut multiparts = self.multiparts.lock();
        let (upload, _ns_id) = multiparts
            .get_mut(upload_id)
            .ok_or_else(|| CompositionError::MultipartNotFound(upload_id.to_owned()))?;

        if !upload.abort() {
            return Err(CompositionError::MultipartNotFinalized(
                upload_id.to_owned(),
            ));
        }
        Ok(())
    }

    fn finalize_multipart(&self, upload_id: &str) -> Result<CompositionId, CompositionError> {
        // Drop the multipart-state lock before calling self.create_with_lens
        // (which takes the storage lock) to keep the critical sections
        // disjoint.
        let (chunks, chunk_plaintext_lens, size, ns_id) = {
            let mut multiparts = self.multiparts.lock();
            let (upload, ns_id) = multiparts
                .get_mut(upload_id)
                .ok_or_else(|| CompositionError::MultipartNotFound(upload_id.to_owned()))?;

            if !upload.finalize() {
                return Err(CompositionError::MultipartNotFinalized(
                    upload_id.to_owned(),
                ));
            }

            let chunks: Vec<ChunkId> = upload.parts.iter().map(|p| p.chunk_id).collect();
            // Multipart parts have arbitrary sizes — capture each
            // part's plaintext length so the read path can compute
            // per-chunk file offsets without assuming the regular
            // MAX_PLAINTEXT_PER_CHUNK grid.
            let lens: Vec<u32> = upload
                .parts
                .iter()
                .map(|p| u32::try_from(p.size).unwrap_or(u32::MAX))
                .collect();
            let size = upload.total_size();
            (chunks, lens, size, *ns_id)
        };

        // Create the composition now that it's visible (I-L5).
        self.create_with_lens(ns_id, chunks, chunk_plaintext_lens, size)
    }
}

/// Compute the hashed key for a composition — deterministic routing key.
///
/// Uses UUID v5 (SHA-1 based, deterministic) of `namespace_id` || `composition_id`.
/// Stable across restarts (PIPE-ADV-3).
#[must_use]
pub fn composition_hash_key(ns: NamespaceId, comp: CompositionId) -> [u8; 32] {
    let combined = uuid::Uuid::new_v5(&ns.0, comp.0.as_bytes());
    let mut buf = [0u8; 32];
    buf[..16].copy_from_slice(combined.as_bytes());
    // Mirror to fill 32 bytes deterministically.
    buf[16..32].copy_from_slice(combined.as_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

    fn test_shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }

    fn make_ns(id: u128, tenant: OrgId, shard: ShardId) -> Namespace {
        Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(id)),
            tenant_id: tenant,
            shard_id: shard,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
            size_band_pools: crate::namespace::NamespaceSizeBandPools::default(),
        }
    }

    fn setup() -> CompositionStore {
        let store = CompositionStore::new();
        store.add_namespace(make_ns(10, test_tenant(), test_shard()));
        store
    }

    fn test_ns() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(10))
    }

    #[test]
    fn create_and_get() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 1024)
            .unwrap();

        let comp = store.get(id).unwrap();
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.chunks.len(), 1);
        assert_eq!(comp.size, 1024);
    }

    #[test]
    fn delete_removes_composition() {
        let store = setup();
        let id = store.create(test_ns(), vec![], 0).unwrap();
        let result = store.delete(id).unwrap();
        assert!(matches!(result, DeleteResult::Removed(_)));
        assert!(store.get(id).is_err());
    }

    #[test]
    fn cross_shard_rename_returns_exdev() {
        let store = setup();
        store.add_namespace(make_ns(
            20,
            test_tenant(),
            ShardId(uuid::Uuid::from_u128(2)),
        ));

        let id = store.create(test_ns(), vec![], 0).unwrap();
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(20)));
        assert!(matches!(
            result,
            Err(CompositionError::CrossShardRename(_, _))
        ));
    }

    #[test]
    fn same_shard_rename_succeeds() {
        let store = setup();
        store.add_namespace(make_ns(11, test_tenant(), test_shard()));

        let id = store.create(test_ns(), vec![], 0).unwrap();
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(11)));
        assert!(result.is_ok());
    }

    #[test]
    fn read_only_namespace_rejects_create() {
        let store = CompositionStore::new();
        let mut ns = make_ns(10, test_tenant(), test_shard());
        ns.read_only = true;
        store.add_namespace(ns);

        let result = store.create(test_ns(), vec![], 0);
        assert!(matches!(
            result,
            Err(CompositionError::ReadOnlyNamespace(_))
        ));
    }

    #[test]
    fn multipart_lifecycle() {
        let store = setup();
        let upload_id = store
            .start_multipart(test_ns())
            .unwrap_or_else(|_| unreachable!());

        // Add parts directly to the multipart. Scope the lock so it
        // drops before `finalize_multipart` re-acquires it —
        // `parking_lot::Mutex` is non-reentrant; holding it across
        // the call would deadlock.
        {
            let mut multiparts = store.multiparts.lock();
            if let Some((upload, _)) = multiparts.get_mut(&upload_id) {
                upload.add_part(crate::multipart::MultipartPart {
                    part_number: 1,
                    chunk_id: ChunkId([0x01; 32]),
                    size: 512,
                    was_new: true,
                });
                upload.add_part(crate::multipart::MultipartPart {
                    part_number: 2,
                    chunk_id: ChunkId([0x02; 32]),
                    size: 512,
                    was_new: true,
                });
            }
        }

        let comp_id = store
            .finalize_multipart(&upload_id)
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(comp_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 1024);
    }

    #[test]
    fn versioning() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap_or_else(|_| unreachable!());

        assert_eq!(store.get(id).unwrap_or_else(|_| unreachable!()).version, 1);

        let v2 = store
            .update(id, vec![ChunkId([0x02; 32]), ChunkId([0x03; 32])], 200)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.version, 2);
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 200);
    }

    #[test]
    fn composition_belongs_to_one_tenant_ix1() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0xaa; 32])], 512)
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        // I-X1: composition is owned by the namespace's tenant.
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.namespace_id, test_ns());
    }

    #[test]
    fn namespace_not_found_returns_error() {
        let store = CompositionStore::new();
        let bogus_ns = NamespaceId(uuid::Uuid::from_u128(999));
        let result = store.create(bogus_ns, vec![], 0);
        assert!(matches!(
            result,
            Err(CompositionError::NamespaceNotFound(_))
        ));
    }

    #[test]
    fn list_compositions_in_namespace() {
        let store = setup();

        let id1 = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap_or_else(|_| unreachable!());
        let id2 = store
            .create(test_ns(), vec![ChunkId([0x02; 32])], 200)
            .unwrap_or_else(|_| unreachable!());
        let id3 = store
            .create(test_ns(), vec![ChunkId([0x03; 32])], 300)
            .unwrap_or_else(|_| unreachable!());

        let listed = store.list_by_namespace(test_ns()).unwrap();
        assert_eq!(listed.len(), 3);

        let listed_ids: Vec<CompositionId> = listed.iter().map(|c| c.id).collect();
        assert!(listed_ids.contains(&id1));
        assert!(listed_ids.contains(&id2));
        assert!(listed_ids.contains(&id3));
    }

    #[test]
    fn count_tracks_compositions() {
        let store = setup();
        assert_eq!(store.count().unwrap(), 0);

        store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count().unwrap(), 1);

        let id2 = store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count().unwrap(), 2);

        let _ = store.delete(id2).unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count().unwrap(), 1);
    }

    // ===================================================================
    // Composition-feature @unit scenario tests
    // ===================================================================

    // --- Scenario: Create a new file composition via protocol gateway ---
    #[test]
    fn create_composition_returns_chunk_ids_for_refcount() {
        let store = setup();
        let c1 = ChunkId([0x01; 32]);
        let c2 = ChunkId([0x02; 32]);
        let id = store.create(test_ns(), vec![c1, c2], 2048).unwrap();
        let comp = store.get(id).unwrap();

        // Composition references chunks that the caller would pass to
        // ChunkStore for refcount tracking.
        assert_eq!(comp.chunks, vec![c1, c2]);
        assert_eq!(comp.shard_id, test_shard());
        assert_eq!(comp.version, 1);
        assert!(!comp.has_inline_data);
    }

    // --- Scenario: Create a small file with inline data ---
    #[test]
    fn create_small_file_sets_inline_data_flag() {
        let store = setup();
        // 512 bytes, no chunk IDs — data would be inline in the delta payload.
        let id = store.create(test_ns(), vec![], 512).unwrap();
        let comp = store.get(id).unwrap();

        assert!(comp.has_inline_data);
        assert!(comp.chunks.is_empty());
        assert_eq!(comp.size, 512);
    }

    #[test]
    fn create_above_threshold_not_inline() {
        let store = setup();
        // 8192 bytes with a chunk ref — not inline.
        let id = store
            .create(test_ns(), vec![ChunkId([0xaa; 32])], 8192)
            .unwrap();
        let comp = store.get(id).unwrap();
        assert!(!comp.has_inline_data);
    }

    #[test]
    fn create_zero_size_not_inline() {
        let store = setup();
        // Empty file (size 0, no chunks) — not inline (nothing to inline).
        let id = store.create(test_ns(), vec![], 0).unwrap();
        let comp = store.get(id).unwrap();
        assert!(!comp.has_inline_data);
    }

    // --- Scenario: Append data to an existing composition ---
    #[test]
    fn append_extends_chunk_list() {
        let store = setup();
        let c1 = ChunkId([0x01; 32]);
        let c2 = ChunkId([0x02; 32]);
        let id = store
            .create(test_ns(), vec![c1, c2], 128 * 1024 * 1024)
            .unwrap();

        let c3 = ChunkId([0x03; 32]);
        let c4 = ChunkId([0x04; 32]);
        let v2 = store
            .update(id, vec![c1, c2, c3, c4], 256 * 1024 * 1024)
            .unwrap();
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap();
        assert_eq!(comp.chunks, vec![c1, c2, c3, c4]);
    }

    // --- Scenario: Overwrite a byte range in a composition ---
    #[test]
    fn overwrite_replaces_chunk_in_list() {
        let store = setup();
        let c1 = ChunkId([0x01; 32]);
        let c2 = ChunkId([0x02; 32]);
        let c3 = ChunkId([0x03; 32]);
        let id = store
            .create(test_ns(), vec![c1, c2, c3], 192 * 1024 * 1024)
            .unwrap();

        // Replace c2 with c2_prime (byte-range overwrite of second chunk).
        let c2_prime = ChunkId([0x22; 32]);
        let v2 = store
            .update(id, vec![c1, c2_prime, c3], 192 * 1024 * 1024)
            .unwrap();
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap();
        assert_eq!(comp.chunks, vec![c1, c2_prime, c3]);
        // c2 is no longer referenced — caller decrements its refcount.
        assert!(!comp.chunks.contains(&c2));
    }

    // --- Scenario: S3 multipart upload (I-L5) ---
    #[test]
    fn multipart_not_visible_before_finalize_il5() {
        let store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();

        store
            .upload_part(&upload_id, 1, ChunkId([0x10; 32]), 1024, true)
            .unwrap();
        store
            .upload_part(&upload_id, 2, ChunkId([0x11; 32]), 1024, true)
            .unwrap();
        store
            .upload_part(&upload_id, 3, ChunkId([0x12; 32]), 1024, true)
            .unwrap();

        // Before finalize: no composition exists for these parts (I-L5).
        assert_eq!(store.count().unwrap(), 0);

        let comp_id = store.finalize_multipart(&upload_id).unwrap();
        let comp = store.get(comp_id).unwrap();
        assert_eq!(comp.chunks.len(), 3);
        assert_eq!(comp.size, 3072);
    }

    // --- Scenario: Multipart upload aborted ---
    #[test]
    fn multipart_abort_no_composition_created() {
        let store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();

        store
            .upload_part(&upload_id, 1, ChunkId([0x10; 32]), 1024, true)
            .unwrap();
        store
            .upload_part(&upload_id, 2, ChunkId([0x11; 32]), 1024, true)
            .unwrap();

        store.abort_multipart(&upload_id).unwrap();

        // No composition was created — chunks have refcount 0.
        assert_eq!(store.count().unwrap(), 0);

        // Verify the upload is in Aborted state — cannot finalize.
        let result = store.finalize_multipart(&upload_id);
        assert!(result.is_err());
    }

    #[test]
    fn aborted_multipart_rejects_further_parts() {
        let store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();
        store.abort_multipart(&upload_id).unwrap();

        let result = store.upload_part(&upload_id, 1, ChunkId([0x10; 32]), 512, true);
        assert!(result.is_err());
    }

    // --- Scenario: Delete a composition (refcount tracking) ---
    #[test]
    fn delete_returns_chunk_ids_for_refcount_decrement() {
        let store = setup();
        let c5 = ChunkId([0x05; 32]);
        let c6 = ChunkId([0x06; 32]);
        let id = store.create(test_ns(), vec![c5, c6], 1024).unwrap();

        let result = store.delete(id).unwrap();
        // Caller uses the returned chunk IDs to decrement refcounts.
        assert_eq!(result, DeleteResult::Removed(vec![c5, c6]));
        assert!(store.get(id).is_err());
    }

    // --- Scenario: Delete composition with object versioning enabled ---
    #[test]
    fn versioned_delete_creates_delete_marker() {
        let store = CompositionStore::new();
        let mut ns = make_ns(10, test_tenant(), test_shard());
        ns.versioning_enabled = true;
        store.add_namespace(ns);

        let c1 = ChunkId([0x01; 32]);
        let id = store.create(test_ns(), vec![c1], 100).unwrap();
        assert_eq!(store.get(id).unwrap().version, 1);

        // Create versions v2, v3.
        store.update(id, vec![ChunkId([0x02; 32])], 200).unwrap();
        store.update(id, vec![ChunkId([0x03; 32])], 300).unwrap();
        assert_eq!(store.get(id).unwrap().version, 3);

        let result = store.delete(id).unwrap();
        assert_eq!(result, DeleteResult::DeleteMarker);

        // Composition still exists (versioned — not removed).
        let comp = store.get(id).unwrap();
        // Version bumped for the tombstone.
        assert_eq!(comp.version, 4);
        // Chunk refcounts are NOT decremented (caller checks DeleteMarker).
    }

    // --- Scenario: Intra-tenant dedup — same chunk ID yields same ref ---
    #[test]
    fn intra_tenant_dedup_same_chunk_id() {
        let store = setup();
        let chunk_abc = ChunkId([0xab; 32]); // sha256(P) = "abc"

        let id_a = store.create(test_ns(), vec![chunk_abc], 1024).unwrap();
        let id_b = store.create(test_ns(), vec![chunk_abc], 1024).unwrap();

        let comp_a = store.get(id_a).unwrap();
        let comp_b = store.get(id_b).unwrap();

        // Both compositions reference the same chunk — refcount would be 2.
        assert_eq!(comp_a.chunks, vec![chunk_abc]);
        assert_eq!(comp_b.chunks, vec![chunk_abc]);
        // The ChunkStore (separate) handles the actual refcount.
    }

    // --- Scenario: Cross-tenant dedup (default tenants) ---
    #[test]
    fn cross_tenant_dedup_same_chunk_id() {
        let store = CompositionStore::new();
        let tenant_pharma = OrgId(uuid::Uuid::from_u128(100));
        let tenant_biotech = OrgId(uuid::Uuid::from_u128(200));
        store.add_namespace(make_ns(10, tenant_pharma, test_shard()));
        store.add_namespace(make_ns(20, tenant_biotech, test_shard()));

        let chunk_abc = ChunkId([0xab; 32]);
        let ns_pharma = NamespaceId(uuid::Uuid::from_u128(10));
        let ns_biotech = NamespaceId(uuid::Uuid::from_u128(20));

        let id_p = store.create(ns_pharma, vec![chunk_abc], 1024).unwrap();
        let id_b = store.create(ns_biotech, vec![chunk_abc], 1024).unwrap();

        // Both compositions reference the same chunk ID — dedup at ChunkStore.
        assert_eq!(store.get(id_p).unwrap().chunks, vec![chunk_abc]);
        assert_eq!(store.get(id_b).unwrap().chunks, vec![chunk_abc]);
        // Different tenants own the compositions.
        assert_eq!(store.get(id_p).unwrap().tenant_id, tenant_pharma);
        assert_eq!(store.get(id_b).unwrap().tenant_id, tenant_biotech);
    }

    // --- Scenario: No cross-tenant dedup for HMAC opted-out tenant ---
    #[test]
    fn hmac_tenant_different_chunk_id_no_dedup() {
        let store = CompositionStore::new();
        let tenant_defense = OrgId(uuid::Uuid::from_u128(300));
        let tenant_pharma = OrgId(uuid::Uuid::from_u128(100));
        store.add_namespace(make_ns(30, tenant_defense, test_shard()));
        store.add_namespace(make_ns(10, tenant_pharma, test_shard()));

        // HMAC-derived chunk ID vs SHA256 chunk ID for the same plaintext.
        let chunk_hmac = ChunkId([0xde; 32]); // HMAC(P, defense_key) = "def456"
        let chunk_sha = ChunkId([0xab; 32]); // sha256(P) = "abc123"

        let ns_defense = NamespaceId(uuid::Uuid::from_u128(30));
        let ns_pharma = NamespaceId(uuid::Uuid::from_u128(10));

        let id_d = store.create(ns_defense, vec![chunk_hmac], 1024).unwrap();
        let id_p = store.create(ns_pharma, vec![chunk_sha], 1024).unwrap();

        // Different chunk IDs — no dedup match.
        assert_ne!(
            store.get(id_d).unwrap().chunks[0],
            store.get(id_p).unwrap().chunks[0]
        );
    }

    // --- Scenario: Namespace inherits compliance tags ---
    #[test]
    fn namespace_inherits_org_compliance_tags() {
        use crate::namespace::ComplianceTag;

        let org_tags = vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr];
        let ns = Namespace {
            id: test_ns(),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: vec![ComplianceTag::RevFadp],
            tier_policy: Vec::new(),
            size_band_pools: crate::namespace::NamespaceSizeBandPools::default(),
        };

        let effective = ns.effective_compliance_tags(&org_tags);
        assert_eq!(
            effective,
            vec![
                ComplianceTag::Hipaa,
                ComplianceTag::Gdpr,
                ComplianceTag::RevFadp
            ]
        );
    }

    #[test]
    fn namespace_compliance_tags_dedup() {
        use crate::namespace::ComplianceTag;

        let org_tags = vec![ComplianceTag::Hipaa];
        let ns = Namespace {
            id: test_ns(),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr],
            tier_policy: Vec::new(),
            size_band_pools: crate::namespace::NamespaceSizeBandPools::default(),
        };

        let effective = ns.effective_compliance_tags(&org_tags);
        // HIPAA appears once despite being in both org and namespace.
        assert_eq!(effective, vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr]);
    }

    // --- Scenario: Chunk write fails during composition create ---
    #[test]
    fn chunk_write_failure_aborts_create_no_partial_state() {
        // Composition creates take chunk IDs after the caller confirms
        // chunk writes. If the caller does not pass chunk IDs (simulating
        // a chunk write failure), no composition is created.
        let store = setup();
        let initial_count = store.count().unwrap();

        // Simulate: chunk write failed, so we never call create().
        // Verify the store has no partial state.
        assert_eq!(store.count().unwrap(), initial_count);

        // Also: creating with valid chunks then deleting leaves no trace.
        let id = store
            .create(test_ns(), vec![ChunkId([0xff; 32])], 100)
            .unwrap();
        assert_eq!(store.count().unwrap(), initial_count + 1);
        let _ = store.delete(id).unwrap();
        assert_eq!(store.count().unwrap(), initial_count);
    }

    // --- Scenario: Delta commit fails after chunk write succeeds ---
    #[test]
    fn delta_commit_failure_rollback_removes_composition() {
        let store = setup();
        let c20 = ChunkId([0x20; 32]);
        let id = store.create(test_ns(), vec![c20], 4096).unwrap();

        // Simulate delta commit failure: caller rolls back by deleting.
        let result = store.delete(id).unwrap();
        assert_eq!(result, DeleteResult::Removed(vec![c20]));
        assert!(store.get(id).is_err());
        // c20 now has refcount 0 (returned to caller for GC).
    }

    // --- Scenario: Collective checkpoint announcement (I-WA1) ---
    #[test]
    fn advisory_hint_does_not_affect_create_correctness() {
        // Advisory hints are pass-through — composition operations succeed
        // identically with or without them (I-WA1).
        let store = setup();
        let chunks = vec![ChunkId([0xcc; 32])];

        // Create without any advisory context.
        let id = store.create(test_ns(), chunks.clone(), 4096).unwrap();
        let comp = store.get(id).unwrap().clone();

        // Verify the composition is correct regardless of advisory state.
        assert_eq!(comp.chunks, chunks);
        assert_eq!(comp.size, 4096);
        assert_eq!(comp.version, 1);
    }

    // --- Scenario: Retention-intent { final } ---
    #[test]
    fn retention_intent_does_not_change_multipart_finalize() {
        // retention_intent is advisory — finalize semantics are unchanged.
        let store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();

        store
            .upload_part(&upload_id, 1, ChunkId([0xa0; 32]), 512, true)
            .unwrap();
        store
            .upload_part(&upload_id, 2, ChunkId([0xa1; 32]), 512, true)
            .unwrap();

        let comp_id = store.finalize_multipart(&upload_id).unwrap();
        let comp = store.get(comp_id).unwrap();

        // I-L5: chunks confirmed and visible only after finalize.
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 1024);
        // I-C2: refcount semantics unchanged by advisory hints.
    }

    // --- Scenario: Caller-scoped refcount activity telemetry ---
    #[test]
    fn rapid_creates_tracked_by_store_count() {
        // Telemetry is an observability concern; unit-level validation:
        // the store tracks composition count accurately under rapid mutations.
        let store = setup();
        let mut ids = Vec::new();
        for i in 0u8..10 {
            let id = store
                .create(test_ns(), vec![ChunkId([i; 32])], 100)
                .unwrap();
            ids.push(id);
        }
        assert_eq!(store.count().unwrap(), 10);

        for id in &ids[..5] {
            let _ = store.delete(*id).unwrap();
        }
        assert_eq!(store.count().unwrap(), 5);
    }

    // --- Scenario: Hint cannot enable cross-namespace creation (I-WA14) ---
    #[test]
    fn create_in_unauthorized_namespace_rejected_regardless_of_hints() {
        let store = setup();
        // Namespace 99 does not exist — any create attempt is rejected
        // regardless of advisory context.
        let bogus_ns = NamespaceId(uuid::Uuid::from_u128(99));
        let result = store.create(bogus_ns, vec![], 0);
        assert!(matches!(
            result,
            Err(CompositionError::NamespaceNotFound(_))
        ));
    }

    // --- Scenario: Advisory disabled — composition path unaffected (I-WA2) ---
    #[test]
    fn all_ops_succeed_without_advisory_context() {
        // Full lifecycle without any advisory integration — correctness
        // is identical (I-WA2).
        let store = setup();

        // Create.
        let c1 = ChunkId([0x01; 32]);
        let id = store.create(test_ns(), vec![c1], 1024).unwrap();

        // Update.
        let c2 = ChunkId([0x02; 32]);
        let v2 = store.update(id, vec![c1, c2], 2048).unwrap();
        assert_eq!(v2, 2);

        // Multipart.
        let upload_id = store.start_multipart(test_ns()).unwrap();
        store
            .upload_part(&upload_id, 1, ChunkId([0x10; 32]), 512, true)
            .unwrap();
        let mp_id = store.finalize_multipart(&upload_id).unwrap();
        assert!(store.get(mp_id).is_ok());

        // Delete.
        let result = store.delete(id).unwrap();
        assert!(matches!(result, DeleteResult::Removed(_)));
    }

    // ---------------------------------------------------------------
    // B1 — concurrent-write contention witness.
    //
    // The pre-B1 contention curve on `InProcessPersistent` regressed
    // at concurrency >= 8: 1→4 scaled linearly, 4→8 plateaued, 8→16
    // *fell back* to 1-thread speed (lock convoy on the outer
    // `Arc<Mutex<CompositionStore>>`). After B1, the per-store locks
    // are field-grained (storage + namespaces + multiparts each have
    // their own Mutex / RwLock), so concurrent PUTs on disjoint
    // composition_ids no longer serialize on a single mutex.
    //
    // This test pins the property: 16 worker threads each do K
    // create()s against a shared `Arc<CompositionStore>`. We assert
    // the final count is exactly `16 * K` and the run completes
    // within a generous wall budget (10 s on any sane dev box).
    // The wall budget is a regression-only guard, NOT a perf gate —
    // the actual perf measurement lives in `kiseki-profile`.
    //
    // Runs in well under 1 s on the in-memory `MemoryStorage`
    // backend; no `#[ignore = "slow:…"]` annotation needed.
    // ---------------------------------------------------------------
    #[test]
    fn concurrent_creates_scale_to_sixteen_writers() {
        use std::sync::Arc;
        use std::time::Instant;

        const WORKERS: usize = 16;
        const PER_WORKER: usize = 200;

        let store = Arc::new(setup());
        let started = Instant::now();
        let handles: Vec<_> = (0..WORKERS)
            .map(|w| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..PER_WORKER {
                        // Distinct chunk_ids per write so there's no
                        // accidental dedup short-circuit at the
                        // composition-table layer.
                        let mut chunk_bytes = [0u8; 32];
                        chunk_bytes[0] = u8::try_from(w).expect("WORKERS fits in u8");
                        let i_bytes = u16::try_from(i)
                            .expect("PER_WORKER fits in u16")
                            .to_le_bytes();
                        chunk_bytes[1] = i_bytes[0];
                        chunk_bytes[2] = i_bytes[1];
                        store
                            .create(test_ns(), vec![ChunkId(chunk_bytes)], 1024)
                            .expect("concurrent create");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread");
        }
        let elapsed = started.elapsed();

        // Correctness: every create produced a fresh row.
        assert_eq!(
            store.count().unwrap(),
            (WORKERS * PER_WORKER) as u64,
            "all concurrent creates should land",
        );
        // Regression guard: 3200 creates against an in-memory
        // backend should finish well under 10 s. If a future change
        // re-introduces a global lock, this trips.
        assert!(
            elapsed.as_secs() < 10,
            "concurrent_creates ran for {elapsed:?} — possible lock-convoy regression",
        );
    }

    // ---------------------------------------------------------------
    // Read-cache (LRU) tests — ADR-042 post-V3 perf sweep.
    //
    // Goal: prove the cache is correct, not that it's fast. The
    // interesting cases are: hit returns a value, mutation
    // invalidates so subsequent reads see the new value, and
    // hydrator-side `invalidate_cache` lets follower reads observe
    // post-batch state.
    // ---------------------------------------------------------------

    #[test]
    fn read_cache_hit_returns_value() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 1024)
            .unwrap();

        // First get populates the cache; second hits the cache.
        let first = store.get(id).unwrap();
        let second = store.get(id).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.version, second.version);
        assert_eq!(first.chunks, second.chunks);
    }

    #[test]
    fn read_cache_invalidates_on_update() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap();
        // Prime the cache with v=1.
        let v1 = store.get(id).unwrap();
        assert_eq!(v1.version, 1);

        // Update bumps to v=2 and must drop the cache entry.
        let v2 = store.update(id, vec![ChunkId([0x02; 32])], 200).unwrap();
        assert_eq!(v2, 2);

        // Subsequent get must reflect the post-update version, not
        // the pre-update cached value.
        let after = store.get(id).unwrap();
        assert_eq!(after.version, 2);
        assert_eq!(after.size, 200);
        assert_eq!(after.chunks, vec![ChunkId([0x02; 32])]);
    }

    #[test]
    fn read_cache_invalidates_on_delete() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap();
        // Prime cache.
        let _ = store.get(id).unwrap();

        store.delete(id).unwrap();
        // Post-delete get must miss cache and surface NotFound from
        // the storage backend, not return the cached pre-delete value.
        assert!(matches!(
            store.get(id),
            Err(CompositionError::CompositionNotFound(_))
        ));
    }

    #[test]
    fn read_cache_invalidates_on_rename() {
        let store = setup();
        store.add_namespace(make_ns(11, test_tenant(), test_shard()));
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap();
        let _ = store.get(id).unwrap();

        store
            .rename(id, NamespaceId(uuid::Uuid::from_u128(11)))
            .unwrap();

        let after = store.get(id).unwrap();
        assert_eq!(after.namespace_id, NamespaceId(uuid::Uuid::from_u128(11)));
    }

    #[test]
    fn read_cache_invalidates_on_set_content_type() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap();
        let v1 = store.get(id).unwrap();
        assert_eq!(v1.content_type, None);

        store
            .set_content_type(id, Some("application/json".into()))
            .unwrap();

        let after = store.get(id).unwrap();
        assert_eq!(after.content_type.as_deref(), Some("application/json"));
    }

    #[test]
    fn invalidate_cache_drops_single_entry() {
        let store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap();

        // Prime, then bypass CompositionOps to mutate storage
        // directly — simulates what the hydrator does on a follower
        // via `with_storage_locked`. Without the explicit
        // invalidate_cache call below, the next get would return a
        // stale clone.
        let _ = store.get(id).unwrap();
        store.with_storage_locked(|s| {
            let mut comp = s.get(id).unwrap().unwrap();
            comp.version = 99;
            comp.size = 999;
            s.put(comp).unwrap();
        });

        // Without invalidate_cache → stale.
        let stale = store.get(id).unwrap();
        assert_eq!(stale.version, 1);

        // After invalidate_cache → fresh from storage.
        store.invalidate_cache(id);
        let fresh = store.get(id).unwrap();
        assert_eq!(fresh.version, 99);
        assert_eq!(fresh.size, 999);
    }

    #[test]
    fn clear_cache_drops_every_entry() {
        let store = setup();
        let id_a = store.create(test_ns(), vec![], 1).unwrap();
        let id_b = store.create(test_ns(), vec![], 2).unwrap();
        // Prime both.
        let _ = store.get(id_a).unwrap();
        let _ = store.get(id_b).unwrap();

        store.clear_cache();
        // Both must miss cache and re-fetch — no panic, correct
        // values.
        assert_eq!(store.get(id_a).unwrap().size, 1);
        assert_eq!(store.get(id_b).unwrap().size, 2);
    }

    /// ADR-045 §D3: the tier policy round-trips through the
    /// `NamespaceCreate` delta payload, and a legacy 49-byte payload
    /// (no appended section) decodes to an empty policy.
    #[test]
    fn namespace_tier_policy_round_trips() {
        use crate::namespace::{Namespace, TierQuota};
        use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
        let ns = Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(7)),
            tenant_id: OrgId(uuid::Uuid::from_u128(8)),
            shard_id: ShardId(uuid::Uuid::from_u128(9)),
            read_only: false,
            versioning_enabled: true,
            compliance_tags: Vec::new(),
            tier_policy: vec![
                TierQuota {
                    tier: "fast".into(),
                    quota_bytes: 10 * 1024 * 1024 * 1024 * 1024,
                },
                TierQuota {
                    tier: "cold".into(),
                    quota_bytes: 0,
                },
            ],
            size_band_pools: crate::namespace::NamespaceSizeBandPools::default(),
        };
        let bytes = encode_namespace_create_payload(&ns);
        let back = decode_namespace_create_payload(&bytes).expect("decode");
        assert_eq!(back.tier_policy, ns.tier_policy);
        assert!(back.versioning_enabled);

        // Legacy fixed-length payload (no tier section) → empty policy.
        let legacy = &bytes[..NAMESPACE_CREATE_PAYLOAD_LEN];
        let back_legacy = decode_namespace_create_payload(legacy).expect("legacy decode");
        assert!(back_legacy.tier_policy.is_empty());
    }

    /// ADR-048 — the `MigrateChunkLocations` payload round-trips
    /// chunk-index → `(offset_in_slab, length)` entries through the
    /// log delta encoding so followers see exactly the same
    /// composition-state mutation the leader committed.
    #[test]
    fn migrate_chunk_locations_payload_round_trip() {
        let comp = CompositionId(uuid::Uuid::from_u128(99));
        let slab = kiseki_common::SlabId(uuid::Uuid::from_u128(77));
        let entries = vec![(0u32, 0u64, 16u32), (1, 16, 32), (5, 1024, 65_536)];
        let payload = encode_migrate_chunk_locations_payload(comp, "cold-ec", slab, &entries);
        let decoded = decode_migrate_chunk_locations_payload(&payload).expect("decode");
        assert_eq!(decoded.composition_id, comp);
        assert_eq!(decoded.pool_name, "cold-ec");
        assert_eq!(decoded.slab_id, slab);
        assert_eq!(decoded.entries, entries);
        // Truncated payload → None, never panics.
        assert!(decode_migrate_chunk_locations_payload(&payload[..10]).is_none());
        // Empty entries section is valid.
        let empty = encode_migrate_chunk_locations_payload(comp, "p", slab, &[]);
        let decoded = decode_migrate_chunk_locations_payload(&empty).expect("decode-empty");
        assert!(decoded.entries.is_empty());
    }

    /// ADR-024 2026-05-31 amendment: the per-namespace size-band pool
    /// selector round-trips through the `NamespaceCreate` payload, and
    /// pre-amendment payloads (no bands section after `tier_policy`)
    /// decode to the default empty selector.
    #[test]
    fn namespace_size_band_pools_round_trip() {
        use crate::namespace::{Namespace, NamespaceSizeBandPools, TierQuota};
        use kiseki_common::ids::{NamespaceId, OrgId, ShardId};

        let ns = Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(11)),
            tenant_id: OrgId(uuid::Uuid::from_u128(12)),
            shard_id: ShardId(uuid::Uuid::from_u128(13)),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: vec![TierQuota {
                tier: "fast".into(),
                quota_bytes: 1024,
            }],
            size_band_pools: NamespaceSizeBandPools {
                inline: Some("nvme-meta".into()),
                replicated: None,
                ec: Some("bulk-ec".into()),
            },
        };
        let bytes = encode_namespace_create_payload(&ns);
        let back = decode_namespace_create_payload(&bytes).expect("decode");
        assert_eq!(back.size_band_pools, ns.size_band_pools);
        assert_eq!(back.tier_policy, ns.tier_policy);

        // Pre-amendment payload: tier policy present but no bands section.
        // We truncate at the byte right after the tier-policy section by
        // re-encoding with empty bands and chopping the trailing 1-byte
        // flag (which is zero in that case).
        let mut empty = ns.clone();
        empty.size_band_pools = NamespaceSizeBandPools::default();
        let mut empty_bytes = encode_namespace_create_payload(&empty);
        // The last byte is the bands_flag = 0; drop it to simulate a
        // record written before the amendment.
        assert_eq!(empty_bytes.last(), Some(&0u8));
        empty_bytes.pop();
        let back_legacy =
            decode_namespace_create_payload(&empty_bytes).expect("pre-amendment decode");
        assert!(back_legacy.size_band_pools.is_empty());
        assert_eq!(back_legacy.tier_policy, empty.tier_policy);
    }

    // -- Composition-create payload round-trip (the one wire shape) --------

    #[test]
    fn create_payload_round_trip_nameless() {
        // Nameless Create (NFS / internal): no name, no lens, no seq.
        let comp_id = CompositionId(uuid::Uuid::from_u128(1));
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let bytes = encode_composition_create_payload(comp_id, ns_id, 7, None, &[], None);
        let (c, n, s, name, lens, seq) =
            decode_composition_create_payload(&bytes).expect("nameless decode");
        assert_eq!(c, comp_id);
        assert_eq!(n, ns_id);
        assert_eq!(s, 7);
        assert!(name.is_none());
        assert!(lens.is_none());
        assert!(seq.is_none());
    }

    #[test]
    fn create_payload_round_trip_named_sync() {
        // Sync-surface named Create: name present, no lens, no seq.
        let comp_id = CompositionId(uuid::Uuid::from_u128(1));
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let bytes =
            encode_composition_create_payload(comp_id, ns_id, 32, Some("named/sync"), &[], None);
        let (_c, _n, _s, name, lens, seq) =
            decode_composition_create_payload(&bytes).expect("named-sync decode");
        assert_eq!(name.as_deref(), Some("named/sync"));
        assert!(lens.is_none());
        assert!(seq.is_none());
    }

    #[test]
    fn create_payload_round_trip_named_async_with_seq() {
        use kiseki_common::ids::NodeId;
        use kiseki_common::time::HybridLogicalClock;
        use kiseki_log::intent::PerspectiveSeq;
        let comp_id = CompositionId(uuid::Uuid::from_u128(7));
        let ns_id = NamespaceId(uuid::Uuid::from_u128(13));
        let seq = PerspectiveSeq(HybridLogicalClock {
            physical_ms: 0xDEAD_BEEF,
            logical: 0xCAFE,
            node_id: NodeId(42),
        });
        let bytes = encode_composition_create_payload(
            comp_id,
            ns_id,
            128,
            Some("lww/file.bin"),
            &[],
            Some(seq),
        );
        let (c, n, s, name, lens, decoded_seq) =
            decode_composition_create_payload(&bytes).expect("named-async decode");
        assert_eq!(c, comp_id);
        assert_eq!(n, ns_id);
        assert_eq!(s, 128);
        assert_eq!(name.as_deref(), Some("lww/file.bin"));
        assert!(lens.is_none());
        assert_eq!(decoded_seq, Some(seq));
    }

    #[test]
    fn create_payload_round_trip_named_with_seq_and_lens() {
        use kiseki_common::ids::NodeId;
        use kiseki_common::time::HybridLogicalClock;
        use kiseki_log::intent::PerspectiveSeq;
        let comp_id = CompositionId(uuid::Uuid::from_u128(9));
        let ns_id = NamespaceId(uuid::Uuid::from_u128(13));
        let seq = PerspectiveSeq(HybridLogicalClock {
            physical_ms: 1_000_000,
            logical: 3,
            node_id: NodeId(1),
        });
        let lens = vec![100u32, 200, 300];
        let bytes = encode_composition_create_payload(
            comp_id,
            ns_id,
            600,
            Some("multipart/key"),
            &lens,
            Some(seq),
        );
        let (_c, _n, _s, name, decoded_lens, decoded_seq) =
            decode_composition_create_payload(&bytes).expect("named+lens+seq decode");
        assert_eq!(name.as_deref(), Some("multipart/key"));
        assert_eq!(decoded_lens, Some(lens));
        assert_eq!(decoded_seq, Some(seq));
    }

    #[test]
    fn create_payload_rejects_trailing_bytes() {
        // Trailing bytes after a well-formed payload are a structural
        // error — the decoder returns None and the hydrator records a
        // permanent skip.
        let comp_id = CompositionId(uuid::Uuid::from_u128(1));
        let ns_id = NamespaceId(uuid::Uuid::from_u128(2));
        let mut bytes = encode_composition_create_payload(comp_id, ns_id, 7, None, &[], None);
        bytes.push(0xAB);
        assert!(decode_composition_create_payload(&bytes).is_none());
    }

    #[test]
    fn create_payload_rejects_short_prefix() {
        // Shorter than the fixed 40-byte prefix — structural failure.
        assert!(decode_composition_create_payload(&[0u8; 5]).is_none());
    }

    /// P4 (#226) — volatile create overlay: rows + name bindings
    /// created at ack time on async surfaces, durably materialized by
    /// the hydrator (drain tests live in `hydrator::tests`).
    mod volatile_overlay {
        use super::*;

        fn pseq(physical_ms: u64) -> PerspectiveSeq {
            PerspectiveSeq(kiseki_common::time::HybridLogicalClock {
                physical_ms,
                logical: 0,
                node_id: kiseki_common::ids::NodeId(1),
            })
        }

        /// `If-None-Match: *` — rejects when any existing binding is
        /// visible.
        struct IfNoneMatchStar;
        impl ConditionalCheck for IfNoneMatchStar {
            fn check(&self, existing: Option<CompositionId>) -> Result<(), String> {
                if existing.is_some() {
                    Err("precondition failed: object exists".to_owned())
                } else {
                    Ok(())
                }
            }
        }

        #[test]
        fn volatile_create_visible_via_read_paths() {
            let store = setup();
            let id = store
                .create_with_name_volatile(
                    test_ns(),
                    "k1".to_owned(),
                    None,
                    vec![ChunkId([0x01; 32])],
                    1024,
                    Some(pseq(1)),
                )
                .unwrap();

            // get
            let comp = store.get(id).unwrap();
            assert_eq!(comp.size, 1024);
            assert_eq!(comp.tenant_id, test_tenant());
            // lookup_by_name
            assert_eq!(store.lookup_by_name(test_ns(), "k1").unwrap(), Some(id));
            // name_for (reverse)
            assert_eq!(
                store.name_for(id).unwrap(),
                Some((test_ns(), "k1".to_owned()))
            );
            // list_names (LIST coherence)
            assert_eq!(
                store.list_names(test_ns(), None).unwrap(),
                vec![("k1".to_owned(), id)]
            );
            // list_by_namespace (by-id LIST coherence)
            assert_eq!(store.list_by_namespace(test_ns()).unwrap().len(), 1);
        }

        /// HARD CONSTRAINT pin: the overlay must NOT be visible
        /// through the `CompositionStorage` trait. The hydrator's
        /// `Staging::view` reads raw storage via `with_storage_locked`
        /// DELIBERATELY — that bypass is what makes the ingress
        /// node's own hydrator stage the durable put. If the overlay
        /// leaked into `CompositionStorage::get`, the skip-on-exists
        /// check in `stage_create` would orphan the row (never
        /// durable, unrecoverable on replay).
        #[test]
        fn volatile_overlay_invisible_to_storage_trait() {
            let store = setup();
            let id = store
                .create_with_name_volatile(
                    test_ns(),
                    "k1".to_owned(),
                    None,
                    vec![],
                    64,
                    Some(pseq(1)),
                )
                .unwrap();

            // Visible through the store wrapper...
            assert!(store.get(id).is_ok());
            // ...but ABSENT through the raw storage trait (the
            // hydrator's view).
            assert!(
                store.with_storage_locked(|s| s.get(id).unwrap()).is_none(),
                "volatile row leaked into CompositionStorage::get — \
                 the hydrator would skip-on-exists and the row would \
                 never be durably materialized",
            );
            assert!(
                store
                    .with_storage_locked(|s| s.name_lookup(test_ns(), "k1").unwrap())
                    .is_none(),
                "volatile name binding leaked into the storage name index",
            );
        }

        #[test]
        fn discard_volatile_drops_row_and_names() {
            let store = setup();
            let id = store
                .create_with_name_volatile(
                    test_ns(),
                    "k1".to_owned(),
                    None,
                    vec![],
                    0,
                    Some(pseq(1)),
                )
                .unwrap();

            assert!(store.discard_volatile(id));
            assert!(store.get(id).is_err());
            assert_eq!(store.lookup_by_name(test_ns(), "k1").unwrap(), None);
            assert_eq!(store.pending_len(), 0);
            // Second discard: row already gone.
            assert!(!store.discard_volatile(id));
        }

        /// LWW: draining an older `comp_id` must NOT drop a newer
        /// pending bind on the same name.
        #[test]
        fn drain_pending_keeps_newer_bind_on_same_name() {
            let store = setup();
            let older = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    1,
                    Some(pseq(1)),
                )
                .unwrap();
            let newer = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    2,
                    Some(pseq(2)),
                )
                .unwrap();
            assert_eq!(store.lookup_by_name(test_ns(), "k").unwrap(), Some(newer));

            store.drain_pending(older);

            assert_eq!(
                store.lookup_by_name(test_ns(), "k").unwrap(),
                Some(newer),
                "draining the older comp_id dropped the newer pending \
                 bind on the same name (drain must key off comp_id, \
                 not name)",
            );
            assert!(store.get(newer).is_ok());
            assert_eq!(store.pending_len(), 1);
        }

        /// #256: `name_for` resolves through the `by_id` index (O(1))
        /// and the index never serves a stale binding — a superseded
        /// older comp falls through to storage, exactly like the
        /// pre-index full-scan (which matched by current map VALUE).
        #[test]
        fn name_for_overlay_index_consistency() {
            let store = setup();
            let older = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    1,
                    Some(pseq(1)),
                )
                .unwrap();
            assert_eq!(
                store.name_for(older).unwrap(),
                Some((test_ns(), "k".to_owned()))
            );

            let newer = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    2,
                    Some(pseq(2)),
                )
                .unwrap();
            // The newer bind owns the name; the older comp's binding
            // is gone from the overlay in BOTH directions.
            assert_eq!(
                store.name_for(newer).unwrap(),
                Some((test_ns(), "k".to_owned()))
            );
            assert_eq!(
                store.name_for(older).unwrap(),
                None,
                "superseded comp must not resolve via a stale by_id entry",
            );

            // Drain the newer comp: both directions clear.
            store.drain_pending(newer);
            assert_eq!(store.name_for(newer).unwrap(), None);
            assert_eq!(store.lookup_by_name(test_ns(), "k").unwrap(), None);
        }

        #[test]
        fn overflow_falls_back_to_durable() {
            let store = CompositionStore::new().with_pending_max(1);
            store.add_namespace(make_ns(10, test_tenant(), test_shard()));

            let first = store.create_volatile(test_ns(), vec![], 0).unwrap();
            let second = store.create_volatile(test_ns(), vec![], 0).unwrap();

            assert_eq!(store.pending_len(), 1);
            assert_eq!(store.pending_overflows(), 1);
            // First is overlay-only; second went durable write-through.
            assert!(store
                .with_storage_locked(|s| s.get(first).unwrap())
                .is_none());
            assert!(store
                .with_storage_locked(|s| s.get(second).unwrap())
                .is_some());
            // Both are readable through the store wrapper.
            assert!(store.get(first).is_ok());
            assert!(store.get(second).is_ok());
        }

        /// Overflow on the named path: the durable bind supersedes any
        /// pending bind on that name (the pending entry must drop so
        /// it can't shadow the strictly-newer durable winner).
        #[test]
        fn overflow_with_name_supersedes_pending_bind() {
            let store = CompositionStore::new().with_pending_max(1);
            store.add_namespace(make_ns(10, test_tenant(), test_shard()));

            let older = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    1,
                    Some(pseq(1)),
                )
                .unwrap();
            // Overlay full → durable write-through.
            let newer = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    2,
                    Some(pseq(2)),
                )
                .unwrap();

            assert_eq!(store.pending_overflows(), 1);
            assert_eq!(
                store.lookup_by_name(test_ns(), "k").unwrap(),
                Some(newer),
                "the stale pending bind shadows the newer durable bind",
            );
            assert_eq!(
                store.with_storage_locked(|s| s.name_lookup(test_ns(), "k").unwrap()),
                Some(newer),
            );
            // The older row stays readable by id until its drain.
            assert!(store.get(older).is_ok());
        }

        /// Conditional semantics unchanged: the existing-check is
        /// `pending_names` ∪ `storage.name_lookup`, so
        /// `If-None-Match: *` sees a pending-only binding.
        #[test]
        fn conditional_check_sees_pending_binding() {
            let store = setup();
            store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![],
                    0,
                    Some(pseq(1)),
                )
                .unwrap();

            let res = store.create_with_name_volatile(
                test_ns(),
                "k".to_owned(),
                Some(&IfNoneMatchStar),
                vec![],
                0,
                Some(pseq(2)),
            );
            assert!(
                matches!(res, Err(CompositionError::PreconditionFailed(_))),
                "If-None-Match: * must reject against a pending-only binding",
            );
        }

        #[test]
        fn update_sees_pending_row() {
            let store = setup();
            let id = store
                .create_volatile(test_ns(), vec![ChunkId([0x01; 32])], 64)
                .unwrap();

            let v = store.update(id, vec![ChunkId([0x02; 32])], 128).unwrap();
            assert_eq!(v, 2);
            let comp = store.get(id).unwrap();
            assert_eq!(comp.size, 128);
            assert_eq!(comp.chunks, vec![ChunkId([0x02; 32])]);
            // Still overlay-only — the RMW must not write through.
            assert!(store.with_storage_locked(|s| s.get(id).unwrap()).is_none());
        }

        #[test]
        fn set_content_type_sees_pending_row() {
            let store = setup();
            let id = store.create_volatile(test_ns(), vec![], 0).unwrap();

            store
                .set_content_type(id, Some("text/plain".to_owned()))
                .unwrap();
            assert_eq!(
                store.get(id).unwrap().content_type.as_deref(),
                Some("text/plain")
            );
            assert!(store.with_storage_locked(|s| s.get(id).unwrap()).is_none());
        }

        #[test]
        fn delete_sees_pending_row_and_drops_names() {
            let store = setup();
            let id = store
                .create_with_name_volatile(
                    test_ns(),
                    "k".to_owned(),
                    None,
                    vec![ChunkId([0x03; 32])],
                    64,
                    Some(pseq(1)),
                )
                .unwrap();

            let res = store.delete(id).unwrap();
            assert!(
                matches!(res, DeleteResult::Removed(chunks) if chunks == vec![ChunkId([0x03; 32])])
            );
            assert!(store.get(id).is_err());
            assert_eq!(store.lookup_by_name(test_ns(), "k").unwrap(), None);
            assert_eq!(store.pending_len(), 0);
        }

        #[test]
        fn rename_sees_pending_row() {
            let store = setup();
            // Same-shard sibling namespace.
            store.add_namespace(make_ns(11, test_tenant(), test_shard()));
            let target = NamespaceId(uuid::Uuid::from_u128(11));
            let id = store.create_volatile(test_ns(), vec![], 0).unwrap();

            store.rename(id, target).unwrap();
            assert_eq!(store.get(id).unwrap().namespace_id, target);
            assert!(store.with_storage_locked(|s| s.get(id).unwrap()).is_none());

            // Cross-shard rename on a pending row → EXDEV (I-L8).
            store.add_namespace(make_ns(
                20,
                test_tenant(),
                ShardId(uuid::Uuid::from_u128(2)),
            ));
            let res = store.rename(id, NamespaceId(uuid::Uuid::from_u128(20)));
            assert!(matches!(res, Err(CompositionError::CrossShardRename(_, _))));
        }

        /// `list_names` merge: pending wins on duplicate names; the
        /// result stays name-sorted (S3 LIST ordering).
        #[test]
        fn list_names_merges_overlay_over_durable() {
            let store = setup();
            // Durable binds for "a" and "b".
            let durable_a = store
                .create_with_name(test_ns(), "a".to_owned(), None, vec![], 0)
                .unwrap();
            store
                .create_with_name(test_ns(), "b".to_owned(), None, vec![], 0)
                .unwrap();
            // Pending overwrite of "a" + a fresh pending "c".
            let pending_a = store
                .create_with_name_volatile(
                    test_ns(),
                    "a".to_owned(),
                    None,
                    vec![],
                    0,
                    Some(pseq(1)),
                )
                .unwrap();
            let pending_c = store
                .create_with_name_volatile(
                    test_ns(),
                    "c".to_owned(),
                    None,
                    vec![],
                    0,
                    Some(pseq(2)),
                )
                .unwrap();

            let listed = store.list_names(test_ns(), None).unwrap();
            let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, vec!["a", "b", "c"], "sorted, no duplicates");
            assert_eq!(listed[0].1, pending_a, "pending bind wins on 'a'");
            assert_ne!(listed[0].1, durable_a);
            assert_eq!(listed[2].1, pending_c);

            // Prefix filtering applies to the overlay too.
            let prefixed = store.list_names(test_ns(), Some("c")).unwrap();
            assert_eq!(prefixed, vec![("c".to_owned(), pending_c)]);
        }

        /// Regression (#226 review, fix shape 3): a Content-Type set
        /// on a create that overflowed to the durable write-through
        /// path (no overlay row) lands durably via `set_content_type`'s
        /// storage arm, and the hydrator's later `drain_pending` for
        /// that id (a no-op — no overlay row) must not disturb it.
        #[test]
        fn content_type_on_overflowed_row_writes_through_and_survives_drain() {
            let store = CompositionStore::new().with_pending_max(0);
            store.add_namespace(make_ns(10, test_tenant(), test_shard()));

            // pending_max = 0 → every volatile create overflows.
            let id = store.create_volatile(test_ns(), vec![], 0).unwrap();
            assert_eq!(store.pending_overflows(), 1);
            assert_eq!(store.pending_len(), 0);

            store
                .set_content_type(id, Some("text/plain".to_owned()))
                .unwrap();
            // Durable immediately (the row went write-through).
            assert_eq!(
                store
                    .with_storage_locked(|s| s.get(id).unwrap())
                    .expect("write-through row is durable")
                    .content_type
                    .as_deref(),
                Some("text/plain"),
            );

            // The hydrator still calls drain_pending for every
            // touched id when the Create delta applies.
            store.drain_pending(id);
            assert_eq!(
                store.get(id).unwrap().content_type.as_deref(),
                Some("text/plain"),
                "drain of an overflowed (no-overlay) row disturbed its Content-Type",
            );
        }

        /// Storage wrapper counting reads + writes — pins the
        /// drain-time merge's storage-I/O contract: zero I/O on the
        /// common no-Content-Type drain, exactly one get + one put on
        /// a merge, get-only when the durable row already carries the
        /// identical value.
        struct CountingStorage {
            inner: crate::persistent::MemoryStorage,
            gets: Arc<std::sync::atomic::AtomicU64>,
            puts: Arc<std::sync::atomic::AtomicU64>,
        }

        impl CountingStorage {
            fn new() -> (
                Self,
                Arc<std::sync::atomic::AtomicU64>,
                Arc<std::sync::atomic::AtomicU64>,
            ) {
                let gets = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let puts = Arc::new(std::sync::atomic::AtomicU64::new(0));
                (
                    Self {
                        inner: crate::persistent::MemoryStorage::new(),
                        gets: Arc::clone(&gets),
                        puts: Arc::clone(&puts),
                    },
                    gets,
                    puts,
                )
            }
        }

        impl crate::persistent::CompositionStorage for CountingStorage {
            fn get(
                &self,
                id: CompositionId,
            ) -> Result<Option<Composition>, crate::persistent::PersistentStoreError> {
                self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.get(id)
            }
            fn count(&self) -> Result<u64, crate::persistent::PersistentStoreError> {
                self.inner.count()
            }
            fn list_in_namespace(
                &self,
                ns: NamespaceId,
            ) -> Result<Vec<Composition>, crate::persistent::PersistentStoreError> {
                self.inner.list_in_namespace(ns)
            }
            fn put(
                &self,
                comp: Composition,
            ) -> Result<(), crate::persistent::PersistentStoreError> {
                self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.put(comp)
            }
            fn remove(
                &self,
                id: CompositionId,
            ) -> Result<bool, crate::persistent::PersistentStoreError> {
                self.inner.remove(id)
            }
            fn name_lookup(
                &self,
                ns: NamespaceId,
                name: &str,
            ) -> Result<Option<CompositionId>, crate::persistent::PersistentStoreError>
            {
                self.inner.name_lookup(ns, name)
            }
            fn name_for(
                &self,
                id: CompositionId,
            ) -> Result<Option<(NamespaceId, String)>, crate::persistent::PersistentStoreError>
            {
                self.inner.name_for(id)
            }
            fn name_insert(
                &self,
                ns: NamespaceId,
                name: String,
                id: CompositionId,
            ) -> Result<(), crate::persistent::PersistentStoreError> {
                self.inner.name_insert(ns, name, id)
            }
            fn name_seq_lookup(
                &self,
                ns: NamespaceId,
                name: &str,
            ) -> Result<Option<PerspectiveSeq>, crate::persistent::PersistentStoreError>
            {
                self.inner.name_seq_lookup(ns, name)
            }
            fn name_seq_record(
                &self,
                ns: NamespaceId,
                name: String,
                seq: Option<PerspectiveSeq>,
            ) -> Result<(), crate::persistent::PersistentStoreError> {
                self.inner.name_seq_record(ns, name, seq)
            }
            fn name_remove(
                &self,
                ns: NamespaceId,
                name: &str,
            ) -> Result<bool, crate::persistent::PersistentStoreError> {
                self.inner.name_remove(ns, name)
            }
            fn name_list(
                &self,
                ns: NamespaceId,
                prefix: Option<&str>,
            ) -> Result<Vec<(String, CompositionId)>, crate::persistent::PersistentStoreError>
            {
                self.inner.name_list(ns, prefix)
            }
            fn last_applied_seq(
                &self,
                shard_id: ShardId,
            ) -> Result<kiseki_common::ids::SequenceNumber, crate::persistent::PersistentStoreError>
            {
                self.inner.last_applied_seq(shard_id)
            }
            fn stuck_state(
                &self,
                shard_id: ShardId,
            ) -> Result<
                Option<(kiseki_common::ids::SequenceNumber, u32)>,
                crate::persistent::PersistentStoreError,
            > {
                self.inner.stuck_state(shard_id)
            }
            fn halted(
                &self,
                shard_id: ShardId,
            ) -> Result<bool, crate::persistent::PersistentStoreError> {
                self.inner.halted(shard_id)
            }
            fn halted_any(&self) -> Result<bool, crate::persistent::PersistentStoreError> {
                self.inner.halted_any()
            }
            fn apply_hydration_batch(
                &self,
                batch: crate::persistent::HydrationBatch,
            ) -> Result<(), crate::persistent::PersistentStoreError> {
                self.inner.apply_hydration_batch(batch)
            }
        }

        fn counting_store() -> (
            CompositionStore,
            Arc<std::sync::atomic::AtomicU64>,
            Arc<std::sync::atomic::AtomicU64>,
        ) {
            let (storage, gets, puts) = CountingStorage::new();
            let store = CompositionStore::with_storage(Box::new(storage));
            store.add_namespace(make_ns(10, test_tenant(), test_shard()));
            (store, gets, puts)
        }

        /// The common drain (no Content-Type ever set) must do ZERO
        /// storage I/O — the merge fast path returns before touching
        /// the backend.
        #[test]
        fn drain_without_content_type_does_no_storage_io() {
            let (store, gets, puts) = counting_store();
            let id = store.create_volatile(test_ns(), vec![], 0).unwrap();

            let (g0, p0) = (
                gets.load(std::sync::atomic::Ordering::Relaxed),
                puts.load(std::sync::atomic::Ordering::Relaxed),
            );
            store.drain_pending(id);
            assert_eq!(store.pending_len(), 0, "drain must drop the overlay row");
            assert_eq!(
                (
                    gets.load(std::sync::atomic::Ordering::Relaxed),
                    puts.load(std::sync::atomic::Ordering::Relaxed),
                ),
                (g0, p0),
                "no-Content-Type drain must not touch storage",
            );
        }

        /// A drain that merges does exactly one storage read + one
        /// storage write; a drain whose durable row already carries
        /// the identical Content-Type reads but does not write.
        #[test]
        fn drain_merge_storage_io_is_one_get_one_put() {
            let (store, gets, puts) = counting_store();
            let id = store.create_volatile(test_ns(), vec![], 0).unwrap();
            store
                .set_content_type(id, Some("application/json".to_owned()))
                .unwrap();
            // Materialize the durable row the way stage_create does:
            // raw storage put with content_type: None.
            let mut durable = store.get(id).unwrap();
            durable.content_type = None;
            store.with_storage_locked(|s| s.put(durable)).unwrap();

            let (g0, p0) = (
                gets.load(std::sync::atomic::Ordering::Relaxed),
                puts.load(std::sync::atomic::Ordering::Relaxed),
            );
            store.drain_pending(id);
            assert_eq!(
                (
                    gets.load(std::sync::atomic::Ordering::Relaxed),
                    puts.load(std::sync::atomic::Ordering::Relaxed),
                ),
                (g0 + 1, p0 + 1),
                "merging drain must do exactly one get + one put",
            );
            assert_eq!(
                store.get(id).unwrap().content_type.as_deref(),
                Some("application/json"),
            );

            // Identical-value case: durable row already carries the
            // CT (defensive replay shape) — read, but no write.
            let id2 = store.create_volatile(test_ns(), vec![], 0).unwrap();
            store
                .set_content_type(id2, Some("text/csv".to_owned()))
                .unwrap();
            let durable2 = store.get(id2).unwrap(); // CT already set
            store.with_storage_locked(|s| s.put(durable2)).unwrap();
            let (g1, p1) = (
                gets.load(std::sync::atomic::Ordering::Relaxed),
                puts.load(std::sync::atomic::Ordering::Relaxed),
            );
            store.drain_pending(id2);
            assert_eq!(
                (
                    gets.load(std::sync::atomic::Ordering::Relaxed),
                    puts.load(std::sync::atomic::Ordering::Relaxed),
                ),
                (g1 + 1, p1),
                "identical-value drain must read once and skip the write",
            );
        }
    }
}
