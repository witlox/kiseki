//! ADR-047 phase 5b-core — the per-shard committer driver.
//!
//! Phases 3/4/5a built the pieces this module composes:
//!
//! - [`crate::intent`] — the durable [`IntentStore`] (`pending`,
//!   `next_pending_seq`, `prune`) and [`WriteIntent`] / [`PerspectiveSeq`].
//! - [`crate::intent_committer`] — the pure consensus logic:
//!   [`Committer::run`], [`compute_stability_watermark`], plus election
//!   recovery ([`recover_pending`] / [`restore_into`]) and the synchronous
//!   [`IncorporationSink`] seam.
//! - [`crate::raft_intent_sink`] — the live Raft-log [`IncorporationSink`],
//!   which bridges to the async log via `Handle::block_on` and so MUST be
//!   driven off a dedicated thread, never a tokio worker.
//!
//! What this module adds is the **driver** that ties them together for one
//! shard, behind an abstract [`PeerIntentGatherer`] seam: gather the peers'
//! `next_pending_seq` reports over the wire, feed them (with the local
//! report) into [`Committer::run`], and prune. The concrete `IntentSync`
//! gRPC gatherer is phase 5b-rpc; the producer that fans intents to the
//! quorum is phase 5c.
//!
//! This module is **additive and UNWIRED**: it spawns nothing in production,
//! touches neither the gateway, the synchronous write path, nor any
//! startup/spawn wiring. The steady-state loop ([`run_committer_loop`]) is
//! defined here but only spawned behind the `DecoupledAckEnabled` capability
//! gate in phase 5c/5d.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kiseki_common::ids::NodeId;

use crate::intent::{IntentError, IntentStore, PerspectiveSeq, WriteIntent};
use crate::intent_committer::{recover_pending, restore_into, Committer, IncorporationSink};

/// The per-shard committer's view of its peer replicas' intent stores,
/// gathered over the wire (ADR-047 §3 / phase 5b). The concrete gRPC
/// `IntentSync` implementation is phase 5b-rpc; tests use a fake.
///
/// Dispatch is **generic** (`G: PeerIntentGatherer`), not `dyn` — the
/// `async fn`s have no object-safe form and none is needed (mirrors
/// [`crate::raft_intent_sink::IntentLogAppender`]).
#[allow(async_fn_in_trait)]
pub trait PeerIntentGatherer: Send + Sync {
    /// Each *reachable* peer's [`IntentStore::next_pending_seq`] for this
    /// shard, **keyed by the reporting peer's [`NodeId`]** — one entry per peer
    /// that answered, NOT the local node (the driver prepends its own report).
    /// Non-reporting peers are simply absent;
    /// [`compute_stability_watermark`](crate::intent_committer::compute_stability_watermark)
    /// pads them as conservative `Unknown`, so a partial gather can only lower
    /// the watermark, never raise it past what the full membership permits.
    ///
    /// The result is keyed by `NodeId` because the driver **dedups by node**
    /// before counting: the safety property below is enforced in
    /// [`ShardCommitter`], not trusted from the gatherer.
    ///
    /// # Safety (gate-1 — why this is node-keyed)
    /// A duplicate report from the same peer (a stale connection, a retry, a
    /// membership glitch) must NOT inflate the membership count: over-counting
    /// a single peer as two members fabricates a majority that does not exist,
    /// which would let [`ShardCommitter::tick`] raise the watermark past what a
    /// true majority has closed (premature / out-of-order incorporation — an
    /// I-CS1 violation). The driver collapses duplicate `NodeId`s to one.
    ///
    /// # Errors
    /// Transport / peer-store failure rendered into [`IntentError`].
    async fn gather_next_pending_seqs(
        &self,
    ) -> Result<Vec<(NodeId, Option<PerspectiveSeq>)>, IntentError>;

    /// Each peer's full pending set, for election intent-recovery (gate-1
    /// **O2**), **keyed by the peer's [`NodeId`]** — one entry per *reachable*
    /// peer, NOT the local node. The driver adds the local store, **dedups by
    /// node**, and requires a majority of *distinct* nodes before unioning:
    /// only a majority gather is guaranteed to overlap every acked intent's
    /// `min_acks` durability quorum, and counting the same peer twice toward
    /// that majority could miss an acked intent (silent intent loss). Keying by
    /// `NodeId` lets the driver enforce distinctness rather than trust it.
    ///
    /// # Errors
    /// Transport / peer-store failure rendered into [`IntentError`].
    async fn gather_pending(&self) -> Result<Vec<(NodeId, Vec<WriteIntent>)>, IntentError>;
}

/// The per-shard committer driver (ADR-047 phase 5b-core).
///
/// Owns the local [`IntentStore`], a [`IncorporationSink`] (the Raft-log
/// bridge in production, a recording fake in tests), and the cluster size.
/// Its decision methods are **synchronous** so that the sink's
/// `Handle::block_on` (in [`crate::raft_intent_sink::RaftLogIncorporationSink`])
/// is safe on the driver's own dedicated thread — see
/// [`run_committer_loop`]'s threading contract.
pub struct ShardCommitter<S: IncorporationSink> {
    local_store: Arc<dyn IntentStore>,
    sink: S,
    cluster_size: usize,
}

impl<S: IncorporationSink> ShardCommitter<S> {
    /// Build a driver over the local store, sink, and cluster size.
    #[must_use]
    pub fn new(local_store: Arc<dyn IntentStore>, sink: S, cluster_size: usize) -> Self {
        Self {
            local_store,
            sink,
            cluster_size,
        }
    }

    /// Run one steady-state committer pass against the peers' gathered
    /// `next_pending_seq` reports.
    ///
    /// Builds the full `next_pendings` slice = this replica's
    /// [`IntentStore::next_pending_seq`] followed by the peers' reports
    /// (**deduped by [`NodeId`]** — see the safety note on
    /// [`PeerIntentGatherer::gather_next_pending_seqs`]), then delegates to
    /// [`Committer::run`]. Passing fewer than `cluster_size` distinct reports is
    /// correct: `compute_stability_watermark` pads the absent (non-reporting)
    /// members as conservative `Unknown`, so a sub-majority gather computes
    /// `NotStable` and incorporates nothing.
    ///
    /// # Returns
    /// The number of intents incorporated into the log this pass (`0` if the
    /// gather was sub-majority or nothing was below the watermark).
    ///
    /// # Errors
    /// Propagates [`IntentError`] from [`IntentStore::next_pending_seq`],
    /// [`IntentStore::pending`], [`IncorporationSink::incorporate`], or
    /// [`IntentStore::prune`] (all via [`Committer::run`]).
    pub fn tick(
        &mut self,
        peer_reports: &[(NodeId, Option<PerspectiveSeq>)],
    ) -> Result<usize, IntentError> {
        // Collapse duplicate peer node-ids to one report so a peer reported
        // twice cannot fabricate a majority (the gate-1 safety property). A
        // node reporting more than once is expected to report the same value;
        // last write wins on the rare disagreement.
        let mut by_node: HashMap<NodeId, Option<PerspectiveSeq>> = HashMap::new();
        for (node, report) in peer_reports {
            by_node.insert(*node, *report);
        }

        // The full membership report: local first, then the distinct peers that
        // answered. The local store's own low-water-mark is this replica's
        // contribution to the majority watermark.
        let mut next_pendings = Vec::with_capacity(1 + by_node.len());
        next_pendings.push(self.local_store.next_pending_seq()?);
        next_pendings.extend(by_node.into_values());

        Committer::run(
            &*self.local_store,
            &next_pendings,
            self.cluster_size,
            &mut self.sink,
        )
    }

    /// Reconstruct the complete pending set on a new leader and load it into
    /// the local store (election intent-recovery — gate-1 **O2**).
    ///
    /// The contributing replica views are the LOCAL store plus every
    /// `peer_pending` set. Recovery requires a **majority** of those views
    /// (`cluster_size / 2 + 1`): only a majority gather is guaranteed to
    /// overlap every acked intent's `min_acks` durability quorum by ≥1, so a
    /// sub-majority gather could miss an acked intent. Below the threshold it
    /// refuses with [`IntentError::InsufficientQuorum`] rather than restore an
    /// incomplete (intent-losing) set.
    ///
    /// Peer sets are first **deduped by [`NodeId`]** (a peer counted twice must
    /// not inflate the majority — gate-1 safety). The majority guard and the
    /// seq-deduped union are then **reused** from [`recover_pending`]: each
    /// distinct peer's set is wrapped in a temporary
    /// [`InMemIntentStore`](crate::intent::InMemIntentStore) and gathered
    /// alongside the live local store as `&dyn IntentStore`, so the
    /// `have = 1 + (distinct peer node count)` view count is checked against
    /// `need = majority` exactly as the election path does — no duplicated
    /// guard logic. The union is then loaded via [`restore_into`], which is
    /// idempotent on perspective-seq (re-running restores nothing new).
    ///
    /// Recovery is **at-least-once** for un-acked intents (gate-1 **F-P4-1**):
    /// the union includes every pending intent on the gathered views,
    /// including a write that reached only one replica and was never
    /// `min_acks`-acked. That is I-L5-safe **only because** the write protocol
    /// fans the chunks to the quorum *before* the metadata intent
    /// (data-before-metadata — the **F-P4-2** / phase-5c producer obligation),
    /// so even a partial intent composes over durable chunks. Clients get
    /// exactly-once by supplying an idempotency key (ADR-047 §5).
    ///
    /// # Returns
    /// The number of intents newly inserted into the local store (existing
    /// seqs are not double-counted — [`restore_into`] is idempotent).
    ///
    /// # Errors
    /// [`IntentError::InsufficientQuorum`] if `1 + (distinct peer node count)`
    /// is below `majority(cluster_size)`; otherwise propagates [`IntentError`]
    /// from [`IntentStore::pending`] or [`IntentStore::put`].
    pub fn recover(
        &mut self,
        peer_pending: &[(NodeId, Vec<WriteIntent>)],
    ) -> Result<usize, IntentError> {
        // Collapse duplicate peer node-ids FIRST: counting the same peer twice
        // toward the majority could pass the O2 guard with too few distinct
        // replicas and restore an incomplete (intent-losing) set. Last write
        // wins on a rare per-node disagreement.
        let mut by_node: HashMap<NodeId, &Vec<WriteIntent>> = HashMap::new();
        for (node, set) in peer_pending {
            by_node.insert(*node, set);
        }

        // Wrap each DISTINCT peer's pending set in a temp in-memory store so the
        // whole gather (local + distinct peers) is a uniform `&[&dyn IntentStore]`,
        // and `recover_pending` applies its O2 majority guard + seq-deduped union
        // verbatim. The `have` it counts is `1 + (distinct peer node count)`.
        let mut peer_stores: Vec<crate::intent::InMemIntentStore> =
            Vec::with_capacity(by_node.len());
        for set in by_node.into_values() {
            let store = crate::intent::InMemIntentStore::new();
            for intent in set {
                store.put(intent.clone())?;
            }
            peer_stores.push(store);
        }

        let mut views: Vec<&dyn IntentStore> = Vec::with_capacity(1 + peer_stores.len());
        views.push(&*self.local_store);
        for store in &peer_stores {
            views.push(store);
        }

        // Majority guard (InsufficientQuorum) + the BTreeMap union live in
        // `recover_pending`; we do not replicate them here.
        let union = recover_pending(&views, self.cluster_size)?;
        restore_into(&*self.local_store, &union)
    }
}

/// Drive a [`ShardCommitter`] on a recurring interval until `shutdown` fires.
///
/// Each iteration gathers the peers' `next_pending_seq` reports, runs one
/// [`ShardCommitter::tick`], then waits `interval` (or breaks on shutdown).
/// Per-tick *and* per-gather errors are logged and swallowed so a single
/// transient failure (an unreachable peer, a transient store error) does not
/// kill the loop — the next pass re-gathers and retries.
///
/// Election-triggered recovery ([`ShardCommitter::recover`] via
/// [`PeerIntentGatherer::gather_pending`]) is **NOT** run here. It is invoked
/// once by the phase-5c election handoff when this node becomes the shard
/// leader; the steady-state loop only advances the watermark.
///
/// # THREADING CONTRACT
/// This loop **MUST** run on its **own dedicated thread** — a `std::thread`
/// holding a runtime [`Handle`](tokio::runtime::Handle) — and **NEVER** on a
/// tokio worker thread. [`ShardCommitter::tick`] drives the synchronous
/// [`IncorporationSink`], whose Raft-log implementation
/// ([`crate::raft_intent_sink::RaftLogIncorporationSink`]) `block_on`s into
/// the async log (ADR-032 / the `raft_intent_sink` threading contract).
/// `block_on` on a worker would block that worker inside the runtime it is
/// trying to drive, deadlocking or starving the append. Spawning this loop is
/// phase 5c/5d, gated on `DecoupledAckEnabled`; this phase only defines it.
pub async fn run_committer_loop<S, G>(
    mut committer: ShardCommitter<S>,
    gatherer: G,
    interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    S: IncorporationSink,
    G: PeerIntentGatherer,
{
    // Already asked to stop before the first pass.
    if *shutdown.borrow() {
        return;
    }
    loop {
        match gatherer.gather_next_pending_seqs().await {
            Ok(peers) => {
                if let Err(e) = committer.tick(&peers) {
                    // Swallow per-tick errors — one bad pass must not kill the
                    // loop; the next quorate pass retries.
                    tracing::warn!(error = %e, "shard committer tick failed; retrying next pass");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "shard committer peer gather failed; retrying next pass");
            }
        }

        // Sleep `interval`, but wake immediately if shutdown is signalled.
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            res = shutdown.changed() => {
                // Sender dropped (Err) or a new value posted (Ok): stop if the
                // current value is `true`, otherwise continue.
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

    use crate::delta::OperationType;
    use crate::intent::{IdempotencyKey, InMemIntentStore, PutOutcome};
    use crate::intent_committer::RecordingSink;
    use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};

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

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s, None)).unwrap(), PutOutcome::Recorded);
        }
    }

    /// A fake [`PeerIntentGatherer`] returning canned peer reports — no
    /// transport. Used by the loop test; `tick`/`recover` are driven directly
    /// with their slices in the other tests.
    struct FakeGatherer {
        next_pendings: Vec<(NodeId, Option<PerspectiveSeq>)>,
        pending: Vec<(NodeId, Vec<WriteIntent>)>,
    }

    impl PeerIntentGatherer for FakeGatherer {
        async fn gather_next_pending_seqs(
            &self,
        ) -> Result<Vec<(NodeId, Option<PerspectiveSeq>)>, IntentError> {
            Ok(self.next_pendings.clone())
        }

        async fn gather_pending(&self) -> Result<Vec<(NodeId, Vec<WriteIntent>)>, IntentError> {
            Ok(self.pending.clone())
        }
    }

    #[test]
    fn tick_with_majority_drains_store() {
        // 3-node cluster, local store holds seq1,seq2,seq3, both peers report
        // None (FullyClosed) → a majority has nothing pending → no upper bound,
        // all three incorporate and the store is pruned.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);

        let n = committer
            .tick(&[(NodeId(2), None), (NodeId(3), None)])
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            committer.sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
        );
        // Store drained by the committer's prune.
        assert_eq!(store.pending_len().unwrap(), 0);
    }

    #[test]
    fn tick_sub_majority_incorporates_nothing() {
        // cluster_size=3 but only the local report (no peers answered). The two
        // silent members pad as `Unknown` → NotStable → nothing incorporated,
        // store intact (the phase-3 sub-majority safety property).
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);

        let n = committer.tick(&[]).unwrap();
        assert_eq!(n, 0);
        assert!(committer.sink.incorporated.is_empty());
        assert_eq!(store.pending_len().unwrap(), 2);
    }

    #[test]
    fn tick_respects_watermark() {
        // Local holds seq1..seq4. The full report becomes
        // [local=Some(seq1), peer=Some(seq3), peer=Some(seq3)] → sorted
        // [seq1, seq3, seq3], idx 1 (maj=2) = seq3 → W = seq3. Only seq1, seq2
        // are strictly below W; seq3, seq4 stay pending.
        let store = Arc::new(InMemIntentStore::new());
        fill(
            &store,
            &[seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1), seq(4, 0, 1)],
        );
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);

        // Peers report a low next_pending that pins W to seq3.
        let n = committer
            .tick(&[
                (NodeId(2), Some(seq(3, 0, 1))),
                (NodeId(3), Some(seq(3, 0, 2))),
            ])
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            committer.sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1)]
        );
        let remaining: Vec<_> = store
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(remaining, vec![seq(3, 0, 1), seq(4, 0, 1)]);
    }

    #[test]
    fn recover_unions_local_and_peers() {
        // Local holds seq1; one peer set holds seq2,seq3. 1 local + 1 peer = 2
        // views = majority(3). recover unions into local (seq1,seq2,seq3) and
        // is idempotent on re-run.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);

        let peer_sets = vec![(
            NodeId(2),
            vec![intent(seq(2, 0, 1), None), intent(seq(3, 0, 1), None)],
        )];
        let restored = committer.recover(&peer_sets).unwrap();
        // seq2, seq3 are new to the local store (seq1 already present).
        assert_eq!(restored, 2);
        let pending: Vec<_> = store
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(pending, vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);

        // Idempotent: re-running restores nothing new and leaves the set whole.
        let again = committer.recover(&peer_sets).unwrap();
        assert_eq!(again, 0);
        assert_eq!(store.pending_len().unwrap(), 3);
    }

    #[test]
    fn recover_below_majority_errors() {
        // cluster_size=3, only the local view (0 peer sets) → have=1 < need=2
        // → InsufficientQuorum, refusing to restore an incomplete set.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);

        match committer.recover(&[]) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 1);
                assert_eq!(need, 2);
            }
            other => panic!("expected InsufficientQuorum, got {other:?}"),
        }
        // The store is untouched on refusal.
        assert_eq!(store.pending_len().unwrap(), 1);
    }

    #[test]
    fn recover_dedups_duplicate_peer() {
        // cluster_size=5 (majority=3). A single peer (NodeId 2) is reported
        // THREE times — a stale-connection / retry duplicate. Without the
        // node-id dedup the view count would be 1 + 3 = 4 >= 3 and recovery
        // would proceed on only TWO distinct nodes (local + node 2), risking a
        // missed acked intent. With dedup the distinct count is 1 + 1 = 2 < 3,
        // so recovery correctly refuses (gate-1 O2).
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 5);

        let dup_peer = vec![
            (NodeId(2), vec![intent(seq(2, 0, 2), None)]),
            (NodeId(2), vec![intent(seq(3, 0, 2), None)]),
            (NodeId(2), vec![intent(seq(4, 0, 2), None)]),
        ];
        match committer.recover(&dup_peer) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 2, "1 local + 1 DISTINCT peer, not 1 + 3 reports");
                assert_eq!(need, 3);
            }
            other => panic!("expected InsufficientQuorum after dedup, got {other:?}"),
        }
        assert_eq!(
            store.pending_len().unwrap(),
            1,
            "store untouched on refusal"
        );
    }

    #[test]
    fn tick_dedups_duplicate_peer() {
        // cluster_size=3. Local holds only seq(9). One peer (NodeId 2) is
        // reported twice as None (FullyClosed). Without dedup the reports
        // [Some(seq9), None, None] would compute a FullyClosed majority (the
        // duplicate fabricating a 2nd voter) and incorporate seq(9) prematurely
        // — node 3 never reported. With dedup the reports are [Some(seq9),
        // None] padded with one Unknown → W = seq(9), so seq(9) is NOT below
        // the watermark and stays pending.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(9, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);

        let n = committer
            .tick(&[(NodeId(2), None), (NodeId(2), None)])
            .unwrap();
        assert_eq!(
            n, 0,
            "duplicate peer must not fabricate a FullyClosed majority"
        );
        assert!(committer.sink.incorporated.is_empty());
        assert_eq!(store.pending_len().unwrap(), 1);
    }

    #[test]
    fn loop_ticks_then_shuts_down_via_gatherer() {
        // Exercise run_committer_loop end-to-end on a local runtime (the
        // #[test] thread is not a tokio worker, so the sink contract holds).
        // The fake gatherer reports both peers None (FullyClosed); one tick
        // drains the store, then we signal shutdown and the loop exits.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = Arc::new(InMemIntentStore::new());
            fill(&store, &[seq(1, 0, 1), seq(2, 0, 1)]);
            let committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3);
            let gatherer = FakeGatherer {
                next_pendings: vec![(NodeId(2), None), (NodeId(3), None)],
                pending: vec![],
            };
            let (tx, rx) = tokio::sync::watch::channel(false);

            let handle = tokio::spawn(run_committer_loop(
                committer,
                gatherer,
                Duration::from_millis(5),
                rx,
            ));

            // Give the loop a few passes to drain the store, then stop it.
            loop {
                if store.pending_len().unwrap() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            tx.send(true).unwrap();
            handle.await.unwrap();
            assert_eq!(store.pending_len().unwrap(), 0);
        });
    }
}
