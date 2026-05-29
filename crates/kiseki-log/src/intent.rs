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
use std::sync::Mutex;

use kiseki_common::locks::LockOrDie;
use kiseki_common::time::HybridLogicalClock;

use crate::traits::AppendChunkAndDeltaRequest;

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
    fn put(&self, intent: WriteIntent) -> PutOutcome;

    /// Pending (un-incorporated) intents, ascending by perspective seq — the
    /// order the async committer applies them (ADR-047 §3).
    fn pending(&self) -> Vec<WriteIntent>;

    /// Lowest pending perspective seq — this replica's contribution to the
    /// stability watermark (ADR-047 §3; the watermark advances on a *majority*
    /// low-water-mark per gate-1 F-1). `None` if nothing is pending.
    fn next_pending_seq(&self) -> Option<PerspectiveSeq>;

    /// Drop intents up to and including `up_to` once incorporated into the
    /// Raft log. Pruning is an optimization derivable from the log, never a
    /// correctness dependency (ADR-047 §F-2 — recovery re-derives from the
    /// log's `max_incorporated_seq`).
    fn prune(&self, up_to: PerspectiveSeq);

    /// Pending count — observability + backpressure (ADR-047 §F-6).
    fn pending_len(&self) -> usize;
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
    fn put(&self, intent: WriteIntent) -> PutOutcome {
        let mut g = self.inner.lock().lock_or_die("intent_store.inner");
        if let Some(key) = intent.idempotency_key {
            if let Some(&existing) = g.by_key.get(&key) {
                return PutOutcome::Duplicate(existing);
            }
            g.by_key.insert(key, intent.perspective_seq);
        }
        g.by_seq.insert(intent.perspective_seq, intent);
        PutOutcome::Recorded
    }

    fn pending(&self) -> Vec<WriteIntent> {
        self.inner.lock().lock_or_die("intent_store.inner").by_seq.values().cloned().collect()
    }

    fn next_pending_seq(&self) -> Option<PerspectiveSeq> {
        self.inner.lock().lock_or_die("intent_store.inner").by_seq.keys().next().copied()
    }

    fn prune(&self, up_to: PerspectiveSeq) {
        let mut g = self.inner.lock().lock_or_die("intent_store.inner");
        // Intents with seq ≤ up_to are incorporated into the Raft log; keep
        // only the strictly-greater remainder.
        g.by_seq.retain(|&s, _| s > up_to);
        // Drop dedup entries whose seq is no longer pending.
        let live: std::collections::HashSet<PerspectiveSeq> =
            g.by_seq.keys().copied().collect();
        g.by_key.retain(|_, seq| live.contains(seq));
    }

    fn pending_len(&self) -> usize {
        self.inner.lock().lock_or_die("intent_store.inner").by_seq.len()
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
        assert_eq!(v, vec![seq(1, 0, 1), seq(1, 0, 2), seq(1, 1, 1), seq(2, 0, 1)]);
    }

    #[test]
    fn put_records_and_pending_is_ordered() {
        let s = InMemIntentStore::new();
        // Insert out of order; pending() must come back ascending.
        assert_eq!(s.put(intent(seq(3, 0, 1), None)), PutOutcome::Recorded);
        assert_eq!(s.put(intent(seq(1, 0, 1), None)), PutOutcome::Recorded);
        assert_eq!(s.put(intent(seq(2, 0, 1), None)), PutOutcome::Recorded);
        let ordered: Vec<_> = s.pending().iter().map(|i| i.perspective_seq).collect();
        assert_eq!(ordered, vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        assert_eq!(s.next_pending_seq(), Some(seq(1, 0, 1)));
        assert_eq!(s.pending_len(), 3);
    }

    #[test]
    fn put_is_idempotent_on_key() {
        let s = InMemIntentStore::new();
        let key = [7u8; 16];
        assert_eq!(s.put(intent(seq(1, 0, 1), Some(key))), PutOutcome::Recorded);
        // Same key, re-ingressed on another node with a fresh (later) seq:
        // returns the ORIGINAL seq and does not add a second entry (O3).
        assert_eq!(
            s.put(intent(seq(5, 0, 2), Some(key))),
            PutOutcome::Duplicate(seq(1, 0, 1))
        );
        assert_eq!(s.pending_len(), 1);
    }

    #[test]
    fn keyless_puts_are_never_deduped() {
        let s = InMemIntentStore::new();
        assert_eq!(s.put(intent(seq(1, 0, 1), None)), PutOutcome::Recorded);
        assert_eq!(s.put(intent(seq(2, 0, 1), None)), PutOutcome::Recorded);
        assert_eq!(s.pending_len(), 2);
    }

    #[test]
    fn prune_drops_incorporated_inclusive() {
        let s = InMemIntentStore::new();
        let k2 = [2u8; 16];
        s.put(intent(seq(1, 0, 1), None));
        s.put(intent(seq(2, 0, 1), Some(k2)));
        s.put(intent(seq(3, 0, 1), None));
        // Incorporated through seq(2,0,1) inclusive.
        s.prune(seq(2, 0, 1));
        let remaining: Vec<_> = s.pending().iter().map(|i| i.perspective_seq).collect();
        assert_eq!(remaining, vec![seq(3, 0, 1)]);
        assert_eq!(s.next_pending_seq(), Some(seq(3, 0, 1)));
        // The pruned key's dedup entry is gone, so its key can be reused.
        assert_eq!(s.put(intent(seq(4, 0, 1), Some(k2))), PutOutcome::Recorded);
        assert_eq!(s.pending_len(), 2);
    }

    #[test]
    fn next_pending_seq_none_when_empty() {
        let s = InMemIntentStore::new();
        assert_eq!(s.next_pending_seq(), None);
        assert_eq!(s.pending_len(), 0);
    }
}
