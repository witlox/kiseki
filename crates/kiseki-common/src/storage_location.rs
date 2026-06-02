//! Storage-location tag carried per-chunk on every composition.
//!
//! ADR-048 amendment to the chunk-ref model: every chunk is either in
//! the **hot** tier (a named chunk-fabric pool) or **cold** (migrated
//! into a slab). The tag lives here in `kiseki-common` so both
//! `kiseki-composition` (which persists the tag) and `kiseki-chunk` /
//! `kiseki-chunk-cluster` (which act on it) can refer to one shared
//! type instead of crate-local mirrors.
//!
//! Wire encoding for the tag lives in the composition crate alongside
//! the rest of the composition delta payload — this module just
//! defines the data shape.

/// Unique identifier for a slab. UUIDv4-based so the slab placement
/// rendezvous hash (`pick_placement`) hits the same device set
/// deterministically across nodes (I-SE5).
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct SlabId(pub uuid::Uuid);

impl SlabId {
    /// Mint a fresh random slab id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// Nil id — used as a sentinel during slab construction before
    /// the encoder mints a real id, and on test-only "empty slab"
    /// fixtures.
    #[must_use]
    pub const fn nil() -> Self {
        Self(uuid::Uuid::nil())
    }
}

impl Default for SlabId {
    fn default() -> Self {
        Self::nil()
    }
}

/// Per-chunk location tag. The composition's `chunk_locations[i]`
/// describes where `chunks[i]` actually lives.
///
/// Reads branch on this tag (ADR-048 §"Read path"); writes always
/// land Hot and the compactor flips the location to Cold after a slab
/// is durable (I-SE1).
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ChunkRefLocation {
    /// Chunk lives in the hot tier (existing chunk fabric). Reads
    /// take the existing `read_chunk(pool_name)` path.
    Hot {
        /// Pool name within the chunk fabric.
        pool_name: String,
    },
    /// Chunk has been migrated into a cold-tier slab. Reads
    /// reconstruct the slab from EC fragments, then extract the
    /// chunk's bytes from the slab data buffer at
    /// `offset_in_slab..offset_in_slab+length`.
    Cold {
        /// Pool name the slab lives in (used by placement +
        /// per-pool capacity accounting).
        pool_name: String,
        /// Owning slab.
        slab_id: SlabId,
        /// Byte offset inside the slab data buffer.
        offset_in_slab: u64,
        /// Length of the chunk in bytes — duplicates the slab's
        /// extent table but lets the read path bound the reconstruct
        /// I/O without first decoding the header.
        length: u32,
    },
}

impl ChunkRefLocation {
    /// Pool name the chunk lives in — same shape for Hot and Cold,
    /// lets the caller drive per-pool capacity / GC logic without
    /// matching each time.
    #[must_use]
    pub fn pool_name(&self) -> &str {
        match self {
            Self::Hot { pool_name } | Self::Cold { pool_name, .. } => pool_name,
        }
    }

    /// `true` when the chunk has been migrated to cold tier.
    #[must_use]
    pub const fn is_cold(&self) -> bool {
        matches!(self, Self::Cold { .. })
    }
}

impl Default for ChunkRefLocation {
    fn default() -> Self {
        Self::Hot {
            pool_name: String::new(),
        }
    }
}
