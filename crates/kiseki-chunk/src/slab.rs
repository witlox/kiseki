//! Slab-EC compactor — ADR-048 kernel.
//!
//! Implements the slab abstraction at the heart of ADR-048's "hot
//! tier replicated / cold tier EC" split. A *slab* packs many small
//! chunks into a single fixed-byte unit, then erasure-codes the unit
//! once — amortising the per-PUT EC fan-out cost (N+K RPCs) across
//! many writes.
//!
//! This module supplies the deterministic, pure pieces:
//!
//! - [`SlabId`], [`SlabHeader`], [`SlabExtent`], [`Slab`] data types.
//! - [`ChunkRefLocation`] — Hot vs Cold tag carried on each composition
//!   `chunk_ref`, replacing the today-implicit "every chunk lives in
//!   the chunk fabric" assumption.
//! - [`encode_slab`] — pack chunks → slab data buffer → EC fragments.
//!   Re-uses the existing [`crate::ec`] Reed-Solomon encoder so the
//!   slab parity math is identical to per-PUT EC; a slab is "just a
//!   bigger stripe".
//! - [`extract_chunk`] — recover a single chunk's bytes from a
//!   reconstructed slab buffer using the slab's extent table.
//! - [`SlabStore`] trait + [`InMemorySlabStore`] — the storage seam
//!   the compactor and gateway read path hit; the in-memory impl is
//!   sufficient for unit/integration tests, the production placement
//!   path will land alongside the runtime compactor task (ADR-048
//!   phase 5.3 follow-up).
//! - [`SlabBuilder`] — accumulator the compactor task uses to assemble
//!   a candidate slab under the configured byte / count / age caps.
//! - Backpressure helpers for ADR-048 §"Backpressure" /
//!   `I-SE6` (`SlabBacklog::record` + `is_over_threshold`).
//!
//! The compactor task scheduler + the Raft delta that flips a
//! composition's `chunk_ref.location` from Hot to Cold + the
//! gateway's cold-path read branch land in the runtime/log/gateway
//! crates as the wiring work for this kernel; see ADR-048 §"Phasing".

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::ec::{self, EcEncoded};
use crate::error::ChunkError;
use crate::pool::DEFAULT_REPLICATION_CEILING_BYTES;
use kiseki_common::ids::ChunkId;
pub use kiseki_common::storage_location::{ChunkRefLocation, SlabId};

/// Default per-slab byte budget — ADR-048 §"Compactor task" 64 MiB.
/// Picked to amortise EC fan-out across many chunks while staying
/// small enough that the slab fits in RAM for both encode and
/// reconstruction.
pub const DEFAULT_SLAB_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Default max chunk count per slab — ADR-048 §"Compactor task" 1024.
/// Prevents pathological packing (e.g. many tiny chunks → giant
/// extent table) without changing the byte-budget cap.
pub const DEFAULT_SLAB_MAX_CHUNKS: usize = 1024;

/// Default candidate-age timeout — ADR-048 §"Compactor task" 30 s.
/// Bounds how long a chunk lingers in the hot tier before getting
/// flushed into a slab; tunable via `KISEKI_SLAB_AGE_TIMEOUT_MS`.
pub const DEFAULT_SLAB_AGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Slab format version — bumped on any incompatible on-disk layout
/// change. The compactor refuses to read slabs whose `version > MAX`,
/// so a downgraded binary fails fast instead of returning corrupt
/// bytes (ADR-024 amendment §"durability" defense-in-depth).
pub const SLAB_FORMAT_VERSION: u16 = 1;

/// Fixed-size slab header serialised at the start of every slab.
/// Layout (32 B):
///   `[0..2)`   version u16 LE
///   `[2..4)`   data shards u16 LE
///   `[4..6)`   parity shards u16 LE
///   `[6..8)`   reserved u16 (must be 0)
///   `[8..16)`  `byte_size` u64 LE (sum of `extent.length` across the slab)
///   `[16..24)` `extent_count` u64 LE (number of [`SlabExtent`] entries)
///   `[24..32)` `original_len` u64 LE (slab data buffer length BEFORE EC pad)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlabHeader {
    /// Slab on-disk format version.
    pub version: u16,
    /// EC data-shard count for the slab's fragment encoding.
    pub data_shards: u16,
    /// EC parity-shard count for the slab's fragment encoding.
    pub parity_shards: u16,
    /// Sum of `extent.length` across the slab's extent table. Used
    /// for capacity accounting and read-path sanity checks.
    pub byte_size: u64,
    /// Number of [`SlabExtent`] entries.
    pub extent_count: u64,
    /// Length of the slab data buffer before EC padding. The read
    /// path passes this through to [`ec::decode`] as `original_len`.
    pub original_len: u64,
}

/// Wire size of [`SlabHeader`] on disk.
pub const SLAB_HEADER_LEN: usize = 32;

/// One extent in a slab's extent table — points at a chunk's bytes
/// inside the reconstructed slab data buffer.
///
/// Layout (48 B):
///   `[0..32)`  `chunk_id` (32 B, content-addressed)
///   `[32..40)` offset u64 LE
///   `[40..44)` length u32 LE
///   `[44..48)` refcount u32 LE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlabExtent {
    /// Chunk identifier this extent represents.
    pub chunk_id: ChunkId,
    /// Byte offset of the chunk inside the slab data buffer.
    pub offset: u64,
    /// Length of the chunk in bytes.
    pub length: u32,
    /// Reference count — when this drops to zero the extent is
    /// eligible for slab GC. ADR-048 §"Slab GC" + I-SE3.
    pub refcount: u32,
}

/// Wire size of [`SlabExtent`] on disk.
pub const SLAB_EXTENT_LEN: usize = 48;

/// A fully decoded slab — header + extent table + concatenated data
/// buffer. The compactor builds this; the read path reconstructs it
/// from EC fragments (existing [`ec::decode`] path).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Slab {
    /// Slab identifier.
    pub id: SlabId,
    /// Fixed header.
    pub header: SlabHeader,
    /// Extent table sorted by `offset` (ADR-048 §"Slab structure":
    /// "sorted by `chunk_id`" — we use `offset` because that is the
    /// natural pack order and gives an O(log N) chunk lookup by id
    /// once the table is rebuilt into an index).
    pub extents: Vec<SlabExtent>,
    /// Concatenated chunk bytes; sum of `extent.length` equals
    /// `header.byte_size`.
    pub data: Vec<u8>,
}

impl Slab {
    /// Look up an extent by chunk id. Returns `None` if the chunk is
    /// not in this slab (a slab that was rewritten by the maintenance
    /// pass will drop stale chunks).
    #[must_use]
    pub fn extent_for(&self, chunk_id: ChunkId) -> Option<&SlabExtent> {
        self.extents.iter().find(|e| e.chunk_id == chunk_id)
    }

    /// Sum of live (refcount > 0) extent bytes. Drives the slab
    /// fragmentation metric the maintenance pass uses to decide
    /// whether to rewrite the slab. ADR-048 §"Slab GC".
    #[must_use]
    pub fn live_byte_size(&self) -> u64 {
        self.extents
            .iter()
            .filter(|e| e.refcount > 0)
            .map(|e| u64::from(e.length))
            .sum()
    }

    /// Fraction of slab `byte_size` that is unreferenced (`refcount=0`).
    /// Returns a value in `[0.0, 1.0]`. The maintenance pass rewrites
    /// the slab when this exceeds the configured threshold
    /// (default `0.5`, ADR-048 §"Slab GC" "f4 rebalance").
    #[must_use]
    pub fn fragmentation_ratio(&self) -> f64 {
        if self.header.byte_size == 0 {
            return 0.0;
        }
        // total - live = dead bytes. Cast through `i64` -> `f64` —
        // both u64 values fit comfortably in `i64` range for any
        // sensible slab (~64 MiB; clippy's cast_precision_loss
        // warning is about the byte-count saturating f64 mantissa
        // at 2^53, which is ~9 PiB and well beyond a single slab).
        #[allow(clippy::cast_precision_loss)]
        let r = {
            let dead = self.header.byte_size - self.live_byte_size();
            dead as f64 / self.header.byte_size as f64
        };
        r
    }

    /// True when every extent has refcount = 0. The slab is eligible
    /// for whole-slab GC; the placement nodes can drop the fragments
    /// outright. I-SE3.
    #[must_use]
    pub fn fully_unreferenced(&self) -> bool {
        self.extents.iter().all(|e| e.refcount == 0)
    }
}

// ChunkRefLocation + SlabId live in `kiseki_common::storage_location`
// (re-exported above) so kiseki-composition can persist the tag without
// taking a dep on kiseki-chunk. ADR-048 §"ChunkRef extends".

/// Build a slab from a set of `(chunk_id, bytes)` pairs and EC-encode it.
///
/// The packing rule is simple: concatenate `chunks` in the order given
/// into the slab data buffer, building the extent table as we go. The
/// compactor is responsible for sorting/grouping chunks before calling
/// this — the encoder doesn't re-order so the offsets remain stable
/// for the duration of the slab's lifetime (ADR-048 §"Slab structure"
/// extent-table integrity).
///
/// The EC parity is computed over `data + extent_table` so a corrupted
/// extent table can be reconstructed if the parity fragments survive.
/// (Tier-1 design uses just `data`; the extent table is tiny relative
/// to the slab payload, and we accept the cost for the simpler
/// recovery story.)
///
/// # Errors
/// - [`ChunkError::EcInvalidConfig`] if `data_shards == 0` or
///   `parity_shards == 0`, or the underlying RS encoder fails.
/// - [`ChunkError::EcEncodeFailed`] if RS encoding fails for any
///   other reason.
pub fn encode_slab(
    chunks: &[(ChunkId, Vec<u8>)],
    data_shards: u16,
    parity_shards: u16,
) -> Result<(Slab, EcEncoded), ChunkError> {
    if data_shards == 0 || parity_shards == 0 {
        return Err(ChunkError::EcInvalidConfig);
    }
    let slab_id = SlabId::new();

    // Build the data buffer + extent table in one pass.
    let total_bytes: u64 = chunks.iter().map(|(_, b)| b.len() as u64).sum();
    let mut data = Vec::with_capacity(usize::try_from(total_bytes).unwrap_or(usize::MAX));
    let mut extents: Vec<SlabExtent> = Vec::with_capacity(chunks.len());
    let mut offset: u64 = 0;
    for (cid, bytes) in chunks {
        let length = u32::try_from(bytes.len()).map_err(|_| ChunkError::EcInvalidConfig)?;
        extents.push(SlabExtent {
            chunk_id: *cid,
            offset,
            length,
            refcount: 1,
        });
        data.extend_from_slice(bytes);
        offset += u64::from(length);
    }
    let header = SlabHeader {
        version: SLAB_FORMAT_VERSION,
        data_shards,
        parity_shards,
        byte_size: total_bytes,
        extent_count: extents.len() as u64,
        original_len: data.len() as u64,
    };

    // EC-encode the data buffer using the existing kernel — slabs are
    // "just a bigger stripe", so the math is identical to per-PUT EC.
    let encoded = ec::encode(&data, usize::from(data_shards), usize::from(parity_shards))?;
    Ok((
        Slab {
            id: slab_id,
            header,
            extents,
            data,
        },
        encoded,
    ))
}

/// Extract a single chunk's bytes from a reconstructed slab data
/// buffer. Used by the read path after EC reconstruction of the slab
/// fragments.
///
/// # Errors
/// Returns [`ChunkError::ChunkLost`] when the requested range
/// exceeds the slab data buffer — the slab is corrupt or the extent
/// metadata was tampered with.
pub fn extract_chunk(
    slab_data: &[u8],
    offset_in_slab: u64,
    length: u32,
) -> Result<&[u8], ChunkError> {
    let start = usize::try_from(offset_in_slab).map_err(|_| ChunkError::ChunkLost)?;
    let len = length as usize;
    let end = start.checked_add(len).ok_or(ChunkError::ChunkLost)?;
    if end > slab_data.len() {
        return Err(ChunkError::ChunkLost);
    }
    Ok(&slab_data[start..end])
}

/// Accumulator the compactor task feeds with pending-migration chunks
/// until it hits any of the configured caps (byte budget, max chunks,
/// age timeout). When [`should_flush`] returns true, the caller drains
/// the accumulated chunks into [`encode_slab`].
pub struct SlabBuilder {
    /// Maximum total bytes the builder accepts before forcing a flush.
    byte_budget: u64,
    /// Maximum chunk count.
    max_chunks: usize,
    /// Maximum age of the oldest accumulated chunk.
    age_timeout: Duration,
    /// When the first chunk landed (None before any chunk arrived).
    first_arrived: Option<Instant>,
    /// Accumulated `(chunk_id, bytes)` pairs in arrival order.
    chunks: Vec<(ChunkId, Vec<u8>)>,
    /// Running byte total.
    bytes: u64,
}

impl SlabBuilder {
    /// Build with the ADR-048 defaults
    /// (64 MiB / 1024 chunks / 30 s age).
    #[must_use]
    pub fn new() -> Self {
        Self::with_caps(
            DEFAULT_SLAB_BYTE_BUDGET,
            DEFAULT_SLAB_MAX_CHUNKS,
            DEFAULT_SLAB_AGE_TIMEOUT,
        )
    }

    /// Build with explicit caps. Used by the compactor when its
    /// per-pool config overrides the defaults
    /// (e.g. small-object-heavy pool wants 16 MiB slabs for shorter
    /// extract-chunk read latency).
    #[must_use]
    pub fn with_caps(byte_budget: u64, max_chunks: usize, age_timeout: Duration) -> Self {
        Self {
            byte_budget,
            max_chunks,
            age_timeout,
            first_arrived: None,
            chunks: Vec::new(),
            bytes: 0,
        }
    }

    /// Push a chunk into the candidate slab. The chunk lands even if
    /// it pushes the builder over a cap — [`should_flush`] will
    /// return true after, and the caller drains. (Refusing the
    /// addition would force the compactor to size-check before every
    /// push, with no real protection benefit; the budget is a soft
    /// flush trigger, not a hard cap.)
    pub fn push(&mut self, now: Instant, chunk_id: ChunkId, bytes: Vec<u8>) {
        if self.first_arrived.is_none() {
            self.first_arrived = Some(now);
        }
        self.bytes += bytes.len() as u64;
        self.chunks.push((chunk_id, bytes));
    }

    /// `true` when any of the configured caps fires for this `now`.
    /// Caller drains via [`drain`] and starts a fresh slab.
    #[must_use]
    pub fn should_flush(&self, now: Instant) -> bool {
        if self.chunks.is_empty() {
            return false;
        }
        if self.bytes >= self.byte_budget {
            return true;
        }
        if self.chunks.len() >= self.max_chunks {
            return true;
        }
        match self.first_arrived {
            Some(t) => now.duration_since(t) >= self.age_timeout,
            None => false,
        }
    }

    /// Take ownership of the accumulated chunks and reset the
    /// builder. Returns an empty vec when no chunks have been pushed.
    pub fn drain(&mut self) -> Vec<(ChunkId, Vec<u8>)> {
        self.first_arrived = None;
        self.bytes = 0;
        std::mem::take(&mut self.chunks)
    }

    /// Current accumulated bytes — surfaced for the compactor's
    /// backpressure metric.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Current accumulated chunk count.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

impl Default for SlabBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-pool slab-backlog tracker — ADR-048 §"Backpressure" + I-SE6.
/// The compactor records the **age of the oldest pending-migration
/// chunk** every tick; the gateway reads `is_over_threshold` before
/// declaring `WriteSurface::async_ack_eligible = true`.
pub struct SlabBacklog {
    /// Time the oldest pending chunk landed. `None` when the
    /// migration queue is empty.
    oldest_pending: Option<Instant>,
    /// Soft threshold; pool emits the backlog metric above this.
    soft_threshold: Duration,
    /// Hard threshold; pool gates async-ack above this.
    hard_threshold: Duration,
}

impl SlabBacklog {
    /// Build with the ADR-048 §"Backpressure" defaults
    /// (60 s hard threshold, 30 s soft).
    #[must_use]
    pub fn new() -> Self {
        Self::with_thresholds(Duration::from_secs(30), Duration::from_secs(60))
    }

    /// Build with explicit thresholds. The compactor task reads
    /// `KISEKI_COMPACTOR_BACKLOG_SOFT_S` / `_HARD_S` to override.
    #[must_use]
    pub fn with_thresholds(soft: Duration, hard: Duration) -> Self {
        Self {
            oldest_pending: None,
            soft_threshold: soft,
            hard_threshold: hard,
        }
    }

    /// Record that a chunk arrived in the migration queue at `at`.
    /// Updates the "oldest pending" tracker only when this is the
    /// first chunk after a drain.
    pub fn record(&mut self, at: Instant) {
        if self.oldest_pending.is_none() {
            self.oldest_pending = Some(at);
        }
    }

    /// Drain the queue — reset the oldest-pending tracker. Called by
    /// the compactor after a successful slab flush.
    pub fn drain(&mut self) {
        self.oldest_pending = None;
    }

    /// Backlog age right now. Returns `Duration::ZERO` when the queue
    /// is empty.
    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        self.oldest_pending
            .map_or(Duration::ZERO, |t| now.duration_since(t))
    }

    /// `true` when the backlog age has crossed `hard_threshold`. The
    /// gateway uses this to gate async-ack-eligible writes per-pool.
    #[must_use]
    pub fn is_over_threshold(&self, now: Instant) -> bool {
        self.age(now) >= self.hard_threshold
    }

    /// `true` when the backlog age has crossed `soft_threshold`. The
    /// runtime emits an audit-tier warning when this is true but not
    /// yet over hard.
    #[must_use]
    pub fn is_over_soft(&self, now: Instant) -> bool {
        let a = self.age(now);
        a >= self.soft_threshold && a < self.hard_threshold
    }
}

impl Default for SlabBacklog {
    fn default() -> Self {
        Self::new()
    }
}

/// Storage seam for slab persistence — the compactor PUTs slabs here
/// after EC encoding; the gateway GETs them through the read path. A
/// pool whose `requires_migration == true` gets a `SlabStore` wired by
/// the runtime; pools without one stay R-3 / EC-per-PUT (the legacy
/// path).
///
/// Implementations:
///   - [`InMemorySlabStore`] — for tests and the single-node dev box.
///   - The production placement-aware store lands as ADR-048
///     phase 5.3 follow-up (fans fragments to placement nodes via the
///     existing chunk fabric).
pub trait SlabStore: Send + Sync {
    /// Persist a freshly-encoded slab. The store is responsible for
    /// fanning fragments to placement nodes; this trait method
    /// returns only after quorum-durable (so the caller can flip
    /// `ChunkRefLocation` from Hot to Cold per I-SE1).
    fn put_slab(&self, slab: Slab, encoded: EcEncoded) -> Result<(), ChunkError>;

    /// Fetch a slab by id, reconstructing it from EC fragments if
    /// necessary. Returns `Err(ChunkError::SlabNotFound(slab_id.0))` (re-used
    /// for slabs since they share the chunk-id UUID space) when the
    /// slab isn't present.
    fn get_slab(&self, slab_id: SlabId) -> Result<Slab, ChunkError>;

    /// Decrement an extent's refcount. When it reaches zero, the
    /// extent is eligible for slab GC; when every extent in the slab
    /// has refcount zero, the store may delete the slab entirely
    /// (I-SE3).
    fn decrement_refcount(&self, slab_id: SlabId, chunk_id: ChunkId) -> Result<(), ChunkError>;

    /// Garbage-collect a slab whose extents are all unreferenced.
    /// Idempotent: a no-op when the slab isn't present or still has
    /// referenced extents.
    fn gc_slab(&self, slab_id: SlabId) -> Result<(), ChunkError>;
}

/// In-process slab store — backed by a `BTreeMap<SlabId, Slab>`. Used
/// by unit tests, the BDD harness, and the single-node dev box (where
/// "fan to placement nodes" degenerates to "write to local map"). The
/// production placement-aware store lives elsewhere (ADR-048 phase
/// 5.3 follow-up).
pub struct InMemorySlabStore {
    inner: Arc<Mutex<BTreeMap<SlabId, Slab>>>,
}

impl InMemorySlabStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Snapshot every slab id known locally. Used by tests to drive
    /// the GC pass without needing the runtime task scheduler.
    #[must_use]
    pub fn slab_ids(&self) -> Vec<SlabId> {
        self.inner.lock().keys().copied().collect()
    }
}

impl Default for InMemorySlabStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SlabStore for InMemorySlabStore {
    fn put_slab(&self, slab: Slab, _encoded: EcEncoded) -> Result<(), ChunkError> {
        self.inner.lock().insert(slab.id, slab);
        Ok(())
    }

    fn get_slab(&self, slab_id: SlabId) -> Result<Slab, ChunkError> {
        self.inner
            .lock()
            .get(&slab_id)
            .cloned()
            .ok_or(ChunkError::SlabNotFound(slab_id.0))
    }

    fn decrement_refcount(&self, slab_id: SlabId, chunk_id: ChunkId) -> Result<(), ChunkError> {
        let mut guard = self.inner.lock();
        let Some(slab) = guard.get_mut(&slab_id) else {
            return Err(ChunkError::SlabNotFound(slab_id.0));
        };
        let Some(extent) = slab.extents.iter_mut().find(|e| e.chunk_id == chunk_id) else {
            return Err(ChunkError::SlabNotFound(slab_id.0));
        };
        extent.refcount = extent.refcount.saturating_sub(1);
        Ok(())
    }

    fn gc_slab(&self, slab_id: SlabId) -> Result<(), ChunkError> {
        let mut guard = self.inner.lock();
        let drop = guard
            .get(&slab_id)
            .is_some_and(super::slab::Slab::fully_unreferenced);
        if drop {
            guard.remove(&slab_id);
        }
        Ok(())
    }
}

/// Convenience: the default replication-ceiling band threshold a pool
/// uses to decide whether a chunk is slab-eligible. A chunk smaller
/// than this falls in the replicated band and rides the hot tier
/// (where the slab compactor picks it up); a chunk larger than this
/// is already EC per-PUT and is *not* slab-eligible (ADR-048
/// §"Decision" "chunks that originated directly in an EC pool ...
/// land EC-encoded (unchanged)").
#[must_use]
pub const fn slab_eligibility_ceiling() -> u64 {
    DEFAULT_REPLICATION_CEILING_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::ChunkId;

    fn cid(n: u128) -> ChunkId {
        // ChunkId is content-addressed [u8; 32]; pack the test seed
        // into the low 16 bytes so each `cid(n)` is distinct.
        let mut buf = [0u8; 32];
        buf[16..32].copy_from_slice(&n.to_le_bytes());
        ChunkId(buf)
    }

    #[test]
    fn encode_then_extract_round_trips_each_chunk() {
        let chunks = vec![
            (cid(1), b"alpha".to_vec()),
            (cid(2), b"betagamma".to_vec()),
            (cid(3), b"deltaepsilonzeta".to_vec()),
        ];
        let (slab, _encoded) = encode_slab(&chunks, 4, 2).expect("encode");
        assert_eq!(slab.header.byte_size, 5 + 9 + 16);
        for (cid_in, bytes) in &chunks {
            let extent = slab.extent_for(*cid_in).expect("extent present");
            let extracted =
                extract_chunk(&slab.data, extent.offset, extent.length).expect("extract");
            assert_eq!(extracted, bytes.as_slice());
        }
    }

    #[test]
    fn extract_chunk_rejects_out_of_range() {
        let buf = vec![0u8; 16];
        assert!(extract_chunk(&buf, 0, 16).is_ok());
        assert!(extract_chunk(&buf, 0, 17).is_err());
        assert!(extract_chunk(&buf, 16, 1).is_err());
        assert!(extract_chunk(&buf, u64::MAX, 1).is_err());
    }

    #[test]
    fn builder_flushes_at_byte_budget() {
        let mut b = SlabBuilder::with_caps(100, 1_000, Duration::from_secs(60));
        let now = Instant::now();
        b.push(now, cid(1), vec![0u8; 60]);
        assert!(!b.should_flush(now));
        b.push(now, cid(2), vec![0u8; 60]);
        assert!(b.should_flush(now));
        let drained = b.drain();
        assert_eq!(drained.len(), 2);
        // Drain resets the builder.
        assert_eq!(b.bytes(), 0);
        assert_eq!(b.chunk_count(), 0);
        assert!(!b.should_flush(now));
    }

    #[test]
    fn builder_flushes_at_max_chunks() {
        let mut b = SlabBuilder::with_caps(1_000_000, 3, Duration::from_secs(60));
        let now = Instant::now();
        b.push(now, cid(1), vec![0u8; 1]);
        b.push(now, cid(2), vec![0u8; 1]);
        assert!(!b.should_flush(now));
        b.push(now, cid(3), vec![0u8; 1]);
        assert!(b.should_flush(now));
    }

    #[test]
    fn builder_flushes_at_age_timeout() {
        let mut b = SlabBuilder::with_caps(1_000_000, 1_000, Duration::from_millis(100));
        let t0 = Instant::now();
        b.push(t0, cid(1), vec![0u8; 1]);
        assert!(!b.should_flush(t0));
        let t1 = t0 + Duration::from_millis(101);
        assert!(b.should_flush(t1));
    }

    #[test]
    fn backlog_records_oldest_pending_age() {
        let mut bp =
            SlabBacklog::with_thresholds(Duration::from_millis(50), Duration::from_millis(100));
        let t0 = Instant::now();
        assert_eq!(bp.age(t0), Duration::ZERO);
        bp.record(t0);
        // Second record doesn't move the oldest.
        bp.record(t0 + Duration::from_millis(20));
        let t1 = t0 + Duration::from_millis(60);
        assert!(bp.is_over_soft(t1));
        assert!(!bp.is_over_threshold(t1));
        let t2 = t0 + Duration::from_millis(110);
        assert!(bp.is_over_threshold(t2));
        bp.drain();
        assert_eq!(bp.age(t2), Duration::ZERO);
    }

    #[test]
    fn fragmentation_ratio_drives_rewrite_decision() {
        let chunks = vec![
            (cid(1), vec![0u8; 100]),
            (cid(2), vec![0u8; 100]),
            (cid(3), vec![0u8; 100]),
            (cid(4), vec![0u8; 100]),
        ];
        let (mut slab, _) = encode_slab(&chunks, 4, 2).expect("encode");
        assert!((slab.fragmentation_ratio() - 0.0).abs() < f64::EPSILON);
        slab.extents[0].refcount = 0;
        slab.extents[1].refcount = 0;
        // 200 / 400 = 0.5
        assert!((slab.fragmentation_ratio() - 0.5).abs() < f64::EPSILON);
        assert!(!slab.fully_unreferenced());
        for e in &mut slab.extents {
            e.refcount = 0;
        }
        assert!(slab.fully_unreferenced());
    }

    #[test]
    fn in_memory_store_round_trips_and_gcs_unreferenced() {
        let store = InMemorySlabStore::new();
        let chunks = vec![(cid(1), b"hello".to_vec()), (cid(2), b"world".to_vec())];
        let (slab, encoded) = encode_slab(&chunks, 4, 2).expect("encode");
        let sid = slab.id;
        store.put_slab(slab, encoded).expect("put");
        let got = store.get_slab(sid).expect("get");
        assert_eq!(got.id, sid);

        // Decrement both extents → fully unreferenced → gc removes slab.
        store.decrement_refcount(sid, cid(1)).expect("dec1");
        store.decrement_refcount(sid, cid(2)).expect("dec2");
        store.gc_slab(sid).expect("gc");
        assert!(store.get_slab(sid).is_err());
    }

    #[test]
    fn in_memory_store_keeps_partially_referenced_slab() {
        let store = InMemorySlabStore::new();
        let chunks = vec![(cid(1), b"keep".to_vec()), (cid(2), b"drop".to_vec())];
        let (slab, encoded) = encode_slab(&chunks, 4, 2).expect("encode");
        let sid = slab.id;
        store.put_slab(slab, encoded).expect("put");
        store.decrement_refcount(sid, cid(2)).expect("dec");
        // GC is a no-op when any extent still has refcount > 0.
        store.gc_slab(sid).expect("gc");
        assert!(store.get_slab(sid).is_ok());
    }

    #[test]
    fn chunk_ref_location_helpers_branch_on_tag() {
        let hot = ChunkRefLocation::Hot {
            pool_name: "fast".into(),
        };
        assert!(!hot.is_cold());
        assert_eq!(hot.pool_name(), "fast");

        let cold = ChunkRefLocation::Cold {
            pool_name: "cold".into(),
            slab_id: SlabId::new(),
            offset_in_slab: 0,
            length: 16,
        };
        assert!(cold.is_cold());
        assert_eq!(cold.pool_name(), "cold");
    }
}
