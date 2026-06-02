//! ADR-047 `LeaderSink` — the per-shard committer driver (leader-only).
//!
//! Phases 3/4/5a built the pieces this module composes:
//!
//! - [`crate::intent`] — the durable [`IntentStore`] (`pending`, `prune`) and
//!   [`WriteIntent`] / [`PerspectiveSeq`].
//! - [`crate::intent_committer`] — the pure consensus logic:
//!   [`Committer::run`] (drain-all) plus election recovery
//!   ([`recover_pending`] / [`restore_into`]) and the synchronous
//!   [`IncorporationSink`] seam.
//! - [`crate::raft_intent_sink`] — the live Raft-log [`IncorporationSink`],
//!   which bridges to the async log via `Handle::block_on` and so MUST be
//!   driven off a dedicated thread, never a tokio worker.
//!
//! **`LeaderSink` driver.** The committer runs ONLY on the shard leader. Its
//! steady state is [`ShardCommitter::drain_local`]: read ALL local pending
//! intents above the F-2 floor, incorporate them ascending, prune. No peer
//! gossip, no watermark — the durability fan includes the leader, so the
//! leader holds every acked intent in its own store. The [`PeerIntentGatherer`]
//! seam survives ONLY for election recovery ([`ShardCommitter::recover`] via
//! [`PeerIntentGatherer::gather_pending`]).
//!
//! The leader-only supervisor that watches Raft leadership, runs `recover()`
//! on becoming leader, and drives `drain_local` (and the follower self-prune)
//! lives in [`crate::raft_shard_store`]; this module provides the building
//! blocks it composes.

use std::collections::HashMap;
use std::sync::Arc;

use kiseki_common::ids::NodeId;

use crate::intent::{IntentError, IntentStore, WriteIntent};
use crate::intent_committer::{recover_pending, restore_into, Committer, IncorporationSink};

/// The per-shard committer's view of its peer replicas' intent stores,
/// gathered over the wire for **election recovery only** (ADR-047 `LeaderSink` /
/// gate-1 O2). The concrete gRPC `IntentSync` implementation is in
/// [`crate::intent_sync`]; tests use a fake.
///
/// Dispatch is **generic** (`G: PeerIntentGatherer`), not `dyn` — the
/// `async fn` has no object-safe form and none is needed (mirrors
/// [`crate::raft_intent_sink::IntentLogAppender`]).
///
/// The steady-state `next_pending` gossip is GONE under `LeaderSink` (no
/// watermark to advance); only the recovery gather remains.
#[allow(async_fn_in_trait)]
pub trait PeerIntentGatherer: Send + Sync {
    /// Each peer's full pending set, for election intent-recovery (gate-1
    /// **O2**), **keyed by the peer's [`NodeId`]** — one entry per *reachable*
    /// peer, NOT the local node. The driver adds the local store, **dedups by
    /// node**, and requires `cluster_size − min_acks + 1` *distinct* nodes
    /// before unioning: only such a gather is guaranteed to overlap every acked
    /// intent's `min_acks` durability quorum, and counting the same peer twice
    /// toward that threshold could miss an acked intent (silent intent loss).
    /// Keying by `NodeId` lets the driver enforce distinctness rather than
    /// trust it.
    ///
    /// # Errors
    /// Transport / peer-store failure rendered into [`IntentError`].
    async fn gather_pending(&self) -> Result<Vec<(NodeId, Vec<WriteIntent>)>, IntentError>;
}

/// The per-shard committer driver (ADR-047 phase 5b-core).
///
/// Owns the local [`IntentStore`], a [`IncorporationSink`] (the Raft-log
/// bridge in production, a recording fake in tests), the cluster size, and
/// `min_acks` (for the recovery-gather threshold). Its decision methods are
/// **synchronous** so that the sink's `Handle::block_on` (in
/// [`crate::raft_intent_sink::RaftLogIncorporationSink`]) is safe on the
/// driver's own dedicated thread (the leader-only supervisor in
/// [`crate::raft_shard_store`] holds it on such a thread).
pub struct ShardCommitter<S: IncorporationSink> {
    local_store: Arc<dyn IntentStore>,
    sink: S,
    cluster_size: usize,
    min_acks: usize,
}

impl<S: IncorporationSink> ShardCommitter<S> {
    /// Build a driver over the local store, sink, cluster size, and `min_acks`.
    /// `min_acks` is the durability quorum size (the producer's fan target);
    /// it sets the recovery-gather threshold `cluster_size − min_acks + 1`.
    #[must_use]
    pub fn new(
        local_store: Arc<dyn IntentStore>,
        sink: S,
        cluster_size: usize,
        min_acks: usize,
    ) -> Self {
        Self {
            local_store,
            sink,
            cluster_size,
            min_acks,
        }
    }

    /// Run one steady-state drain-all pass (`LeaderSink` — leader-only).
    ///
    /// The leader holds every acked intent in its own store (the fan includes
    /// the leader), so it incorporates purely from the local store with NO peer
    /// consultation: read all pending above the F-2 floor, append ascending,
    /// prune. See [`Committer::run`].
    ///
    /// # Returns
    /// The number of intents incorporated into the log this pass (`0` if
    /// nothing was above the floor).
    ///
    /// # Errors
    /// Propagates [`IntentError`] from [`IntentStore::pending`],
    /// [`IncorporationSink::incorporate`], or [`IntentStore::prune`] (all via
    /// [`Committer::run`]).
    pub fn drain_local(&mut self) -> Result<usize, IntentError> {
        Committer::run(&*self.local_store, &mut self.sink)
    }

    /// Reconstruct the complete pending set on a new leader and load it into
    /// the local store (election intent-recovery — gate-1 **O2**).
    ///
    /// The contributing replica views are the LOCAL store plus every
    /// `peer_pending` set. Recovery requires `cluster_size − min_acks + 1`
    /// *distinct* views (MF-7): only such a gather is guaranteed to overlap
    /// every acked intent's `min_acks` durability quorum by ≥1, so a smaller
    /// gather could miss an acked intent. Below the threshold it refuses with
    /// [`IntentError::InsufficientQuorum`] rather than restore an incomplete
    /// (intent-losing) set.
    ///
    /// Peer sets are first **deduped by [`NodeId`]** (a peer counted twice must
    /// not inflate the gather count — gate-1 safety). The threshold guard and
    /// the seq-deduped union are then **reused** from [`recover_pending`]: each
    /// distinct peer's set is wrapped in a temporary
    /// [`InMemIntentStore`](crate::intent::InMemIntentStore) and gathered
    /// alongside the live local store as `&dyn IntentStore`, so the
    /// `have = 1 + (distinct peer node count)` view count is checked against
    /// `need = cluster_size − min_acks + 1`. The union is then loaded via
    /// [`restore_into`], which is idempotent on perspective-seq.
    ///
    /// Recovery is **at-least-once** for un-acked intents (gate-1 **F-P4-1**):
    /// the union includes every pending intent on the gathered views, including
    /// a write that reached only one replica and was never `min_acks`-acked.
    /// That is I-L5-safe **only because** the producer fans chunks to the quorum
    /// *before* the metadata intent (data-before-metadata), so even a partial
    /// intent composes over durable chunks.
    ///
    /// # Returns
    /// The number of intents newly inserted into the local store (existing
    /// seqs are not double-counted — [`restore_into`] is idempotent).
    ///
    /// # Errors
    /// [`IntentError::InsufficientQuorum`] if `1 + (distinct peer node count)`
    /// is below `cluster_size − min_acks + 1`; otherwise propagates
    /// [`IntentError`] from [`IntentStore::pending`] or [`IntentStore::put`].
    pub fn recover(
        &mut self,
        peer_pending: &[(NodeId, Vec<WriteIntent>)],
    ) -> Result<usize, IntentError> {
        // Collapse duplicate peer node-ids FIRST: counting the same peer twice
        // toward the threshold could pass the O2 guard with too few distinct
        // replicas and restore an incomplete (intent-losing) set. Last write
        // wins on a rare per-node disagreement.
        let mut by_node: HashMap<NodeId, &Vec<WriteIntent>> = HashMap::new();
        for (node, set) in peer_pending {
            by_node.insert(*node, set);
        }

        // Wrap each DISTINCT peer's pending set in a temp in-memory store so the
        // whole gather (local + distinct peers) is a uniform
        // `&[&dyn IntentStore]`, and `recover_pending` applies its O2 threshold
        // guard + seq-deduped union verbatim. The `have` it counts is
        // `1 + (distinct peer node count)`.
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

        // Threshold guard (InsufficientQuorum) + the BTreeMap union live in
        // `recover_pending`; we do not replicate them here.
        let union = recover_pending(&views, self.cluster_size, self.min_acks)?;
        restore_into(&*self.local_store, &union)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

    use crate::delta::OperationType;
    use crate::intent::{IdempotencyKey, InMemIntentStore, PerspectiveSeq, PutOutcome};
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
                inline_payloads: vec![],
            },
        }
    }

    fn fill(store: &InMemIntentStore, seqs: &[PerspectiveSeq]) {
        for s in seqs {
            assert_eq!(store.put(intent(*s, None)).unwrap(), PutOutcome::Recorded);
        }
    }

    /// A fake [`PeerIntentGatherer`] returning a canned recovery gather — no
    /// transport. `drain_local` / `recover` are driven directly in the tests.
    struct FakeGatherer {
        pending: Vec<(NodeId, Vec<WriteIntent>)>,
    }

    impl PeerIntentGatherer for FakeGatherer {
        async fn gather_pending(&self) -> Result<Vec<(NodeId, Vec<WriteIntent>)>, IntentError> {
            Ok(self.pending.clone())
        }
    }

    #[test]
    fn drain_local_drains_full_store() {
        // `LeaderSink`: a leader with a populated store drains it fully into the
        // sink ascending, with no peer reports. PART 8 — the store is NOT
        // pruned here; off-band per-intent prune lives in the supervisor.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(3, 0, 1), seq(1, 0, 1), seq(2, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3, 2);

        let n = committer.drain_local().unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            committer.sink.incorporated,
            vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
        );
        assert_eq!(
            store.pending_len().unwrap(),
            3,
            "store retains items; supervisor prunes off-band",
        );
    }

    #[test]
    fn drain_local_drains_single_idle_intent() {
        // The B3 case: one pending intent on an idle shard drains on the next
        // pass (the old exclusive watermark would have stalled it).
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(9, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3, 2);

        let n = committer.drain_local().unwrap();
        assert_eq!(n, 1);
        assert_eq!(committer.sink.incorporated, vec![seq(9, 0, 1)]);
        // Off-band prune lives in the supervisor; committer leaves it.
        assert_eq!(store.pending_len().unwrap(), 1);
    }

    #[test]
    fn drain_local_drains_all_no_floor() {
        // PART 8 — no floor filter; drain everything (SM dedup is the gate).
        // Also: the supervisor (not the committer) prunes, so the store
        // remains populated after `drain_local`.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(2, 0, 1), seq(3, 0, 1)]);
        let sink = RecordingSink::new();
        let mut committer = ShardCommitter::new(store.clone(), sink, 3, 2);

        let n = committer.drain_local().unwrap();
        assert_eq!(n, 2, "drain-all sees both seqs");
        assert_eq!(
            committer.sink.incorporated,
            vec![seq(2, 0, 1), seq(3, 0, 1)]
        );
        // Off-band prune lives in the supervisor; the committer leaves it.
        assert_eq!(store.pending_len().unwrap(), 2);
    }

    #[test]
    fn recover_unions_local_and_peers() {
        // Local holds seq1; one peer set holds seq2,seq3. RF=3/min_acks=2 →
        // threshold 2 = 1 local + 1 peer. recover unions into local and is
        // idempotent on re-run.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3, 2);

        let peer_sets = vec![(
            NodeId(2),
            vec![intent(seq(2, 0, 1), None), intent(seq(3, 0, 1), None)],
        )];
        let restored = committer.recover(&peer_sets).unwrap();
        assert_eq!(restored, 2);
        let pending: Vec<_> = store
            .pending()
            .unwrap()
            .iter()
            .map(|i| i.perspective_seq)
            .collect();
        assert_eq!(pending, vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]);

        // Idempotent: re-running restores nothing new.
        let again = committer.recover(&peer_sets).unwrap();
        assert_eq!(again, 0);
        assert_eq!(store.pending_len().unwrap(), 3);
    }

    #[test]
    fn recover_below_threshold_errors() {
        // RF=3/min_acks=2 → threshold 2. Only the local view (0 peers) →
        // have=1 < need=2 → InsufficientQuorum, refusing an incomplete set.
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3, 2);

        match committer.recover(&[]) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 1);
                assert_eq!(need, 2);
            }
            other => panic!("expected InsufficientQuorum, got {other:?}"),
        }
        assert_eq!(store.pending_len().unwrap(), 1);
    }

    #[test]
    fn recover_rf6_min2_needs_five_distinct_views() {
        // MF-7: RF=6/min_acks=2 → threshold 5. 1 local + 3 distinct peers = 4
        // views < 5 → refuse (a 4-of-6 gather can miss a 2-of-6 acked intent).
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 6, 2);

        let peers = vec![
            (NodeId(2), vec![intent(seq(2, 0, 2), None)]),
            (NodeId(3), vec![intent(seq(3, 0, 3), None)]),
            (NodeId(4), vec![intent(seq(4, 0, 4), None)]),
        ];
        match committer.recover(&peers) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 4, "1 local + 3 distinct peers");
                assert_eq!(need, 5, "RF6/min2 threshold");
            }
            other => panic!("expected InsufficientQuorum {{have:4,need:5}}, got {other:?}"),
        }
    }

    #[test]
    fn recover_dedups_duplicate_peer() {
        // RF=5/min_acks=2 → threshold 4. A single peer (NodeId 2) reported THREE
        // times (a stale-connection / retry duplicate). Without the node-id
        // dedup the view count would be 1 + 3 = 4 >= 4 and recovery would
        // proceed on only TWO distinct nodes. With dedup the distinct count is
        // 1 + 1 = 2 < 4, so recovery correctly refuses (gate-1 O2).
        let store = Arc::new(InMemIntentStore::new());
        fill(&store, &[seq(1, 0, 1)]);
        let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 5, 2);

        let dup_peer = vec![
            (NodeId(2), vec![intent(seq(2, 0, 2), None)]),
            (NodeId(2), vec![intent(seq(3, 0, 2), None)]),
            (NodeId(2), vec![intent(seq(4, 0, 2), None)]),
        ];
        match committer.recover(&dup_peer) {
            Err(IntentError::InsufficientQuorum { have, need }) => {
                assert_eq!(have, 2, "1 local + 1 DISTINCT peer, not 1 + 3 reports");
                assert_eq!(need, 4, "RF5/min2 threshold");
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
    fn recover_then_drain_via_gatherer() {
        // End-to-end on a local runtime: gather peer pending via the fake
        // gatherer, recover (union into local), then drain_local into the sink.
        // RF=3/min_acks=2 → threshold 2 = local + the one peer the fake returns.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = Arc::new(InMemIntentStore::new());
            fill(&store, &[seq(1, 0, 1)]);
            let mut committer = ShardCommitter::new(store.clone(), RecordingSink::new(), 3, 2);
            let gatherer = FakeGatherer {
                pending: vec![(
                    NodeId(2),
                    vec![intent(seq(2, 0, 1), None), intent(seq(3, 0, 1), None)],
                )],
            };

            let peers = gatherer.gather_pending().await.unwrap();
            let restored = committer.recover(&peers).unwrap();
            assert_eq!(restored, 2, "seq2, seq3 unioned in from the peer");

            let n = committer.drain_local().unwrap();
            assert_eq!(n, 3, "leader drains the full recovered set");
            assert_eq!(
                committer.sink.incorporated,
                vec![seq(1, 0, 1), seq(2, 0, 1), seq(3, 0, 1)]
            );
            // PART 8 — off-band prune lives in the supervisor; the committer
            // leaves the store populated for the supervisor's per-intent
            // remove against the SM's recent set.
            assert_eq!(store.pending_len().unwrap(), 3);
        });
    }
}
