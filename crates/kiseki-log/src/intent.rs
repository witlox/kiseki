//! ADR-047 — write intents: the quorum-durable floor + perspective ordering.
//!
//! A [`WriteIntent`] is the durable record fanned to a shard's quorum
//! **before the client is acked** (the universal no-loss floor, I-L2/I-CS1),
//! carrying an ingress-assigned [`PerspectiveSeq`] that fixes the eventual
//! Raft apply order. Decoupling the ack from the Raft consensus round is the
//! ADR-047 write-path relaxation.
//!
//! This module is the **foundational layer**: the perspective sequence, the
//! intent record, the [`IntentStore`] trait, and an in-memory implementation.
//! It is additive and **unwired** — the synchronous write path is unchanged.
//! The durable quorum-replicated store, the majority-watermark async
//! committer, election intent-recovery, the per-surface read path, and the
//! `DecoupledAckEnabled` capability gate land in later build phases (ADR-047
//! "Follow-ups" / issue #140). Gate-1 obligations O1–O4 + resolutions F-1..F-4
//! are tracked there; this layer only owns ordering + idempotent recording.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode};
use kiseki_common::ids::NodeId;
use kiseki_common::locks::LockOrDie;
use kiseki_common::time::HybridLogicalClock;
use kiseki_proto::v1::AppendChunkAndDeltaRequest as ProtoChunkAppendReq;
use prost::Message;

use crate::grpc::{append_chunk_and_delta_request_to_proto, proto_to_append_chunk_and_delta};
use crate::traits::AppendChunkAndDeltaRequest;

/// Errors from a durable [`IntentStore`].
///
/// The in-memory implementation never fails; the trait is fallible so
/// the fjall-backed [`FjallIntentStore`] can surface real I/O and
/// codec faults. Variants carry a `String` rather than the source error
/// so callers outside this crate (and the in-memory tests) need not
/// depend on `fjall` or `prost` to match on the cause.
#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    /// Underlying fjall LSM error (open, get, batch, commit, persist).
    #[error("intent fjall: {0}")]
    Fjall(String),

    /// Encode/decode fault: a truncated value frame, an unparseable
    /// proto, a malformed seq-key, or an unsupported on-disk format
    /// version (the F-4 forward-compat guard).
    #[error("intent codec: {0}")]
    Codec(String),

    /// Election intent-recovery (gate-1 O2) was handed fewer than a
    /// majority of replica intent stores to gather from. Only a majority
    /// gather is guaranteed to overlap every acked intent's `min_acks`
    /// quorum by ≥1, so a sub-majority gather cannot reconstruct the
    /// complete pending set — recovery refuses rather than lose an intent.
    #[error("intent recovery gathered {have} stores, needs a majority of {need}")]
    InsufficientQuorum {
        /// Number of replica intent stores actually gathered.
        have: usize,
        /// Majority threshold required (`cluster_size / 2 + 1`).
        need: usize,
    },

    /// Appending an incorporated intent into the Raft log failed (ADR-047
    /// phase 5a). Carries the underlying append/Raft error rendered to a
    /// `String` so the [`IncorporationSink`](crate::intent_committer::IncorporationSink)
    /// seam need not leak `LogError` into callers that match on `IntentError`.
    #[error("intent incorporation into the log failed: {0}")]
    Incorporate(String),
}

impl From<fjall::Error> for IntentError {
    fn from(e: fjall::Error) -> Self {
        IntentError::Fjall(e.to_string())
    }
}

/// Ingress-assigned total order for a write (ADR-047 §1).
///
/// Wraps the [`HybridLogicalClock`], whose `(physical_ms, logical, node_id)`
/// ordering is a deterministic global total order even when intents are
/// recorded leaderless across different ingress nodes — the basis for
/// last-writer-wins resolution without a synchronous consensus round.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PerspectiveSeq(pub HybridLogicalClock);

/// Client-supplied idempotency key — stable across retries and re-ingress on
/// a different node (ADR-047 §5 / gate-1 O3). The *server* perspective seq is
/// NOT used for dedup because it differs per ingress.
pub type IdempotencyKey = [u8; 16];

/// The quorum-durable intent (ADR-047 §2): the unit fanned to `min_acks`
/// replicas and recorded before ack. Carries the order (`perspective_seq`)
/// and the built append (chunk refs + the metadata delta).
#[derive(Clone, Debug)]
pub struct WriteIntent {
    /// The ingress-assigned order.
    pub perspective_seq: PerspectiveSeq,
    /// Client idempotency key, if supplied. `None` is never deduplicated.
    pub idempotency_key: Option<IdempotencyKey>,
    /// The built append (data references + the metadata delta).
    pub append: AppendChunkAndDeltaRequest,
}

/// Outcome of [`IntentStore::put`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PutOutcome {
    /// Newly recorded at the intent's perspective seq.
    Recorded,
    /// A prior intent with the same idempotency key already exists — this is
    /// a duplicate (retry / re-ingress). Carries the original's seq so the
    /// caller resolves the waiter with the first result, not a second write.
    Duplicate(PerspectiveSeq),
}

/// Per-shard intent store (ADR-047 §2).
///
/// The durable, quorum-replicated implementation lands in a later phase; this
/// trait + [`InMemIntentStore`] are the foundation. `&self` (interior-mutable)
/// so the quorum-write path is not serialized — mirroring the post-#137
/// `ChunkOps` shape.
pub trait IntentStore: Send + Sync {
    /// Record an intent. Idempotent on `idempotency_key`: a duplicate key
    /// returns [`PutOutcome::Duplicate`] with the original seq and does NOT
    /// add a second entry (ADR-047 §5 / O3). A `None` key is never deduped.
    ///
    /// # Errors
    /// Backing-store I/O or codec failure (durable impls only).
    fn put(&self, intent: WriteIntent) -> Result<PutOutcome, IntentError>;

    /// Pending (un-incorporated) intents, ascending by perspective seq — the
    /// order the async committer applies them (ADR-047 §3).
    ///
    /// # Errors
    /// Backing-store I/O or codec failure (durable impls only).
    fn pending(&self) -> Result<Vec<WriteIntent>, IntentError>;

    /// Lowest pending perspective seq — this replica's contribution to the
    /// stability watermark (ADR-047 §3; the watermark advances on a *majority*
    /// low-water-mark per gate-1 F-1). `None` if nothing is pending.
    ///
    /// # Errors
    /// Backing-store I/O or codec failure (durable impls only).
    fn next_pending_seq(&self) -> Result<Option<PerspectiveSeq>, IntentError>;

    /// Drop intents up to and including `up_to` once incorporated into the
    /// Raft log. Pruning is an optimization derivable from the log, never a
    /// correctness dependency (ADR-047 §F-2 — recovery re-derives from the
    /// log's `max_incorporated_seq`).
    ///
    /// # Errors
    /// Backing-store I/O or codec failure (durable impls only).
    fn prune(&self, up_to: PerspectiveSeq) -> Result<(), IntentError>;

    /// Pending count — observability + backpressure (ADR-047 §F-6).
    ///
    /// # Errors
    /// Backing-store I/O or codec failure (durable impls only).
    fn pending_len(&self) -> Result<usize, IntentError>;
}

/// In-memory [`IntentStore`] — the foundational + test implementation.
/// Interior-mutable behind a `Mutex`; the durable store shards and replicates
/// instead.
#[derive(Default)]
pub struct InMemIntentStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_seq: BTreeMap<PerspectiveSeq, WriteIntent>,
    by_key: HashMap<IdempotencyKey, PerspectiveSeq>,
}

impl InMemIntentStore {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl IntentStore for InMemIntentStore {
    fn put(&self, intent: WriteIntent) -> Result<PutOutcome, IntentError> {
        let mut g = self.inner.lock().lock_or_die("intent_store.inner");
        if let Some(key) = intent.idempotency_key {
            if let Some(&existing) = g.by_key.get(&key) {
                return Ok(PutOutcome::Duplicate(existing));
            }
            g.by_key.insert(key, intent.perspective_seq);
        }
        g.by_seq.insert(intent.perspective_seq, intent);
        Ok(PutOutcome::Recorded)
    }

    fn pending(&self) -> Result<Vec<WriteIntent>, IntentError> {
        Ok(self
            .inner
            .lock()
            .lock_or_die("intent_store.inner")
            .by_seq
            .values()
            .cloned()
            .collect())
    }

    fn next_pending_seq(&self) -> Result<Option<PerspectiveSeq>, IntentError> {
        Ok(self
            .inner
            .lock()
            .lock_or_die("intent_store.inner")
            .by_seq
            .keys()
            .next()
            .copied())
    }

    fn prune(&self, up_to: PerspectiveSeq) -> Result<(), IntentError> {
        let mut g = self.inner.lock().lock_or_die("intent_store.inner");
        // Intents with seq ≤ up_to are incorporated into the Raft log; keep
        // only the strictly-greater remainder.
        g.by_seq.retain(|&s, _| s > up_to);
        // Drop dedup entries whose seq is no longer pending.
        let live: std::collections::HashSet<PerspectiveSeq> = g.by_seq.keys().copied().collect();
        g.by_key.retain(|_, seq| live.contains(seq));
        Ok(())
    }

    fn pending_len(&self) -> Result<usize, IntentError> {
        Ok(self
            .inner
            .lock()
            .lock_or_die("intent_store.inner")
            .by_seq
            .len())
    }
}

// ---------------------------------------------------------------------------
// Durable fjall-backed IntentStore (ADR-047 phase 2)
// ---------------------------------------------------------------------------

/// On-disk value-frame version. Bumped only when the value layout (NOT
/// the embedded proto, which carries its own forward-compat) changes.
const VALUE_VERSION: u8 = 1;

/// On-disk format version persisted in `meta["format_version"]`. The
/// F-4 forward-compat guard: opening a store written by a newer,
/// incompatible binary fails loudly rather than silently mis-reading.
const FORMAT_VERSION: u32 = 1;

const KS_INTENTS: &str = "intents";
const KS_IDEM: &str = "idem";
const KS_META: &str = "meta";
const META_FORMAT_VERSION_KEY: &[u8] = b"format_version";

/// Width of an encoded seq-key: `physical_ms` (8) ‖ `logical` (4) ‖
/// `node_id` (8). Big-endian throughout so byte-lexicographic order on
/// the key == [`PerspectiveSeq`]'s `Ord`, and a forward `iter()` yields
/// ascending perspective order with no in-memory sort.
const SEQ_KEY_LEN: usize = 8 + 4 + 8;

/// Encode a [`PerspectiveSeq`] into its 20-byte, order-preserving key.
///
/// Big-endian `physical_ms` ‖ `logical` ‖ `node_id` mirrors the derived
/// lexicographic `Ord` on `(physical_ms, logical, node_id)`, so the
/// `intents` keyspace iterates in ascending perspective order.
#[must_use]
fn encode_seq_key(seq: PerspectiveSeq) -> [u8; SEQ_KEY_LEN] {
    let hlc = seq.0;
    let mut out = [0u8; SEQ_KEY_LEN];
    out[0..8].copy_from_slice(&hlc.physical_ms.to_be_bytes());
    out[8..12].copy_from_slice(&hlc.logical.to_be_bytes());
    out[12..20].copy_from_slice(&hlc.node_id.0.to_be_bytes());
    out
}

/// Decode a 20-byte seq-key back into a [`PerspectiveSeq`].
///
/// # Errors
/// [`IntentError::Codec`] if the slice is not exactly [`SEQ_KEY_LEN`].
fn decode_seq_key(bytes: &[u8]) -> Result<PerspectiveSeq, IntentError> {
    if bytes.len() != SEQ_KEY_LEN {
        return Err(IntentError::Codec(format!(
            "seq-key must be {SEQ_KEY_LEN} bytes, got {}",
            bytes.len()
        )));
    }
    // Widths are checked above, so the fixed-window try_into never fails.
    let physical_ms = u64::from_be_bytes(
        bytes[0..8]
            .try_into()
            .map_err(|_| IntentError::Codec("seq-key physical_ms".to_string()))?,
    );
    let logical = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| IntentError::Codec("seq-key logical".to_string()))?,
    );
    let node = u64::from_be_bytes(
        bytes[12..20]
            .try_into()
            .map_err(|_| IntentError::Codec("seq-key node_id".to_string()))?,
    );
    Ok(PerspectiveSeq(HybridLogicalClock {
        physical_ms,
        logical,
        node_id: NodeId(node),
    }))
}

/// Encode the durable value frame for an intent.
///
/// Layout: `[version: u8][has_idem: u8][idem: 16B if has][proto_len: u32
/// BE][proto bytes]`, where the proto is the
/// [`append_chunk_and_delta_request_to_proto`] form of `intent.append`.
/// The `perspective_seq` is NOT stored in the value — it is the key, so
/// it is reconstructed from the key on decode (single source of truth,
/// no drift possible).
fn encode_value(intent: &WriteIntent) -> Vec<u8> {
    let proto = append_chunk_and_delta_request_to_proto(&intent.append).encode_to_vec();
    let proto_len = u32::try_from(proto.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(2 + 16 + 4 + proto.len());
    out.push(VALUE_VERSION);
    match intent.idempotency_key {
        Some(key) => {
            out.push(1);
            out.extend_from_slice(&key);
        }
        None => out.push(0),
    }
    out.extend_from_slice(&proto_len.to_be_bytes());
    out.extend_from_slice(&proto);
    out
}

/// The idempotency key and the built append, decoded from a value frame.
/// The perspective seq lives in the row key, so it is supplied by the
/// caller (from [`decode_seq_key`]) when reassembling the [`WriteIntent`].
struct DecodedValue {
    idempotency_key: Option<IdempotencyKey>,
    append: AppendChunkAndDeltaRequest,
}

/// Decode a value frame written by [`encode_value`].
///
/// # Errors
/// [`IntentError::Codec`] on a truncated frame, an unknown value
/// version, or an unparseable embedded proto.
fn decode_value(bytes: &[u8]) -> Result<DecodedValue, IntentError> {
    let mut cur = bytes;
    let version = take_u8(&mut cur, "value version")?;
    if version != VALUE_VERSION {
        return Err(IntentError::Codec(format!(
            "unsupported intent value version {version}"
        )));
    }
    let has_idem = take_u8(&mut cur, "value has_idem")?;
    let idempotency_key = match has_idem {
        0 => None,
        1 => {
            let key = take_array16(&mut cur)?;
            Some(key)
        }
        other => {
            return Err(IntentError::Codec(format!(
                "value has_idem must be 0 or 1, got {other}"
            )));
        }
    };
    let proto_len = take_u32(&mut cur, "value proto_len")? as usize;
    if cur.len() != proto_len {
        return Err(IntentError::Codec(format!(
            "value proto_len {proto_len} != remaining {}",
            cur.len()
        )));
    }
    let proto = ProtoChunkAppendReq::decode(cur)
        .map_err(|e| IntentError::Codec(format!("intent proto decode: {e}")))?;
    let append = proto_to_append_chunk_and_delta(proto).map_err(IntentError::Codec)?;
    Ok(DecodedValue {
        idempotency_key,
        append,
    })
}

fn take_u8(cur: &mut &[u8], what: &str) -> Result<u8, IntentError> {
    let (head, rest) = cur
        .split_first()
        .ok_or_else(|| IntentError::Codec(format!("truncated {what}")))?;
    *cur = rest;
    Ok(*head)
}

fn take_u32(cur: &mut &[u8], what: &str) -> Result<u32, IntentError> {
    if cur.len() < 4 {
        return Err(IntentError::Codec(format!("truncated {what}")));
    }
    let (head, rest) = cur.split_at(4);
    *cur = rest;
    let arr: [u8; 4] = head
        .try_into()
        .map_err(|_| IntentError::Codec(format!("truncated {what}")))?;
    Ok(u32::from_be_bytes(arr))
}

fn take_array16(cur: &mut &[u8]) -> Result<[u8; 16], IntentError> {
    if cur.len() < 16 {
        return Err(IntentError::Codec("truncated value idem key".to_string()));
    }
    let (head, rest) = cur.split_at(16);
    *cur = rest;
    head.try_into()
        .map_err(|_| IntentError::Codec("truncated value idem key".to_string()))
}

/// Durable, fjall-backed [`IntentStore`] (ADR-047 phase 2).
///
/// Single-node durable store — NO quorum replication (that lands in a
/// later phase). Three keyspaces inside one fjall database:
///
/// - `intents` — 20-byte order-preserving seq-key → value frame.
/// - `idem`    — 16-byte idempotency key → the seq-key it first mapped
///   to (so dedup survives reopen, ADR-047 §5 / O3).
/// - `meta`    — `"format_version"` → `u32` BE (the F-4 guard).
///
/// `put` and `prune` commit across `intents` + `idem` in ONE fjall
/// `WriteBatch`, so a crash never leaves a dedup pointer without its
/// intent (or vice-versa). Durability per commit follows `sync_per_write`
/// exactly as `kiseki_chunk`'s `FjallMetaStore` does.
pub struct FjallIntentStore {
    db: Database,
    intents_ks: Keyspace,
    idem_ks: Keyspace,
    /// Per-write fsync vs buffered durability. Defaults to `true`
    /// (POSIX-immediate) so a freshly-opened store is durable until the
    /// runtime explicitly relaxes it for the group-commit perf path.
    sync_per_write: AtomicBool,
    /// Serializes the check-then-insert in `put` and the read-then-delete
    /// in `prune` so they are atomic against each other — fjall has no
    /// transaction across a `get` and a later batch commit, so without
    /// this two concurrent same-`idempotency_key` puts could both miss the
    /// dedup pointer and both record (regressing O3 vs the mutex-atomic
    /// in-memory store). Coarse but correct for the single-node store; the
    /// quorum-write phase revisits granularity.
    mutations: Mutex<()>,
}

impl FjallIntentStore {
    /// Open or create a durable intent store at `path` (a directory, the
    /// fjall keyspace layout).
    ///
    /// Seeds `meta["format_version"]` on a fresh store. On an existing
    /// store with a mismatched version, fails with [`IntentError::Codec`]
    /// (the F-4 forward-compat guard) rather than mis-reading.
    ///
    /// # Errors
    /// [`IntentError::Fjall`] on open / keyspace / seed I/O failure;
    /// [`IntentError::Codec`] if the persisted `format_version` is not
    /// [`FORMAT_VERSION`].
    pub fn open(path: &Path) -> Result<Self, IntentError> {
        let db = Database::builder(path).open()?;
        let intents_ks = db.keyspace(KS_INTENTS, KeyspaceCreateOptions::default)?;
        let idem_ks = db.keyspace(KS_IDEM, KeyspaceCreateOptions::default)?;
        let meta_ks = db.keyspace(KS_META, KeyspaceCreateOptions::default)?;

        if let Some(v) = meta_ks.get(META_FORMAT_VERSION_KEY)? {
            let arr: [u8; 4] = v
                .as_ref()
                .try_into()
                .map_err(|_| IntentError::Codec("format_version must be 4 bytes".to_string()))?;
            let found = u32::from_be_bytes(arr);
            if found != FORMAT_VERSION {
                return Err(IntentError::Codec(format!(
                    "unsupported intent format v{found} (this binary speaks v{FORMAT_VERSION})"
                )));
            }
        } else {
            // Fresh store: seed the version durably so the guard is armed
            // even if the process dies right after open.
            let mut batch = db.batch().durability(Some(PersistMode::SyncAll));
            batch.insert(
                &meta_ks,
                META_FORMAT_VERSION_KEY.to_vec(),
                FORMAT_VERSION.to_be_bytes().to_vec(),
            );
            batch.commit()?;
        }

        Ok(Self {
            db,
            intents_ks,
            idem_ks,
            sync_per_write: AtomicBool::new(true),
            mutations: Mutex::new(()),
        })
    }

    /// Toggle inline-fsync vs buffered durability. Defaults to `true`.
    /// Mirrors `kiseki_chunk`'s `FjallMetaStore::set_sync_per_write`.
    pub fn set_sync_per_write(&self, enabled: bool) {
        self.sync_per_write.store(enabled, Ordering::Relaxed);
    }

    /// Force an fsync of the WAL. Used by the runtime's periodic flusher
    /// when `sync_per_write` is relaxed.
    ///
    /// # Errors
    /// [`IntentError::Fjall`] if the persist call fails.
    pub fn flush(&self) -> Result<(), IntentError> {
        self.db.persist(PersistMode::SyncAll)?;
        Ok(())
    }

    /// Build a batch whose durability follows `sync_per_write`. `None`
    /// durability = "queue the WAL bytes; periodic flusher fsyncs".
    fn batch_for_write(&self) -> OwnedWriteBatch {
        let durability = if self.sync_per_write.load(Ordering::Relaxed) {
            Some(PersistMode::SyncAll)
        } else {
            None
        };
        self.db.batch().durability(durability)
    }
}

impl IntentStore for FjallIntentStore {
    fn put(&self, intent: WriteIntent) -> Result<PutOutcome, IntentError> {
        // Hold the mutation lock across the check + commit so the dedup
        // check-then-insert is atomic (fjall has no get→batch transaction).
        let _guard = self
            .mutations
            .lock()
            .lock_or_die("intent_store.fjall.mutations");
        // Idempotency check first: a duplicate key returns the ORIGINAL
        // seq and writes nothing (ADR-047 §5 / O3). The dedup pointer is
        // persisted, so this holds across reopen.
        if let Some(key) = intent.idempotency_key {
            if let Some(existing) = self.idem_ks.get(key)? {
                let seq = decode_seq_key(existing.as_ref())?;
                return Ok(PutOutcome::Duplicate(seq));
            }
        }

        let seq_key = encode_seq_key(intent.perspective_seq);
        let value = encode_value(&intent);

        // One batch: the intent row and (if keyed) its dedup pointer
        // commit atomically. A crash mid-commit replays the WAL all-or-
        // nothing, so a dedup pointer never outlives its intent.
        let mut batch = self.batch_for_write();
        batch.insert(&self.intents_ks, seq_key.to_vec(), value);
        if let Some(key) = intent.idempotency_key {
            batch.insert(&self.idem_ks, key.to_vec(), seq_key.to_vec());
        }
        batch.commit()?;
        Ok(PutOutcome::Recorded)
    }

    fn pending(&self) -> Result<Vec<WriteIntent>, IntentError> {
        let mut out = Vec::new();
        for entry in self.intents_ks.iter() {
            let (k, v) = entry.into_inner()?;
            let seq = decode_seq_key(k.as_ref())?;
            let decoded = decode_value(v.as_ref())?;
            out.push(WriteIntent {
                perspective_seq: seq,
                idempotency_key: decoded.idempotency_key,
                append: decoded.append,
            });
        }
        Ok(out)
    }

    fn next_pending_seq(&self) -> Result<Option<PerspectiveSeq>, IntentError> {
        match self.intents_ks.iter().next() {
            Some(entry) => {
                let (k, _v) = entry.into_inner()?;
                Ok(Some(decode_seq_key(k.as_ref())?))
            }
            None => Ok(None),
        }
    }

    fn prune(&self, up_to: PerspectiveSeq) -> Result<(), IntentError> {
        // Serialize against `put` (and concurrent prunes) so the
        // intent/dedup-pointer pair is mutated atomically.
        let _guard = self
            .mutations
            .lock()
            .lock_or_die("intent_store.fjall.mutations");
        let cutoff = encode_seq_key(up_to);
        let mut batch = self.batch_for_write();
        // Iterate intents with seq-key ≤ cutoff (inclusive). Keys are
        // order-preserving, so a byte-wise `<=` is exactly seq `<= up_to`.
        for entry in self.intents_ks.iter() {
            let (k, v) = entry.into_inner()?;
            if k.as_ref() > cutoff.as_slice() {
                // Ascending order — the first strictly-greater key means
                // nothing past it can be ≤ cutoff. Stop scanning.
                break;
            }
            // Drop the intent row and, if it carried one, its dedup
            // pointer — both in the same batch so the pair stays
            // consistent across a crash mid-prune.
            batch.remove(&self.intents_ks, k.as_ref().to_vec());
            let decoded = decode_value(v.as_ref())?;
            if let Some(idem) = decoded.idempotency_key {
                batch.remove(&self.idem_ks, idem.to_vec());
            }
        }
        batch.commit()?;
        Ok(())
    }

    fn pending_len(&self) -> Result<usize, IntentError> {
        // Not a hot path (observability / backpressure); a full count is
        // acceptable per the trait contract.
        Ok(self.intents_ks.iter().count())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, WallTime};

    use crate::delta::OperationType;
    use crate::traits::AppendDeltaRequest;

    fn seq(physical_ms: u64, logical: u32, node: u64) -> PerspectiveSeq {
        PerspectiveSeq(HybridLogicalClock {
            physical_ms,
            logical,
            node_id: NodeId(node),
        })
    }

    fn intent(s: PerspectiveSeq, key: Option<IdempotencyKey>) -> WriteIntent {
        WriteIntent {
            perspective_seq: s,
            idempotency_key: key,
            append: AppendChunkAndDeltaRequest {
                delta: AppendDeltaRequest {
                    shard_id: ShardId(uuid::Uuid::from_u128(1)),
                    tenant_id: OrgId(uuid::Uuid::from_u128(100)),
                    operation: OperationType::Create,
                    timestamp: DeltaTimestamp {
                        hlc: s.0,
                        wall: WallTime {
                            millis_since_epoch: s.0.physical_ms,
                            timezone: "UTC".into(),
                        },
                        quality: ClockQuality::Ntp,
                    },
                    hashed_key: [0u8; 32],
                    chunk_refs: vec![],
                    payload: vec![],
                    has_inline_data: false,
                },
                new_chunks: vec![],
            },
        }
    }

    #[test]
    fn perspective_seq_total_order() {
        // (physical, logical, node) lexicographic — node_id is the final tie-break.
        assert!(seq(1, 0, 1) < seq(1, 0, 2));
        assert!(seq(1, 0, 2) < seq(1, 1, 1));
        assert!(seq(1, 9, 9) < seq(2, 0, 1));
        let mut v = vec![seq(2, 0, 1), seq(1, 1, 1), seq(1, 0, 2), seq(1, 0, 1)];
        v.sort();
        assert_eq!(
            v,
            vec![seq(1, 0, 1), seq(1, 0, 2), seq(1, 1, 1), seq(2, 0, 1)]
        );
    }

    #[test]
    fn put_records_and_pending_is_ordered() {
        let s = InMemIntentStore::new();
        // Insert out of order; pending() must come back ascending.
        assert_eq!(
            s.put(intent(seq(3, 0, 1), None)).unwrap(),
            PutOutcome::Recorded
        );
        assert_eq!(
            s.put(intent(seq(1, 0, 1), None)).unwrap(),
            PutOutcome::Recorded
        );
        assert_eq!(
            s.put(intent(seq(2, 0, 1), None)).unwrap(),
            PutOutcome::Recorded
        );
        let ordered: Vec<_> = s
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(ordered, vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        assert_eq!(s.next_pending_seq().unwrap(), Some(seq(1, 0, 1)));
        assert_eq!(s.pending_len().unwrap(), 3);
    }

    #[test]
    fn put_is_idempotent_on_key() {
        let s = InMemIntentStore::new();
        let key = [7u8; 16];
        assert_eq!(
            s.put(intent(seq(1, 0, 1), Some(key))).unwrap(),
            PutOutcome::Recorded
        );
        // Same key, re-ingressed on another node with a fresh (later) seq:
        // returns the ORIGINAL seq and does not add a second entry (O3).
        assert_eq!(
            s.put(intent(seq(5, 0, 2), Some(key))).unwrap(),
            PutOutcome::Duplicate(seq(1, 0, 1))
        );
        assert_eq!(s.pending_len().unwrap(), 1);
    }

    #[test]
    fn keyless_puts_are_never_deduped() {
        let s = InMemIntentStore::new();
        assert_eq!(
            s.put(intent(seq(1, 0, 1), None)).unwrap(),
            PutOutcome::Recorded
        );
        assert_eq!(
            s.put(intent(seq(2, 0, 1), None)).unwrap(),
            PutOutcome::Recorded
        );
        assert_eq!(s.pending_len().unwrap(), 2);
    }

    #[test]
    fn prune_drops_incorporated_inclusive() {
        let s = InMemIntentStore::new();
        let k2 = [2u8; 16];
        s.put(intent(seq(1, 0, 1), None)).unwrap();
        s.put(intent(seq(2, 0, 1), Some(k2))).unwrap();
        s.put(intent(seq(3, 0, 1), None)).unwrap();
        // Incorporated through seq(2,0,1) inclusive.
        s.prune(seq(2, 0, 1)).unwrap();
        let remaining: Vec<_> = s
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(remaining, vec![seq(3, 0, 1)]);
        assert_eq!(s.next_pending_seq().unwrap(), Some(seq(3, 0, 1)));
        // The pruned key's dedup entry is gone, so its key can be reused.
        assert_eq!(
            s.put(intent(seq(4, 0, 1), Some(k2))).unwrap(),
            PutOutcome::Recorded
        );
        assert_eq!(s.pending_len().unwrap(), 2);
    }

    #[test]
    fn next_pending_seq_none_when_empty() {
        let s = InMemIntentStore::new();
        assert_eq!(s.next_pending_seq().unwrap(), None);
        assert_eq!(s.pending_len().unwrap(), 0);
    }

    // -- seq-key encoding (the order-preservation contract) --------------

    #[test]
    fn seq_key_round_trips() {
        for s in [
            seq(0, 0, 0),
            seq(1, 0, 1),
            seq(7, 9, 42),
            seq(u64::MAX, u32::MAX, u64::MAX),
        ] {
            assert_eq!(decode_seq_key(&encode_seq_key(s)).unwrap(), s);
        }
    }

    #[test]
    fn seq_key_byte_order_matches_perspective_ord() {
        // Byte-lexicographic order on the key MUST equal PerspectiveSeq Ord,
        // or the intents keyspace would not iterate ascending.
        let mut seqs = vec![
            seq(2, 0, 1),
            seq(1, 1, 1),
            seq(1, 0, 2),
            seq(1, 0, 1),
            seq(1, 9, 9),
        ];
        let mut keys: Vec<_> = seqs.iter().map(|s| encode_seq_key(*s)).collect();
        seqs.sort();
        keys.sort_unstable();
        let decoded: Vec<_> = keys.iter().map(|k| decode_seq_key(k).unwrap()).collect();
        assert_eq!(decoded, seqs);
    }

    #[test]
    fn decode_seq_key_rejects_wrong_width() {
        assert!(matches!(
            decode_seq_key(&[0u8; 19]),
            Err(IntentError::Codec(_))
        ));
    }

    // -- FjallIntentStore (the durable store) ----------------------------

    #[test]
    fn fjall_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intents");
        {
            let s = FjallIntentStore::open(&path).unwrap();
            // Insert out of seq order; pending() must come back ascending.
            assert_eq!(
                s.put(intent(seq(3, 0, 1), None)).unwrap(),
                PutOutcome::Recorded
            );
            assert_eq!(
                s.put(intent(seq(1, 0, 1), None)).unwrap(),
                PutOutcome::Recorded
            );
            assert_eq!(
                s.put(intent(seq(2, 0, 1), None)).unwrap(),
                PutOutcome::Recorded
            );
            s.flush().unwrap();
        }
        let s = FjallIntentStore::open(&path).unwrap();
        let ordered: Vec<_> = s
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(ordered, vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        assert_eq!(s.next_pending_seq().unwrap(), Some(seq(1, 0, 1)));
        assert_eq!(s.pending_len().unwrap(), 3);
    }

    #[test]
    fn fjall_prune_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intents");
        let k2 = [2u8; 16];
        {
            let s = FjallIntentStore::open(&path).unwrap();
            s.put(intent(seq(1, 0, 1), None)).unwrap();
            s.put(intent(seq(2, 0, 1), Some(k2))).unwrap();
            s.put(intent(seq(3, 0, 1), None)).unwrap();
            s.prune(seq(2, 0, 1)).unwrap();
            s.flush().unwrap();
        }
        let s = FjallIntentStore::open(&path).unwrap();
        let remaining: Vec<_> = s
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(remaining, vec![seq(3, 0, 1)]);
        assert_eq!(s.next_pending_seq().unwrap(), Some(seq(3, 0, 1)));
        assert_eq!(s.pending_len().unwrap(), 1);
        // The pruned key's dedup pointer was removed in the same batch, so
        // the key is reusable after reopen.
        assert_eq!(
            s.put(intent(seq(4, 0, 1), Some(k2))).unwrap(),
            PutOutcome::Recorded
        );
        assert_eq!(s.pending_len().unwrap(), 2);
    }

    #[test]
    fn fjall_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intents");
        let key = [7u8; 16];
        {
            let s = FjallIntentStore::open(&path).unwrap();
            assert_eq!(
                s.put(intent(seq(1, 0, 1), Some(key))).unwrap(),
                PutOutcome::Recorded
            );
            // Re-ingress on another node with a later seq — returns the
            // ORIGINAL seq, writes nothing.
            assert_eq!(
                s.put(intent(seq(5, 0, 2), Some(key))).unwrap(),
                PutOutcome::Duplicate(seq(1, 0, 1))
            );
            assert_eq!(s.pending_len().unwrap(), 1);
            s.flush().unwrap();
        }
        // The dedup pointer is persisted: still 1 pending, still dedups.
        let s = FjallIntentStore::open(&path).unwrap();
        assert_eq!(s.pending_len().unwrap(), 1);
        assert_eq!(
            s.put(intent(seq(9, 0, 3), Some(key))).unwrap(),
            PutOutcome::Duplicate(seq(1, 0, 1))
        );
        assert_eq!(s.pending_len().unwrap(), 1);
    }

    #[test]
    fn fjall_iter_is_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let s = FjallIntentStore::open(&dir.path().join("intents")).unwrap();
        // Insert deliberately out of seq order across all three HLC fields.
        for x in [
            seq(2, 0, 1),
            seq(1, 1, 1),
            seq(1, 0, 2),
            seq(1, 0, 1),
            seq(1, 9, 9),
        ] {
            s.put(intent(x, None)).unwrap();
        }
        let ordered: Vec<_> = s
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(
            ordered,
            vec![
                seq(1, 0, 1),
                seq(1, 0, 2),
                seq(1, 1, 1),
                seq(1, 9, 9),
                seq(2, 0, 1)
            ]
        );
    }

    #[test]
    fn fjall_format_version_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intents");
        // First open seeds format_version = 1.
        {
            let s = FjallIntentStore::open(&path).unwrap();
            s.put(intent(seq(1, 0, 1), None)).unwrap();
            s.flush().unwrap();
        }
        // Stomp a bogus version directly into the meta keyspace, then drop.
        {
            let db = Database::builder(&path).open().unwrap();
            let meta_ks = db
                .keyspace(KS_META, KeyspaceCreateOptions::default)
                .unwrap();
            let mut batch = db.batch().durability(Some(PersistMode::SyncAll));
            batch.insert(
                &meta_ks,
                META_FORMAT_VERSION_KEY.to_vec(),
                999u32.to_be_bytes().to_vec(),
            );
            batch.commit().unwrap();
            db.persist(PersistMode::SyncAll).unwrap();
        }
        // Reopen must reject the unsupported format. (`FjallIntentStore`
        // is not `Debug` — fjall handles aren't — so match on the Result
        // rather than `unwrap_err`.)
        match FjallIntentStore::open(&path) {
            Err(IntentError::Codec(_)) => {}
            Err(other) => panic!("expected Codec, got {other:?}"),
            Ok(_) => panic!("expected open to reject bogus format_version"),
        }
    }

    #[test]
    fn fjall_put_is_atomic_under_concurrency() {
        // Gate-1 O3 / F-P2-1 regression guard: N threads racing the SAME
        // idempotency_key (distinct seqs) must yield exactly one Recorded;
        // the rest Duplicate; exactly one intent persisted. Without the
        // mutation guard the check-then-insert races and two could record.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("intents");
        let store = std::sync::Arc::new(FjallIntentStore::open(&path).unwrap());
        let key = [9u8; 16];
        let handles: Vec<_> = (0..16u64)
            .map(|n| {
                let s = std::sync::Arc::clone(&store);
                // Distinct seq per thread, same idempotency key.
                std::thread::spawn(move || s.put(intent(seq(1, 0, n + 1), Some(key))).unwrap())
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let recorded = outcomes
            .iter()
            .filter(|o| **o == PutOutcome::Recorded)
            .count();
        let duplicate = outcomes
            .iter()
            .filter(|o| matches!(o, PutOutcome::Duplicate(_)))
            .count();
        assert_eq!(recorded, 1, "exactly one writer records");
        assert_eq!(duplicate, 15, "the rest dedup to the first");
        assert_eq!(store.pending_len().unwrap(), 1, "one intent persisted");
    }
}
