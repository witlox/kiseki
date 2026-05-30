//! ADR-047 `LeaderSink` — the leader-only incorporator: the seam to the Raft
//! log and the drain-all incorporation algorithm.
//!
//! Phases 1–2 ([`crate::intent`]) own the perspective sequence, the durable
//! intent record, and the per-replica [`IntentStore`]. This module is the
//! **consensus logic** that drains those intents into the Raft log *after*
//! the client has already been acked (the ADR-047 decoupling).
//!
//! **`LeaderSink` (the redesign).** The stability watermark is gone. The shard
//! LEADER is the sole incorporator: the durability fan *includes the leader*
//! (so the leader holds every acked intent locally), and the leader's committer
//! simply drains its OWN store ascending-by-perspective-seq into the Raft log
//! in BATCHES of up to [`DRAIN_BATCH_CAP`] — no peer gossip, no majority
//! watermark. There is exactly one writer (the Raft leader) appending; the
//! state machine's per-intent dedup gate (ancient cutoff + recent set, PART 8)
//! makes re-incorporation safe.
//!
//! 1. [`IncorporationSink`] — the abstract seam to the real Raft log. PART 8
//!    deletes the global-floor read; the sink only appends batches. The SM
//!    apply gate is authoritative.
//! 2. [`Committer::run`] — drains ALL local pending intents (no floor filter;
//!    the broken global floor silently dropped legitimate writes per PART 6),
//!    batches them at [`DRAIN_BATCH_CAP`] per pass, appends each batch via the
//!    sink, then prunes the *successfully appended* seqs only.
//! 3. [`recover_pending`] / [`restore_into`] — election recovery (gate-1 O2):
//!    a new leader unions the pending sets across a `cluster_size − min_acks + 1`
//!    gather before resuming, so it holds every acked intent.

use std::collections::BTreeMap;

use crate::intent::{IntentError, IntentStore, PerspectiveSeq, WriteIntent};

/// The recovery-gather threshold (MF-7 / gate-1 O2): the number of *distinct*
/// replica stores a new leader must union before resuming incorporation, so the
/// gather intersects every `min_acks`-durable intent's durability set.
///
/// `RF − min_acks + 1`, where `RF = cluster_size` (the voter count). `min_acks`
/// is clamped to `1..=cluster_size` so the result is always in
/// `1..=cluster_size`. For `RF=3`/`min_acks=2` → 2 (bare majority); for
/// `RF=6`/`min_acks=2` → 5. See [`recover_pending`] for the overlap proof.
#[must_use]
fn gather_threshold(cluster_size: usize, min_acks: usize) -> usize {
    let rf = cluster_size.max(1);
    let acks = min_acks.clamp(1, rf);
    rf - acks + 1
}

/// Per-pass cap on the number of intents one [`Committer::run`] hands to the
/// sink in a single batch (PART 8 §U / Finding U). The drain loop may run
/// multiple back-to-back passes if the pending queue is deeper than this, but
/// no single Raft round absorbs more than [`DRAIN_BATCH_CAP`] entries — so a
/// recovery-time queue of millions of intents can't lock up the committer
/// thread or the SM apply pipeline for an unbounded duration.
pub const DRAIN_BATCH_CAP: usize = 1_000;

/// The seam between the async committer and the Raft log (ADR-047 §3).
///
/// The committer is pure algorithm; this trait is where its output lands in
/// the real, replicated log. The Raft-backed implementation arrives in phase 5
/// — until then [`RecordingSink`] stands in for tests.
///
/// **The state-machine apply gate is the source of truth.** PART 8 deletes
/// the global-floor read; the SM's `recent_incorporated` + ancient cutoff
/// authoritatively dedup. The sink only appends batches; per-intent dedup
/// happens replicated and identically on every replica.
pub trait IncorporationSink {
    /// Append `ordered` (already ascending by perspective-seq) to the Raft
    /// log as one batched [`crate::raft_store::LogCommand::IncorporateIntents`]
    /// command. The SM apply runs the per-intent gate (recent set + ancient
    /// cutoff) so a re-fanned / replayed / re-gathered seq is a no-op there.
    ///
    /// # Errors
    /// Backing-store / Raft-append failure.
    fn incorporate(&mut self, ordered: &[WriteIntent]) -> Result<(), IntentError>;
}

/// In-memory [`IncorporationSink`] for tests: records every incorporated
/// intent in the order it was appended. PART 8 removes the floor cache — the
/// SM apply gate is authoritative.
#[derive(Default)]
pub struct RecordingSink {
    /// Every incorporated seq, in incorporation order (one entry per intent).
    pub incorporated: Vec<PerspectiveSeq>,
    /// Every incorporated intent, in incorporation order.
    pub intents: Vec<WriteIntent>,
    /// Batch boundaries — the index in [`Self::incorporated`] where each batch
    /// begins. Useful for assertions like "the drain split into 3 batches".
    pub batch_starts: Vec<usize>,
}

impl RecordingSink {
    /// Empty sink — nothing incorporated yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl IncorporationSink for RecordingSink {
    fn incorporate(&mut self, ordered: &[WriteIntent]) -> Result<(), IntentError> {
        self.batch_starts.push(self.incorporated.len());
        for intent in ordered {
            self.incorporated.push(intent.perspective_seq);
            self.intents.push(intent.clone());
        }
        Ok(())
    }
}

/// The async committer (ADR-047 `LeaderSink`): drains the LEADER's own
/// [`IntentStore`] into the Raft log behind an [`IncorporationSink`],
/// idempotently. No watermark, no peer reports — the single-incorporator model.
///
/// PART 8 changes the contract:
/// - **No floor filter.** PART 6 proved the global floor silently dropped
///   legitimate multi-writer late arrivals (newer-seq applied first → older-
///   seq filtered out). The SM apply gate (recent set + ancient cutoff) is
///   authoritative — drain ALL pending and let the SM dedup.
/// - **Per-pass batch cap.** [`DRAIN_BATCH_CAP`] = 1 000 items per `incorporate`
///   call so one Raft round absorbs at most that many; recovery floods can't
///   monopolize the committer thread (Finding U).
/// - **Per-intent prune.** Prune is removed from this loop; the supervisor's
///   off-band per-intent prune (driven by the SM's `recent_incorporated`
///   snapshot) handles it (PART 8 §T).
///
/// TODO(ADR-047 MF-4): the client `idempotency_key` dedup index is not yet
/// built. For now the recent-set + ancient cutoff dedup collapses re-gathers,
/// re-fans, and replays of the SAME seq, but a re-ingressed client retry
/// minting a FRESH seq + same `idempotency_key` would incorporate twice.
pub struct Committer;

impl Committer {
    /// Run one drain-all committer pass (`LeaderSink` steady state, PART 8).
    ///
    /// The caller MUST be the shard leader (the leader-only supervisor enforces
    /// this — see `RaftShardStore`). The leader holds every acked intent in its
    /// own store (the durability fan includes the leader), so it incorporates
    /// purely from `store`:
    ///
    /// 1. Read ALL pending intents from the store — **no floor filter** (PART 6
    ///    bug: a global floor silently drops legitimate older-seq writes from
    ///    other writers; the SM's per-intent dedup is authoritative).
    /// 2. Sort ascending by perspective-seq (LWW order for same-name writes).
    /// 3. Walk in batches of up to [`DRAIN_BATCH_CAP`] entries; for each batch
    ///    call [`IncorporationSink::incorporate`] (one batched Raft round per
    ///    batch). Stop on the first error.
    ///
    /// **Pruning is OFF-BAND.** This loop does NOT call [`IntentStore::prune`]
    /// any more (Finding T): the supervisor reads the SM's recent-incorporated
    /// snapshot every tick and per-intent-prunes the local store. That keeps
    /// the consensus apply path isolated from intent-store I/O faults.
    ///
    /// # Returns
    /// The number of intents incorporated (`0` if the store was empty).
    ///
    /// # Errors
    /// Propagates [`IntentError`] from [`IntentStore::pending`] or
    /// [`IncorporationSink::incorporate`].
    pub fn run(
        store: &dyn IntentStore,
        sink: &mut dyn IncorporationSink,
    ) -> Result<usize, IntentError> {
        let mut pending = store.pending()?;
        if pending.is_empty() {
            return Ok(0);
        }
        // Perspective-seq ascending: same-name LWW order on append.
        pending.sort_by_key(|intent| intent.perspective_seq);

        // Walk in DRAIN_BATCH_CAP batches; each batch is one Raft round.
        let mut total = 0usize;
        for batch in pending.chunks(DRAIN_BATCH_CAP) {
            sink.incorporate(batch)?;
            total += batch.len();
        }
        Ok(total)
    }
}

// ---------------------------------------------------------------------------
// Election intent-recovery (ADR-047 phase 4 / gate-1 O2)
// ---------------------------------------------------------------------------

/// Reconstruct the complete pending set on a new leader by unioning the
/// pending intents across a gathered quorum of replica intent stores, deduped
/// by perspective-seq, ascending. Requires `>= cluster_size − min_acks + 1`
/// distinct stores (else [`IntentError::InsufficientQuorum`]).
///
/// **The overlap threshold (MF-7 / gate-1 O2).** An acked intent is durable on
/// a set `D` of `min_acks` replicas. The gather collects a set `G`. For `G` to
/// intersect *every* possible `D` (so the union misses no acked intent) we need
/// `|D ∩ G| ≥ |D| + |G| − RF ≥ 1`, i.e. `|G| ≥ RF − min_acks + 1`. Here `RF` is
/// the voter count, passed as `cluster_size`. So:
///
/// ```text
/// |D ∩ G| ≥ min_acks + (cluster_size − min_acks + 1) − cluster_size = 1 > 0 ✓
/// ```
///
/// For `RF=3`/`min_acks=2` this is `2` (a bare majority — matches the common case).
/// For `RF=6`/`min_acks=2` this is `5`. **A bare `majority(RF)` is UNSAFE for
/// `RF=6`/`min_acks=2`** (a 4-of-6 gather and a 2-of-6 durability set can be
/// disjoint: 4+2=6), which is why this threshold — not `majority` — is the guard.
///
/// **Liveness tradeoff (documented, owned).** Requiring `RF − min_acks + 1`
/// reachable means recovery STALLS under a wide-shard double-fault: on
/// `RF=6`/`min_acks=2` it needs 5 of 6 voters, so a 2-node-down election cannot
/// resume incorporation (new-write visibility on the new leader) until a 5th
/// returns. Already-visible reads continue. Safety is never traded for liveness:
/// we refuse + retry rather than resume on an under-gathered set that could miss
/// an acked intent. The pain is specifically *wide shards with tiny `min_acks`*;
/// narrow `RF=3`/`min_acks=2` only needs 2-of-3.
///
/// **At-least-once for un-acked writes (gate-1 F-P4-1).** The union includes
/// *every* pending intent on the gathered replicas — including a write that
/// reached only one replica and was never `min_acks`-acked (an interrupted
/// fan-out). It is I-L5-safe **only because** the producer fans chunks to
/// `min_acks` *before* the metadata intent (data-before-metadata), so even a
/// partial intent composes over durable chunks. Callers MUST pass *distinct*
/// replica stores: the guard counts entries and cannot detect a store passed
/// twice (the node-dedup is the caller's job — see [`ShardCommitter::recover`]).
///
/// `min_acks` is clamped to `1..=cluster_size` so the threshold stays in
/// `1..=cluster_size` (a `min_acks` larger than RF, or 0, is nonsensical).
///
/// # Errors
/// [`IntentError::InsufficientQuorum`] if `replica_stores.len()` is below
/// `cluster_size − min_acks + 1`; otherwise propagates [`IntentError`] from
/// [`IntentStore::pending`].
pub fn recover_pending(
    replica_stores: &[&dyn IntentStore],
    cluster_size: usize,
    min_acks: usize,
) -> Result<Vec<WriteIntent>, IntentError> {
    let need = gather_threshold(cluster_size, min_acks);
    let have = replica_stores.len();
    if have < need {
        return Err(IntentError::InsufficientQuorum { have, need });
    }

    // Union by perspective-seq. The BTreeMap dedups (identical seq from
    // multiple replicas collapses to one — perspective-seqs are globally
    // unique so there is no real value conflict) and keeps the result
    // ascending by seq with no separate sort.
    let mut by_seq: BTreeMap<PerspectiveSeq, WriteIntent> = BTreeMap::new();
    for store in replica_stores {
        for intent in store.pending()? {
            by_seq.entry(intent.perspective_seq).or_insert(intent);
        }
    }
    Ok(by_seq.into_values().collect())
}

/// Load recovered intents into the new leader's local store so the committer
/// can incorporate them. Idempotent: re-running with the same set is a no-op
/// (the store keys by perspective-seq). Returns the number newly inserted.
///
/// Recovery only rebuilds the pending set; it does **not** filter against the
/// log. The committer's F-2 `seq > max_incorporated` floor — not recovery — is
/// what drops anything already in the Raft log on the next [`Committer::run`].
///
/// "Newly inserted" is measured by perspective-seq, not by
/// [`PutOutcome`]: a keyless re-restore of the same seq returns
/// [`PutOutcome::Recorded`] (the store overwrites the row in place) yet adds
/// nothing, so the count is taken against the pre-existing seq set. This keeps
/// the return honest for both keyed and keyless intents.
///
/// # Errors
/// Propagates [`IntentError`] from [`IntentStore::pending`] or
/// [`IntentStore::put`].
pub fn restore_into(
    target: &dyn IntentStore,
    recovered: &[WriteIntent],
) -> Result<usize, IntentError> {
    // Snapshot the seqs already present so the count reflects genuine first
    // insertions (the store keys by perspective-seq; a keyless put of an
    // existing seq overwrites in place and still reports `Recorded`).
    let existing: std::collections::BTreeSet<PerspectiveSeq> = target
        .pending()?
        .into_iter()
        .map(|i| i.perspective_seq)
        .collect();
    let mut inserted = 0;
    for intent in recovered {
        let is_new = !existing.contains(&intent.perspective_seq);
        let _ = target.put(intent.clone())?;
        if is_new {
            inserted += 1;
        }
    }
    Ok(inserted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::delta::OperationType;
    use crate::intent::{IdempotencyKey, InMemIntentStore, PutOutcome};
    use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};
    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

    // Minimal copies of the phase-2 test helpers (kept private to `intent`'s
    // test module, so duplicated here per the phase-3 plan).
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

    // ---- gather_threshold (MF-7) ------------------------------------------

    #[test]
    fn gather_threshold_rf3_min2_is_bare_majority() {
        // RF=3, min_acks=2 → 3 - 2 + 1 = 2 (the common-case bare majority).
        assert_eq!(gather_threshold(3, 2), 2);
    }

    #[test]
    fn gather_threshold_rf6_min2_is_five() {
        // RF=6, min_acks=2 → 6 - 2 + 1 = 5 (NOT majority(6)=4 — the safety fix).
        assert_eq!(gather_threshold(6, 2), 5);
    }

    #[test]
    fn gather_threshold_clamps_degenerate_inputs() {
        // min_acks=0 clamps to 1 → full RF. min_acks > RF clamps to RF → 1.
        assert_eq!(gather_threshold(3, 0), 3);
        assert_eq!(gather_threshold(3, 9), 1);
        // RF=1 (single node) always needs 1 view.
        assert_eq!(gather_threshold(1, 2), 1);
    }

    // ---- Committer::run (`LeaderSink` drain-all) ----------------------------

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s, None)).unwrap(), PutOutcome::Recorded);
        }
    }

    #[test]
    fn run_drains_all_pending_in_ascending_order() {
        let store = InMemIntentStore::new();
        // Insert out of order; the sink must see ascending seq.
        fill(&store, &[seq(3, 0, 1), seq(1, 0, 1), seq(2, 0, 1)]);
        let mut sink = RecordingSink::new();
        // `LeaderSink` PART 8: no watermark, no floor — drain the whole store.
        let n = Committer::run(&store, &mut sink).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
        );
    }

    #[test]
    fn run_drains_single_idle_intent() {
        // The B3 case: a single pending intent on an otherwise-idle shard MUST
        // drain (the old exclusive watermark never drained the latest/only one).
        let store = InMemIntentStore::new();
        fill(&store, &[seq(7, 0, 1)]);
        let mut sink = RecordingSink::new();
        let n = Committer::run(&store, &mut sink).unwrap();
        assert_eq!(n, 1);
        assert_eq!(sink.incorporated, vec![seq(7, 0, 1)]);
    }

    #[test]
    fn run_does_not_prune_store_pruning_is_off_band() {
        // PART 8 §T — `Committer::run` no longer prunes; the supervisor does
        // per-intent prune off-band. The store retains its entries until the
        // supervisor's prune runs.
        let store = InMemIntentStore::new();
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        let mut sink = RecordingSink::new();
        let n = Committer::run(&store, &mut sink).unwrap();
        assert_eq!(n, 3);
        // Store still holds all three — supervisor will per-intent prune them.
        assert_eq!(store.pending_len().unwrap(), 3);
    }

    #[test]
    fn run_empty_store_returns_zero() {
        let store = InMemIntentStore::new();
        let mut sink = RecordingSink::new();
        assert_eq!(Committer::run(&store, &mut sink).unwrap(), 0);
        assert!(sink.incorporated.is_empty());
    }

    /// PART 8 §U — drain-all caps each pass at `DRAIN_BATCH_CAP`. A store
    /// with 2500 entries splits into 3 batches (1000 + 1000 + 500).
    #[test]
    fn drain_all_batches_at_cap() {
        let store = InMemIntentStore::new();
        // Seed 2500 ascending seqs. Use distinct physical_ms so they sort
        // unambiguously.
        let seqs: Vec<PerspectiveSeq> = (1..=2500u64).map(|i| seq(i, 0, 1)).collect();
        fill(&store, &seqs);

        let mut sink = RecordingSink::new();
        let n = Committer::run(&store, &mut sink).unwrap();
        assert_eq!(n, 2500);
        assert_eq!(
            sink.batch_starts.len(),
            3,
            "drain must split into exactly 3 batches at cap 1000",
        );
        assert_eq!(sink.batch_starts, vec![0, 1000, 2000]);
        assert_eq!(sink.incorporated.len(), 2500);
    }

    // ---- recover_pending (election intent-recovery / gate-1 O2) -----------

    /// Collect the recovered perspective-seqs (ascending) for a gather.
    fn recovered_seqs(
        stores: &[&dyn IntentStore],
        cluster_size: usize,
        min_acks: usize,
    ) -> Vec<PerspectiveSeq> {
        recover_pending(stores, cluster_size, min_acks)
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect()
    }

    #[test]
    fn recover_o2_overlap_every_gather_recovers_acked_intent() {
        // The core O2 property. RF=3, min_acks=2, threshold = 2. An intent `I`
        // is acked on the durability quorum {A,B} only. For EVERY 2-of-3 gather
        // the new leader could form — {A,B}, {A,C}, {B,C} — recover_pending must
        // include `I` (any 2-of-3 gather intersects the 2-of-3 durability set).
        let a = InMemIntentStore::new();
        let b = InMemIntentStore::new();
        let c = InMemIntentStore::new();
        let acked = seq(7, 0, 1);
        assert_eq!(a.put(intent(acked, None)).unwrap(), PutOutcome::Recorded);
        assert_eq!(b.put(intent(acked, None)).unwrap(), PutOutcome::Recorded);

        let ar: &dyn IntentStore = &a;
        let br: &dyn IntentStore = &b;
        let cr: &dyn IntentStore = &c;

        for (label, gather) in [
            ("{A,B}", [ar, br]),
            ("{A,C}", [ar, cr]),
            ("{B,C}", [br, cr]),
        ] {
            let recovered = recovered_seqs(&gather, 3, 2);
            assert!(
                recovered.contains(&acked),
                "gather {label} must recover acked intent I"
            );
        }
    }

    #[test]
    fn recover_threshold_rf6_min2_needs_five_views() {
        // MF-7: RF=6, min_acks=2 → threshold 5. A 5-view gather succeeds; a
        // 4-view gather is InsufficientQuorum (the bug majority(6)=4 would have
        // allowed — a 4-of-6 gather can miss a 2-of-6 acked intent).
        let stores: Vec<InMemIntentStore> = (0..6).map(|_| InMemIntentStore::new()).collect();
        let refs: Vec<&dyn IntentStore> = stores.iter().map(|s| s as &dyn IntentStore).collect();

        // 5 views → Ok.
        assert!(recover_pending(&refs[..5], 6, 2).is_ok());

        // 4 views → InsufficientQuorum { have: 4, need: 5 }.
        match recover_pending(&refs[..4], 6, 2) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 4);
                assert_eq!(need, 5);
            }
            other => panic!("expected InsufficientQuorum {{have:4,need:5}}, got {other:?}"),
        }
    }

    #[test]
    fn recover_dedups_same_intent_in_two_stores() {
        // The same intent in two gathered stores appears once in the union.
        let a = InMemIntentStore::new();
        let b = InMemIntentStore::new();
        let s = seq(3, 0, 1);
        a.put(intent(s, None)).unwrap();
        b.put(intent(s, None)).unwrap();
        let ar: &dyn IntentStore = &a;
        let br: &dyn IntentStore = &b;
        let recovered = recover_pending(&[ar, br], 3, 2).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].perspective_seq, s);
    }

    #[test]
    fn recover_unions_ascending() {
        // Disjoint, out-of-order seqs union into one ascending sequence.
        let a = InMemIntentStore::new();
        let b = InMemIntentStore::new();
        fill(&a, &[seq(5, 0, 1), seq(1, 0, 1)]);
        fill(&b, &[seq(3, 0, 1), seq(2, 0, 1)]);
        let ar: &dyn IntentStore = &a;
        let br: &dyn IntentStore = &b;
        assert_eq!(
            recovered_seqs(&[ar, br], 3, 2),
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1), seq(5, 0, 1)]
        );
    }

    #[test]
    fn recover_insufficient_quorum_below_threshold() {
        // RF=3/min_acks=2 → threshold 2. 1 view < 2 → InsufficientQuorum.
        let a = InMemIntentStore::new();
        let ar: &dyn IntentStore = &a;
        match recover_pending(&[ar], 3, 2) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 1);
                assert_eq!(need, 2);
            }
            other => panic!("expected InsufficientQuorum, got {other:?}"),
        }
    }

    #[test]
    fn restore_into_is_idempotent() {
        let recovered = vec![
            intent(seq(1, 0, 1), None),
            intent(seq(2, 0, 1), None),
            intent(seq(3, 0, 1), None),
        ];
        let target = InMemIntentStore::new();
        let first = restore_into(&target, &recovered).unwrap();
        assert_eq!(first, 3, "all three newly inserted on first restore");
        assert_eq!(target.pending_len().unwrap(), 3);

        let second = restore_into(&target, &recovered).unwrap();
        assert_eq!(second, 0, "re-restore inserts nothing new (idempotent)");
        assert_eq!(target.pending_len().unwrap(), 3);
        let pending: Vec<_> = target
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(pending, vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
    }

    #[test]
    fn recover_then_drain_incorporates_all() {
        // PART 8 — recovery unions seqs across peers; drain sends ALL of them
        // to the sink. Dedup against the log happens at the SM apply gate
        // (recent_incorporated + ancient cutoff), not in the committer.
        let a = InMemIntentStore::new();
        let b = InMemIntentStore::new();
        fill(&a, &[seq(1, 0, 1), seq(3, 0, 1)]);
        fill(&b, &[seq(2, 0, 1), seq(3, 0, 1)]);
        let ar: &dyn IntentStore = &a;
        let br: &dyn IntentStore = &b;

        let recovered = recover_pending(&[ar, br], 3, 2).unwrap();
        assert_eq!(
            recovered
                .iter()
                .map(|i| i.perspective_seq)
                .collect::<Vec<_>>(),
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)],
        );

        let leader = InMemIntentStore::new();
        assert_eq!(restore_into(&leader, &recovered).unwrap(), 3);

        let mut sink = RecordingSink::new();
        let n = Committer::run(&leader, &mut sink).unwrap();
        assert_eq!(
            n, 3,
            "drain incorporates all pending; SM dedup is authoritative"
        );
        assert_eq!(
            sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
        );
    }
}
