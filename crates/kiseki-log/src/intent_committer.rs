//! ADR-047 §3 — the async committer: the majority stability watermark, the
//! seam to the Raft log, and the incorporation algorithm.
//!
//! Phases 1–2 ([`crate::intent`]) own the perspective sequence, the durable
//! intent record, and the per-replica [`IntentStore`]. This module is the
//! **consensus logic** that drains those intents into the Raft log *after*
//! the client has already been acked (the ADR-047 decoupling):
//!
//! 1. [`compute_stability_watermark`] — the exclusive upper bound `W` below
//!    which a *majority* of replicas have closed (gate-1 **F-1**: a single
//!    laggard with the smallest `next_pending` is ignored, so it never stalls
//!    the committer).
//! 2. [`IncorporationSink`] — the abstract seam to the real Raft log; its
//!    `max_incorporated_seq` is the **source of truth** for what has already
//!    been applied. The fjall/Raft-backed implementation lands in phase 5.
//! 3. [`Committer::run`] — selects the pending intents below `W` that are not
//!    already in the log (gate-1 **F-2**: the `seq > max_incorporated`
//!    re-incorporation guard), appends them as one ordered batch, then prunes.
//!
//! This module is **additive and UNWIRED**: it touches neither the Raft state
//! machine, the synchronous write path, nor any runtime wiring. The real log
//! sink and the gossip that gathers peer `next_pending_seq` values arrive in
//! later phases.

use crate::intent::{IntentError, IntentStore, PerspectiveSeq, WriteIntent};

/// Majority threshold for a cluster of `cluster_size` replicas: a strict
/// majority, `cluster_size / 2 + 1`.
#[must_use]
fn majority(cluster_size: usize) -> usize {
    cluster_size / 2 + 1
}

/// The result of [`compute_stability_watermark`] — an explicit 3-state so the
/// committer never conflates "no majority closure" (apply nothing) with "a
/// majority has fully closed" (apply everything). The earlier `Option` form
/// collapsed both to `None`, which `run` then treated as *no upper bound* —
/// applying unstable intents under a sub-majority gather (the phase-3 gate
/// finding). These are now distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Watermark {
    /// A majority has closed everything below this seq — incorporate intents
    /// with `seq < W`.
    UpTo(PerspectiveSeq),
    /// A majority has nothing pending at all — no upper bound; incorporate
    /// every gathered intent above the log high-water-mark.
    FullyClosed,
    /// Fewer than a majority of the *membership* have closed a common point —
    /// nothing is stable; incorporate nothing this pass.
    NotStable,
}

/// Compute the majority stability watermark (ADR-047 §3 / gate-1 F-1 + the
/// phase-3 partial-gather fix).
///
/// Each entry of `next_pendings` is one replica's
/// [`IntentStore::next_pending_seq`]: `Some(s)` = nothing pending below `s`
/// (closed `< s`); `None` = nothing pending at all (closed up to +∞).
/// **Non-reporting** members are NOT in the slice — they are padded in as a
/// conservative [`Bound::Unknown`] (sorts below every real report), so a
/// partial gather can never set `W` higher than the full membership permits
/// (a non-reporter might still hold a low pending intent on a `min_acks`
/// quorum that overlaps the majority the leader gathered from).
///
/// `W` is the highest value a **majority** (`cluster_size / 2 + 1`) of the
/// *membership* has closed below: pad to `cluster_size` with `Unknown`, sort
/// ascending, take the element at index `cluster_size - majority` (it has
/// exactly `majority` elements at-or-after it). `Unknown` there → fewer than a
/// majority reported → [`Watermark::NotStable`]; a real seq → [`Watermark::UpTo`];
/// the +∞ sentinel → [`Watermark::FullyClosed`]. The single-laggard case (F-1)
/// still holds — a lone low report sits near the front, excluded from the
/// majority position.
#[must_use]
pub fn compute_stability_watermark(
    next_pendings: &[Option<PerspectiveSeq>],
    cluster_size: usize,
) -> Watermark {
    if cluster_size == 0 {
        return Watermark::NotStable;
    }
    let maj = majority(cluster_size);

    // Map each report to a sortable lower bound. `None` (nothing pending)
    // closes to +∞ (the `Inf` sentinel); `Some(s)` closes to `s`.
    let mut bounds: Vec<Bound> = next_pendings
        .iter()
        .map(|p| p.map_or(Bound::Inf, Bound::Seq))
        .collect();
    // Pad non-reporting members with `Unknown` (sorts first) so the majority
    // index reflects the whole membership, not just who answered. Never
    // truncate (a longer-than-membership slice only makes `W` more
    // conservative, never less safe).
    if bounds.len() < cluster_size {
        bounds.resize(cluster_size, Bound::Unknown);
    }
    bounds.sort();

    // The element with exactly `maj` members at-or-after it (it and every
    // larger element). `idx` is always in range: `1 <= maj <= cluster_size`,
    // so `0 <= idx < cluster_size <= bounds.len()`.
    let idx = cluster_size - maj;
    match bounds.get(idx) {
        Some(Bound::Seq(s)) => Watermark::UpTo(*s),
        Some(Bound::Inf) => Watermark::FullyClosed,
        // `Unknown` at the majority position → sub-majority reported.
        Some(Bound::Unknown) | None => Watermark::NotStable,
    }
}

/// A per-replica closure bound for watermark computation, ordered
/// `Unknown < Seq(_) < Inf`. `Unknown` = a member that did not report (padded
/// in — treated as possibly holding an arbitrarily-low pending intent, the
/// conservative assumption); `Seq(s)` = closed everything below `s`; `Inf` =
/// closed everything (nothing pending). The derived `Ord` follows the variant
/// declaration order, then the inner seq for `Seq` — exactly the
/// `-∞ < s < +∞` semantics the watermark needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Bound {
    Unknown,
    Seq(PerspectiveSeq),
    Inf,
}

/// The seam between the async committer and the Raft log (ADR-047 §3).
///
/// The committer is pure algorithm; this trait is where its output lands in
/// the real, replicated log. The Raft-backed implementation arrives in phase 5
/// — until then [`RecordingSink`] stands in for tests.
///
/// **The log is the source of truth.** [`Self::max_incorporated_seq`] is read
/// from the log's recorded incorporated seqs on recovery; the committer uses
/// it (not the prune state of the [`IntentStore`]) to decide what is already
/// applied. This is the gate-1 **F-2** contract: a re-gathered or replayed
/// intent at or below the log's high-water-mark is dropped, never re-applied.
pub trait IncorporationSink {
    /// Highest perspective-seq already incorporated into the Raft log, read
    /// from the log's recorded incorporated seqs on recovery. `None` if the
    /// log has incorporated no intents yet.
    fn max_incorporated_seq(&self) -> Option<PerspectiveSeq>;

    /// Append `ordered` (already ascending by perspective-seq, every element
    /// strictly greater than [`Self::max_incorporated_seq`]) to the Raft log
    /// as one ordered batch, recording their seqs so a later
    /// `max_incorporated_seq` reflects them.
    ///
    /// # Errors
    /// Backing-store / Raft-append failure.
    fn incorporate(&mut self, ordered: &[WriteIntent]) -> Result<(), IntentError>;
}

/// In-memory [`IncorporationSink`] for tests: it remembers every incorporated
/// intent and advances `max_incorporated_seq` to the highest seq it has seen.
#[derive(Default)]
pub struct RecordingSink {
    /// Every incorporated seq, in incorporation order (one entry per intent).
    pub incorporated: Vec<PerspectiveSeq>,
    /// Every incorporated intent, in incorporation order.
    pub intents: Vec<WriteIntent>,
    /// Running maximum incorporated seq (the log's high-water-mark).
    max: Option<PerspectiveSeq>,
}

impl RecordingSink {
    /// Empty sink — nothing incorporated yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the high-water-mark as if `seq` had already been incorporated by a
    /// prior committer run, without recording any intents — used by tests to
    /// simulate recovery against an already-populated log (the F-2 guard).
    #[must_use]
    pub fn with_max_incorporated(mut self, seq: PerspectiveSeq) -> Self {
        self.max = Some(seq);
        self
    }
}

impl IncorporationSink for RecordingSink {
    fn max_incorporated_seq(&self) -> Option<PerspectiveSeq> {
        self.max
    }

    fn incorporate(&mut self, ordered: &[WriteIntent]) -> Result<(), IntentError> {
        for intent in ordered {
            self.incorporated.push(intent.perspective_seq);
            self.max = Some(match self.max {
                Some(m) => m.max(intent.perspective_seq),
                None => intent.perspective_seq,
            });
            self.intents.push(intent.clone());
        }
        Ok(())
    }
}

/// The async committer (ADR-047 §3): drains an [`IntentStore`] into the Raft
/// log behind an [`IncorporationSink`], up to the majority stability
/// watermark, idempotently.
pub struct Committer;

impl Committer {
    /// Run one committer pass.
    ///
    /// 1. Compute the majority stability watermark `W` from `next_pendings`
    ///    (this replica's report plus the peer reports gathered by gossip —
    ///    gossip itself is phase 5) and `cluster_size`.
    /// 2. Read the log's `max_incorporated_seq` — the **source of truth** for
    ///    what is already applied.
    /// 3. From the store's pending intents, select those with
    ///    `seq > max_incorporated` (the F-2 re-incorporation guard) **and**
    ///    (`W` is `None` — no upper bound — OR `seq < W`).
    /// 4. Sort the selection ascending by perspective-seq, append it as one
    ///    ordered batch via [`IncorporationSink::incorporate`], then
    ///    [`IntentStore::prune`] up to the last selected seq (inclusive).
    ///
    /// Pruning is **advisory**: the log's `max_incorporated_seq` is the truth,
    /// so a pruned-but-unrecorded intent (crash between incorporate and prune)
    /// is harmless — the next run re-gathers it and the `seq > max_incorporated`
    /// filter drops it. We never re-apply based on prune state alone.
    ///
    /// # Precondition (phase-5 wiring obligation — gate-1 F-P3-2)
    /// `store` must already hold every intent with `seq < W` for this replica:
    /// the leader's local intent store is completed below the watermark by the
    /// quorum-write + gather. This algorithm reads only the local `store`,
    /// never peers' intents, so it trusts that completeness. The watermark's
    /// majority semantics guarantee any durable intent `< W` is on a `min_acks`
    /// quorum overlapping the majority the leader gathered from — so a complete
    /// leader has it — but the *gather that populates the local store* is the
    /// wiring's job, not this function's.
    ///
    /// # Returns
    /// The number of intents incorporated (`0` if none were selectable).
    ///
    /// # Errors
    /// Propagates [`IntentError`] from [`IntentStore::pending`],
    /// [`IncorporationSink::incorporate`], or [`IntentStore::prune`].
    pub fn run(
        store: &dyn IntentStore,
        next_pendings: &[Option<PerspectiveSeq>],
        cluster_size: usize,
        sink: &mut dyn IncorporationSink,
    ) -> Result<usize, IntentError> {
        let watermark = compute_stability_watermark(next_pendings, cluster_size);
        // No majority has closed a common point → nothing is stable; this pass
        // incorporates nothing (the phase-3 fix — never apply under a
        // sub-majority gather).
        if watermark == Watermark::NotStable {
            return Ok(0);
        }
        let max_inc = sink.max_incorporated_seq();

        let mut selected: Vec<WriteIntent> = store
            .pending()?
            .into_iter()
            .filter(|intent| {
                let seq = intent.perspective_seq;
                // F-2: drop anything already in the log (re-gathered / replayed).
                let above_floor = match max_inc {
                    Some(m) => seq > m,
                    None => true,
                };
                // `FullyClosed` = a majority has nothing pending → no upper
                // bound. `NotStable` was handled by the early return above.
                let below_watermark = match watermark {
                    Watermark::UpTo(w) => seq < w,
                    Watermark::FullyClosed => true,
                    Watermark::NotStable => false,
                };
                above_floor && below_watermark
            })
            .collect();

        if selected.is_empty() {
            return Ok(0);
        }

        // Perspective-seq ascending: the order the log must apply them.
        selected.sort_by_key(|intent| intent.perspective_seq);

        sink.incorporate(&selected)?;

        // `prune` is `<=` inclusive (phase 2), so prune up to the last
        // selected seq. Advisory only — see the run-level docs.
        if let Some(last) = selected.last() {
            store.prune(last.perspective_seq)?;
        }

        Ok(selected.len())
    }
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

    // ---- compute_stability_watermark --------------------------------------

    #[test]
    fn watermark_n1_single_replica_is_its_own_report() {
        // n=1, maj=1, idx=0. A lone replica's report is the watermark.
        assert_eq!(
            compute_stability_watermark(&[Some(seq(5, 0, 1))], 1),
            Watermark::UpTo(seq(5, 0, 1))
        );
        // Nothing pending -> fully closed.
        assert_eq!(
            compute_stability_watermark(&[None], 1),
            Watermark::FullyClosed
        );
    }

    #[test]
    fn watermark_n3_is_the_median() {
        // n=3, maj=2, idx=1. Sorted [1,2,3] -> element 1 = seq(2).
        let reports = [Some(seq(1, 0, 1)), Some(seq(3, 0, 1)), Some(seq(2, 0, 1))];
        assert_eq!(
            compute_stability_watermark(&reports, 3),
            Watermark::UpTo(seq(2, 0, 1))
        );
    }

    #[test]
    fn watermark_n3_minority_laggard_does_not_drag_w_down() {
        // The F-1 fix: a single very-low laggard sits at idx 0 and is ignored.
        // Two replicas have closed everything below seq(100); the laggard at
        // seq(1) must NOT pin the watermark to 1.
        let reports = [
            Some(seq(1, 0, 1)), // laggard — smallest next_pending
            Some(seq(100, 0, 2)),
            Some(seq(100, 0, 3)),
        ];
        // Sorted [1, 100@2, 100@3] -> idx 1 = seq(100,0,2).
        assert_eq!(
            compute_stability_watermark(&reports, 3),
            Watermark::UpTo(seq(100, 0, 2))
        );
    }

    #[test]
    fn watermark_sub_majority_reports_not_stable() {
        // n=3, maj=2: only one of three reports. Padding the two silent members
        // with `Unknown` puts an `Unknown` at the majority index -> NotStable
        // (the phase-3 fix: never advance under a sub-majority gather).
        assert_eq!(
            compute_stability_watermark(&[Some(seq(5, 0, 1))], 3),
            Watermark::NotStable
        );
        assert_eq!(compute_stability_watermark(&[], 3), Watermark::NotStable);
    }

    #[test]
    fn watermark_partial_gather_pads_unknown_and_is_conservative() {
        // n=5, maj=3, idx=2, but only 4 members reported (one silent). The
        // silent member pads as `Unknown` (sorts first); sorted
        // [Unknown, 10, 20, 30, 40] -> idx 2 = seq(20). A naive impl over the
        // 4-element slice would pick idx 2 = seq(30) — too high, applying
        // intents only 2 of 5 have closed below. The pad keeps W safe.
        let reports = [
            Some(seq(10, 0, 1)),
            Some(seq(40, 0, 4)),
            Some(seq(20, 0, 2)),
            Some(seq(30, 0, 3)),
        ];
        assert_eq!(
            compute_stability_watermark(&reports, 5),
            Watermark::UpTo(seq(20, 0, 2))
        );
    }

    #[test]
    fn watermark_all_none_is_fully_closed() {
        // Every replica has nothing pending: a majority has closed +inf.
        assert_eq!(
            compute_stability_watermark(&[None, None, None], 3),
            Watermark::FullyClosed
        );
        assert_eq!(
            compute_stability_watermark(&[None], 1),
            Watermark::FullyClosed
        );
    }

    #[test]
    fn watermark_majority_none_with_minority_seq() {
        // n=3, maj=2: two replicas fully closed (None=+inf), one still pending.
        // Sorted [Seq(2), Inf, Inf] -> idx 1 = Inf -> FullyClosed (the single
        // pending laggard does not cap the majority).
        let reports = [Some(seq(2, 0, 1)), None, None];
        assert_eq!(
            compute_stability_watermark(&reports, 3),
            Watermark::FullyClosed
        );
    }

    #[test]
    fn watermark_n5_is_the_median() {
        // n=5, maj=3, idx=2. Sorted [1,2,3,4,5] -> element 2 = seq(3).
        let reports = [
            Some(seq(5, 0, 1)),
            Some(seq(1, 0, 1)),
            Some(seq(3, 0, 1)),
            Some(seq(2, 0, 1)),
            Some(seq(4, 0, 1)),
        ];
        assert_eq!(
            compute_stability_watermark(&reports, 5),
            Watermark::UpTo(seq(3, 0, 1))
        );
    }

    #[test]
    fn watermark_n5_two_laggards_ignored() {
        // n=5, maj=3, idx=2: two laggards at idx 0,1 are ignored; the third-
        // lowest (the majority boundary) sets W.
        let reports = [
            Some(seq(1, 0, 1)),
            Some(seq(2, 0, 1)),
            Some(seq(100, 0, 3)),
            Some(seq(100, 0, 4)),
            Some(seq(100, 0, 5)),
        ];
        assert_eq!(
            compute_stability_watermark(&reports, 5),
            Watermark::UpTo(seq(100, 0, 3))
        );
    }

    #[test]
    fn watermark_n6_even_boundary() {
        // n=6, maj=4, idx=2. Element at idx 2 has 4 reports at-or-after it.
        // Sorted [1,2,3,4,5,6] -> element 2 = seq(3).
        let reports = [
            Some(seq(6, 0, 1)),
            Some(seq(3, 0, 1)),
            Some(seq(1, 0, 1)),
            Some(seq(5, 0, 1)),
            Some(seq(2, 0, 1)),
            Some(seq(4, 0, 1)),
        ];
        assert_eq!(
            compute_stability_watermark(&reports, 6),
            Watermark::UpTo(seq(3, 0, 1))
        );
    }

    // ---- Committer::run ---------------------------------------------------

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s, None)).unwrap(), PutOutcome::Recorded);
        }
    }

    #[test]
    fn run_applies_pending_in_ascending_order() {
        let store = InMemIntentStore::new();
        // Insert out of order; the sink must see ascending seq.
        fill(&store, &[seq(3, 0, 1), seq(1, 0, 1), seq(2, 0, 1)]);
        let mut sink = RecordingSink::new();
        // All three replicas fully closed -> None watermark -> no upper bound.
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
        );
        // Store drained.
        assert_eq!(store.pending_len().unwrap(), 0);
    }

    #[test]
    fn run_applies_only_below_watermark() {
        let store = InMemIntentStore::new();
        fill(
            &store,
            &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1), seq(4, 0, 1)],
        );
        let mut sink = RecordingSink::new();
        // Majority watermark = seq(3): only seq(1) and seq(2) are < W.
        let reports = [Some(seq(3, 0, 1)), Some(seq(3, 0, 2)), Some(seq(1, 0, 9))];
        // Sorted [1@9, 3@1, 3@2] -> idx 1 = seq(3,0,1).
        assert_eq!(
            compute_stability_watermark(&reports, 3),
            Watermark::UpTo(seq(3, 0, 1))
        );
        let n = Committer::run(&store, &reports, 3, &mut sink).unwrap();
        assert_eq!(n, 2);
        assert_eq!(sink.incorporated, vec![seq(1, 0, 1), seq(2, 0, 1)]);
        // seq(3) and seq(4) stay pending (>= W).
        let remaining: Vec<_> = store
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(remaining, vec![seq(3, 0, 1), seq(4, 0, 1)]);
    }

    #[test]
    fn run_prunes_incorporated_intents() {
        let store = InMemIntentStore::new();
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        let mut sink = RecordingSink::new();
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(n, 3);
        // All incorporated and pruned -> next_pending is None.
        assert_eq!(store.next_pending_seq().unwrap(), None);
        assert_eq!(store.pending_len().unwrap(), 0);
    }

    #[test]
    fn run_empty_store_returns_zero() {
        let store = InMemIntentStore::new();
        let mut sink = RecordingSink::new();
        assert_eq!(
            Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap(),
            0
        );
        assert!(sink.incorporated.is_empty());
    }

    #[test]
    fn run_sub_majority_gather_applies_nothing() {
        // The phase-3 fix: a sub-majority gather (1 of 3 reported) computes
        // `NotStable` (the two silent members pad as `Unknown`), so the
        // committer applies NOTHING and prunes nothing — the intents wait for a
        // later, quorate run. (The pre-fix code treated the sub-majority `None`
        // as "no upper bound" and drained everything — the unsafe behavior this
        // gate fixed.)
        let store = InMemIntentStore::new();
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
        let mut sink = RecordingSink::new();
        let n = Committer::run(&store, &[Some(seq(9, 0, 1))], 3, &mut sink).unwrap();
        assert_eq!(n, 0);
        assert!(sink.incorporated.is_empty());
        assert_eq!(store.pending_len().unwrap(), 2);
    }

    #[test]
    fn run_is_idempotent_across_runs() {
        // F-2: a second run with the same max_incorporated does NOT re-apply.
        let store = InMemIntentStore::new();
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        let mut sink = RecordingSink::new();
        let first = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(first, 3);
        // Store is drained AND the sink's high-water-mark is seq(3). A second
        // run finds nothing pending -> 0, and even if it did, the floor guard
        // would drop anything <= seq(3).
        let second = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(second, 0);
        // Sink saw each intent exactly once.
        assert_eq!(
            sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
        );
    }

    #[test]
    fn run_skips_intent_at_or_below_log_high_water_mark() {
        // F-2 mechanism: an intent present in the store whose seq <=
        // sink.max_incorporated_seq() is DROPPED, not re-applied — simulating
        // a re-gathered / replayed intent the log already has. The log
        // (max_incorporated), not the prune state, is the source of truth.
        let store = InMemIntentStore::new();
        // The store still holds seq(2) and seq(3) (e.g. a crash before prune,
        // or a re-gathered intent), but the log already incorporated up to
        // seq(2).
        fill(&store, &[seq(2, 0, 1), seq(3, 0, 1)]);
        let mut sink = RecordingSink::new().with_max_incorporated(seq(2, 0, 1));
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        // Only seq(3) is above the floor; seq(2) is dropped, not re-applied.
        assert_eq!(n, 1);
        assert_eq!(sink.incorporated, vec![seq(3, 0, 1)]);
        // The store is pruned up to seq(3), clearing the stale seq(2) too.
        assert_eq!(store.pending_len().unwrap(), 0);
    }

    #[test]
    fn run_floor_equal_to_seq_is_excluded() {
        // The floor filter is strict (`seq > max_inc`): an intent whose seq
        // exactly equals the high-water-mark is already in the log -> skipped.
        let store = InMemIntentStore::new();
        fill(&store, &[seq(5, 0, 1)]);
        let mut sink = RecordingSink::new().with_max_incorporated(seq(5, 0, 1));
        let n = Committer::run(&store, &[None, None, None], 3, &mut sink).unwrap();
        assert_eq!(n, 0);
        assert!(sink.incorporated.is_empty());
    }

    #[test]
    fn run_watermark_and_floor_compose() {
        // Both filters active: floor at seq(2), watermark at seq(5). Only
        // seq(3) and seq(4) qualify (> 2 AND < 5).
        let store = InMemIntentStore::new();
        fill(
            &store,
            &[
                seq(1, 0, 1),
                seq(2, 0, 1),
                seq(3, 0, 1),
                seq(4, 0, 1),
                seq(5, 0, 1),
            ],
        );
        let mut sink = RecordingSink::new().with_max_incorporated(seq(2, 0, 1));
        // Watermark seq(5): reports sorted [5@1, 5@2, 9@3] -> idx 1 = seq(5,0,2)?
        // Keep it simple — force W = seq(5,0,1) via two reports at 5 and a high one.
        let reports = [Some(seq(5, 0, 1)), Some(seq(5, 0, 1)), Some(seq(9, 0, 1))];
        assert_eq!(
            compute_stability_watermark(&reports, 3),
            Watermark::UpTo(seq(5, 0, 1))
        );
        let n = Committer::run(&store, &reports, 3, &mut sink).unwrap();
        assert_eq!(n, 2);
        assert_eq!(sink.incorporated, vec![seq(3, 0, 1), seq(4, 0, 1)]);
    }
}
