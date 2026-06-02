//! ADR-047 `LeaderSink` — the `IntentSync` auxiliary RPC.
//!
//! The committer core ([`crate::shard_committer`]) defines the abstract
//! [`PeerIntentGatherer`](crate::shard_committer::PeerIntentGatherer) seam: a
//! new leader gathers each peer's full pending set for **election
//! intent-recovery** (gate-1 O2). This module is the *RPC* half — the concrete
//! transport that rides the ADR-041 multiplexed Raft listener's auxiliary
//! tag mechanism (`RegistryHandle::register_aux` /
//! [`DispatchOutcome::UnknownTag`](kiseki_raft::tcp_transport::DispatchOutcome)):
//!
//! - The **server** side ([`build_intent_dispatcher`]) is an aux
//!   [`ShardDispatch`](kiseki_raft::tcp_transport::ShardDispatch) closure over
//!   a shard's [`IntentStore`]. It answers the two intent tags
//!   ([`INTENT_GATHER_PENDING_TAG`] for recovery, [`INTENT_PUT_TAG`] for the
//!   producer fan) and falls through (`UnknownTag`) for everything else, so a
//!   peer can serve its intent state without the Raft path ever touching it.
//! - The **client** side ([`TransportIntentGatherer`]) implements
//!   [`PeerIntentGatherer`](crate::shard_committer::PeerIntentGatherer) by
//!   fanning [`INTENT_GATHER_PENDING_TAG`] out to the shard's voter peers over
//!   [`rpc_call`](kiseki_raft::tcp_transport::rpc_call).
//!
//! **`LeaderSink`: no steady-state gossip.** The old `next_pending` watermark
//! gather is GONE — under `LeaderSink` the leader incorporates from its own store
//! (the fan includes the leader) with no peer consultation. Only the recovery
//! gather and the producer fan survive.
//!
//! # Wire encoding
//! Both tags ride postcard (the transport's codec). `gather_pending` cannot
//! postcard a [`WriteIntent`] directly — its `append` is not serde — so each
//! intent is reshaped into a [`WireIntent`] whose `append` is carried as its
//! prost proto bytes (`append_chunk_and_delta_request_to_proto(..).encode_to_vec()`),
//! exactly the byte form [`crate::intent::FjallIntentStore`] persists. The
//! response is `postcard(Vec<WireIntent>)`. The proto round-trip preserves the
//! append exactly (`chunk_refs` / payload / operation / `new_chunks`), pinned
//! by the round-trip test.

use std::sync::Arc;

use futures::future::BoxFuture;
use kiseki_common::ids::{NodeId, ShardId};
use kiseki_common::time::HybridLogicalClock;
use kiseki_proto::v1::AppendChunkAndDeltaRequest as ProtoChunkAppendReq;
use prost::Message;
use serde::{Deserialize, Serialize};

use kiseki_raft::tcp_transport::{rpc_call, DispatchOutcome, ShardDispatch};

use crate::grpc::{append_chunk_and_delta_request_to_proto, proto_to_append_chunk_and_delta};
use crate::intent::{IntentError, IntentStore, PerspectiveSeq, WriteIntent};
use crate::shard_committer::PeerIntentGatherer;

/// Aux tag: "your full pending intent set for this shard" — for election
/// intent-recovery (gate-1 O2). MUST NOT collide with the Raft tags
/// (`append_entries` / `vote` / `full_snapshot`).
///
/// Wire-compat coupling: `kiseki_raft::transport_metrics::op::INTENT_GATHER_PENDING`
/// MUST equal this string verbatim (the asserts in the tests below catch drift).
pub const INTENT_GATHER_PENDING_TAG: &str = "intent_gather_pending";

/// Aux tag: "durably record this fanned intent on your local store" — the
/// quorum intent-write the producer fans to a shard's voter peers BEFORE the
/// gateway fast-acks (ADR-047 phase 5c, the no-loss floor I-L2/I-CS1). MUST
/// NOT collide with the Raft tags or the other intent tags.
///
/// Wire-compat coupling: `kiseki_raft::transport_metrics::op::INTENT_PUT`
/// MUST equal this string verbatim (the asserts in the tests below catch drift).
pub const INTENT_PUT_TAG: &str = "intent_put";

/// Wire form of a [`WriteIntent`] — used both for the `gather_pending`
/// response (server → committer) and the `intent_put` fan (producer → peer).
///
/// A `WriteIntent` is not directly serde (its `append` carries domain types
/// that derive no serde). The order (`seq`) and the idempotency key serialize
/// natively; the built append rides as its prost proto bytes — the SAME byte
/// form [`crate::intent::FjallIntentStore`] persists — so the proto's own
/// forward-compat covers the append and the round-trip is exact.
///
/// `pub(crate)` so [`crate::RaftShardStore::put_intent_and_fan`] can encode it
/// for the `intent_put` fan; the fields stay private to this module.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WireIntent {
    /// The ingress-assigned perspective seq (the order).
    seq: HybridLogicalClock,
    /// The client idempotency key, if any.
    idem: Option<[u8; 16]>,
    /// `append_chunk_and_delta_request_to_proto(&wi.append).encode_to_vec()`.
    append_proto: Vec<u8>,
}

impl From<&WriteIntent> for WireIntent {
    /// Reshape a [`WriteIntent`] into its wire form (proto-encode the append).
    fn from(wi: &WriteIntent) -> Self {
        Self {
            seq: wi.perspective_seq.0,
            idem: wi.idempotency_key,
            append_proto: append_chunk_and_delta_request_to_proto(&wi.append).encode_to_vec(),
        }
    }
}

impl WireIntent {
    /// Decode back into a [`WriteIntent`], re-parsing the append from its
    /// proto bytes.
    ///
    /// # Errors
    /// [`IntentError::Codec`] if `append_proto` is not a valid
    /// [`ProtoChunkAppendReq`] or a field is malformed.
    fn into_intent(self) -> Result<WriteIntent, IntentError> {
        let proto = ProtoChunkAppendReq::decode(self.append_proto.as_slice())
            .map_err(|e| IntentError::Codec(format!("WireIntent proto decode: {e}")))?;
        let append = proto_to_append_chunk_and_delta(proto).map_err(IntentError::Codec)?;
        Ok(WriteIntent {
            perspective_seq: PerspectiveSeq(self.seq),
            idempotency_key: self.idem,
            append,
        })
    }
}

// ---------------------------------------------------------------------------
// Server: the per-shard IntentSync aux dispatcher
// ---------------------------------------------------------------------------

/// Build the aux [`ShardDispatch`] closure for a shard's [`IntentStore`]
/// (ADR-047 phase 5b-rpc, server side).
///
/// Registered via `RegistryHandle::register_aux(shard_id, dispatch)`. The
/// listener routes a tag here only after the shard's Raft dispatcher returns
/// `UnknownTag`, so the consensus-critical path never touches this. The
/// closure answers the two intent tags from `store` and returns `UnknownTag`
/// for anything else (which the listener maps to the `ParseError` wire status,
/// indistinguishable from "no aux").
///
/// The read tag ([`INTENT_GATHER_PENDING_TAG`]) SERVES this replica's full
/// pending intent set to a new leader's recovery gather. The write tag
/// ([`INTENT_PUT_TAG`]) is the *server* side of the producer's quorum
/// intent-write: a peer fans a [`WireIntent`] here, this node decodes it and
/// durably records it in `store`, and the `Ok` reply counts as one durable copy
/// toward the producer's `min_acks`.
///
/// A store [`IntentError`] is logged and mapped to
/// [`DispatchOutcome::ParseError`]. For the read tag a non-`Ok` peer is skipped
/// by the client gatherer; for [`INTENT_PUT_TAG`] the producer counts a non-`Ok`
/// reply as a non-ack (it does NOT credit a durable copy), so a failed store
/// write can never inflate the ack count. A payload decode fault likewise
/// degrades to `ParseError` (a non-ack). Response encoding is wrapped so an
/// encode fault also degrades to `ParseError` rather than escaping as a panic.
#[must_use]
pub fn build_intent_dispatcher(
    store: Arc<dyn IntentStore>,
    recv_coalescer: Option<Arc<crate::intent_recv_coalescer::IntentRecvCoalescer>>,
) -> ShardDispatch {
    Arc::new(
        move |tag: &str, payload: &[u8]| -> BoxFuture<'_, DispatchOutcome> {
            let store = Arc::clone(&store);
            let recv_coalescer = recv_coalescer.as_ref().map(Arc::clone);
            let tag = tag.to_owned();
            let payload = payload.to_vec();
            Box::pin(async move {
                match tag.as_str() {
                    INTENT_GATHER_PENDING_TAG => match store.pending() {
                        Ok(intents) => {
                            let wire: Vec<WireIntent> =
                                intents.iter().map(WireIntent::from).collect();
                            encode_ok(&wire)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, tag = %tag, "IntentSync gather_pending failed");
                            DispatchOutcome::ParseError
                        }
                    },
                    INTENT_PUT_TAG => {
                        // W12 (2026-06-02): the wire is now a BATCH —
                        // `Vec<WireIntent>` in, `Vec<bool>` out (one bool per
                        // input intent, true = durable on this replica). The
                        // producer-side coalescer fans up to
                        // KISEKI_INTENT_FAN_BATCH_MAX intents per RPC; the
                        // receiver decodes the whole batch and does ONE
                        // `store.put_batch` so the fjall WAL sync amortises
                        // across the batch.
                        //
                        // ADR-047 hot-path timer (aux.handle_intent_put_total)
                        // — total server-side processing budget for ONE FAN
                        // (carrying N intents post-W12). Paired with the
                        // leader-side `pif.leader_first_hop` /
                        // `pif.parallel_topup` totals so we can split the
                        // round trip into (wire + scheduler queue) vs
                        // (server proc). Note: under W12 a higher mean means
                        // bigger batches, NOT slower receivers — pair with
                        // `kiseki_intent_put_batch_size` to interpret.
                        kiseki_tracing::hot_timer_guard!(
                            _ht_aux_total = "aux.handle_intent_put_total"
                        );
                        // ADR-047 hot-path timer (aux.decode) — the
                        // postcard decode of the Vec<WireIntent> + per-intent
                        // proto re-decode for the appends.
                        let wires: Vec<WireIntent> = {
                            kiseki_tracing::hot_timer_guard!(_ht_dec = "aux.decode");
                            match postcard::from_bytes(&payload) {
                                Ok(w) => w,
                                Err(e) => {
                                    tracing::warn!(error = %e, tag = %tag, "IntentSync intent_put decode failed");
                                    return DispatchOutcome::ParseError;
                                }
                            }
                        };
                        // Record the batch size metric so the per-RPC
                        // histogram remains interpretable post-coalescing.
                        crate::intent_metrics::observe_intent_put_batch_size(wires.len());
                        // Decode every intent up front. A decode fault on ANY
                        // one drops the whole batch — the producer treats
                        // that as a non-ack for every PUT in the batch and
                        // retries. (Pre-W12 the same fault dropped one PUT;
                        // the failure-mode aggregation is acceptable since
                        // any decode fault here points at a wire bug, not a
                        // legitimate per-PUT condition.)
                        let mut intents = Vec::with_capacity(wires.len());
                        for w in wires {
                            match w.into_intent() {
                                Ok(i) => intents.push(i),
                                Err(e) => {
                                    tracing::warn!(error = %e, tag = %tag, "IntentSync intent_put append decode failed");
                                    return DispatchOutcome::ParseError;
                                }
                            }
                        }
                        let n = intents.len();
                        // Lever 1 (2026-06-02): receiver-side coalescer
                        // when configured. The coalescer aggregates this
                        // RPC with concurrent RPCs from other producers
                        // into ONE fjall put_batch per coalesce window.
                        // Falls back to direct `store.put_batch` when no
                        // coalescer is configured (test paths /
                        // not-yet-wired call sites).
                        //
                        // ADR-047 hot-path timer (aux.store_put) — under
                        // the coalescer the span includes the receiver-
                        // side wait. Pair with the
                        // `kiseki_intent_recv_coalesce_wait_seconds`
                        // histogram to split wait vs commit.
                        let put_res = kiseki_tracing::hot_span!("aux.store_put", {
                            match recv_coalescer {
                                Some(c) => c.submit(intents).await,
                                None => store
                                    .put_batch(intents)
                                    .map(|outcomes| outcomes.iter().map(|_| true).collect()),
                            }
                        });
                        match put_res {
                            Ok(acks) => {
                                debug_assert_eq!(
                                    acks.len(),
                                    n,
                                    "ack vec must have one entry per input intent"
                                );
                                kiseki_tracing::hot_span!("aux.encode_response", {
                                    encode_ok(&acks)
                                })
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, tag = %tag, "IntentSync intent_put store write failed");
                                DispatchOutcome::ParseError
                            }
                        }
                    }
                    // Not an intent tag — let the listener report it as the
                    // same ParseError wire status a truly-unknown tag gets.
                    _ => DispatchOutcome::UnknownTag,
                }
            })
        },
    )
}

/// Postcard-encode an `Ok` response body, degrading an encode fault to
/// `ParseError` so the dispatcher never panics on a serialization error.
fn encode_ok<T: Serialize>(value: &T) -> DispatchOutcome {
    match postcard::to_stdvec(value) {
        Ok(bytes) => DispatchOutcome::Ok(bytes),
        Err(e) => {
            tracing::warn!(error = %e, "IntentSync response encode failed");
            DispatchOutcome::ParseError
        }
    }
}

// ---------------------------------------------------------------------------
// Client: TransportIntentGatherer
// ---------------------------------------------------------------------------

/// A reachable peer to gather from: its Raft node id and its transport addr.
#[derive(Clone, Debug)]
struct Peer {
    node_id: NodeId,
    addr: String,
}

/// The client half of `IntentSync` (ADR-047 `LeaderSink` — recovery gather).
///
/// Implements [`PeerIntentGatherer`] by fanning [`INTENT_GATHER_PENDING_TAG`]
/// out to a shard's voter peers (minus the local node) over the multiplexed
/// Raft transport. An unreachable peer (connect/transport failure or a non-`Ok`
/// status) is **skipped** — the new leader's recovery threshold guard
/// ([`ShardCommitter::recover`](crate::shard_committer::ShardCommitter::recover))
/// refuses if too few distinct peers answer, so a partial gather can never
/// silently restore an incomplete set.
///
/// Each entry in the result is keyed by the peer's [`NodeId`], and each voter
/// appears at most once (the peer set is built from a deduped voter list).
pub struct TransportIntentGatherer {
    shard_id: ShardId,
    /// The distinct voter peers to query — voters minus the local node, each
    /// resolved to an addr. Built once at construction from the shard's live
    /// membership; a peer that has no addr in the node map is dropped. When a
    /// `resolver` is set this is the *fallback* (used only if the resolver is
    /// absent); the live resolver wins.
    peers: Vec<Peer>,
    /// Optional **live** voter resolver, re-evaluated on every gather. The
    /// committer is spawned at `create_shard` time — BEFORE the shard's Raft
    /// membership is initialized, when `voter_ids()` is still empty — so a
    /// snapshot taken at construction would fan to nobody forever. The resolver
    /// re-reads the live voter set each tick so the gatherer tracks membership
    /// as it converges (and as it changes on reconfiguration).
    #[allow(clippy::type_complexity)]
    resolver: Option<Arc<dyn Fn() -> Vec<(NodeId, String)> + Send + Sync>>,
    /// Optional TLS client config (mirrors the Raft network's mTLS). `None`
    /// in dev / plaintext clusters.
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl TransportIntentGatherer {
    /// Build a gatherer for `shard_id` from an explicit (static) peer set.
    ///
    /// `peers` is `(node_id, addr)` for each DISTINCT voter to query — already
    /// minus the local node. Prefer
    /// [`RaftShardStore::intent_gatherer`](crate::RaftShardStore::intent_gatherer),
    /// which derives this from the shard's live membership; this constructor is
    /// the seam tests (and any caller with a pre-resolved set) use.
    #[must_use]
    pub fn new(
        shard_id: ShardId,
        peers: Vec<(NodeId, String)>,
        tls_config: Option<Arc<rustls::ClientConfig>>,
    ) -> Self {
        let peers = peers
            .into_iter()
            .map(|(node_id, addr)| Peer { node_id, addr })
            .collect();
        Self {
            shard_id,
            peers,
            resolver: None,
            tls_config,
        }
    }

    /// Build a gatherer that re-resolves its voter peers LIVE on every gather
    /// via `resolver` (voters minus the local node, each mapped to its addr).
    ///
    /// Used by the phase-5c committer spawn, which is wired at `create_shard`
    /// time — before membership is initialized — so a static snapshot would be
    /// empty forever. See the `resolver` field.
    #[must_use]
    pub fn with_resolver(
        shard_id: ShardId,
        resolver: Arc<dyn Fn() -> Vec<(NodeId, String)> + Send + Sync>,
        tls_config: Option<Arc<rustls::ClientConfig>>,
    ) -> Self {
        Self {
            shard_id,
            peers: Vec::new(),
            resolver: Some(resolver),
            tls_config,
        }
    }

    /// The number of distinct voter peers this gatherer fans out to (NOT
    /// counting the local node). The committer's `cluster_size` is this + 1.
    /// For a live-resolver gatherer this reflects the current membership.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.current_peers().len()
    }

    /// The voter peers to query right now: the live resolver's output if set,
    /// else the static snapshot.
    fn current_peers(&self) -> Vec<Peer> {
        match &self.resolver {
            Some(resolve) => resolve()
                .into_iter()
                .map(|(node_id, addr)| Peer { node_id, addr })
                .collect(),
            None => self.peers.clone(),
        }
    }
}

impl PeerIntentGatherer for TransportIntentGatherer {
    async fn gather_pending(&self) -> Result<Vec<(NodeId, Vec<WriteIntent>)>, IntentError> {
        let peers = self.current_peers();
        let mut out = Vec::with_capacity(peers.len());
        for peer in &peers {
            // postcard(Vec<WireIntent>) on the wire.
            let resp: Result<Vec<WireIntent>, _> = rpc_call(
                &peer.addr,
                self.shard_id,
                INTENT_GATHER_PENDING_TAG,
                self.tls_config.as_ref(),
                &(),
            )
            .await;
            match resp {
                Ok(wire) => {
                    // A decode fault on an otherwise-reachable peer IS an error
                    // (a protocol/version bug, not an absent peer): surface it
                    // so the committer logs + retries rather than silently
                    // dropping a peer that DID answer.
                    let intents = wire
                        .into_iter()
                        .map(WireIntent::into_intent)
                        .collect::<Result<Vec<_>, _>>()?;
                    out.push((peer.node_id, intents));
                }
                Err(e) => {
                    tracing::debug!(
                        node = peer.node_id.0,
                        addr = %peer.addr,
                        error = %e,
                        "IntentSync gather_pending: peer unreachable, skipping",
                    );
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use kiseki_common::ids::{ChunkId, OrgId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, WallTime};

    use crate::delta::OperationType;
    use crate::intent::{IdempotencyKey, InMemIntentStore};
    use crate::raft_store::NewChunkMeta;
    use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest};

    fn seq(physical_ms: u64, logical: u32, node: u64) -> PerspectiveSeq {
        PerspectiveSeq(HybridLogicalClock {
            physical_ms,
            logical,
            node_id: NodeId(node),
        })
    }

    /// W9 (2026-06-02): the metric label strings in `kiseki-raft` are
    /// hard-coded literals because `kiseki-raft` can't depend on
    /// `kiseki-log` (the dep edge points the other way). This catches drift
    /// if either side renames its tag.
    #[test]
    fn metric_label_strings_match_aux_tag_strings() {
        assert_eq!(
            INTENT_PUT_TAG,
            kiseki_raft::transport_metrics::op::INTENT_PUT,
            "intent_put tag drift — metric label out of sync"
        );
        assert_eq!(
            INTENT_GATHER_PENDING_TAG,
            kiseki_raft::transport_metrics::op::INTENT_GATHER_PENDING,
            "intent_gather_pending tag drift — metric label out of sync"
        );
    }

    /// A non-trivial append: real `chunk_refs`, payload, operation, and a
    /// new chunk — so the proto round-trip is exercised on every field.
    fn rich_intent(s: PerspectiveSeq, key: Option<IdempotencyKey>) -> WriteIntent {
        WriteIntent {
            perspective_seq: s,
            idempotency_key: key,
            append: AppendChunkAndDeltaRequest {
                delta: AppendDeltaRequest {
                    shard_id: ShardId(uuid::Uuid::from_u128(0x5117)),
                    tenant_id: OrgId(uuid::Uuid::from_u128(0x7e_9a_11)),
                    operation: OperationType::Update,
                    timestamp: DeltaTimestamp {
                        hlc: s.0,
                        wall: WallTime {
                            millis_since_epoch: s.0.physical_ms,
                            timezone: "UTC".into(),
                        },
                        quality: ClockQuality::Ptp,
                    },
                    hashed_key: [0x2bu8; 32],
                    chunk_refs: vec![ChunkId([0x11u8; 32]), ChunkId([0x22u8; 32])],
                    payload: vec![0xde, 0xad, 0xbe, 0xef],
                    has_inline_data: true,
                },
                new_chunks: vec![NewChunkMeta {
                    chunk_id: [0x33u8; 32],
                    placement: vec![7, 9],
                    original_len: 4096,
                }],
            },
        }
    }

    fn assert_append_eq(a: &AppendChunkAndDeltaRequest, b: &AppendChunkAndDeltaRequest) {
        assert_eq!(a.delta.shard_id, b.delta.shard_id, "shard_id");
        assert_eq!(a.delta.tenant_id, b.delta.tenant_id, "tenant_id");
        assert_eq!(a.delta.operation, b.delta.operation, "operation");
        assert_eq!(a.delta.hashed_key, b.delta.hashed_key, "hashed_key");
        assert_eq!(a.delta.chunk_refs, b.delta.chunk_refs, "chunk_refs");
        assert_eq!(a.delta.payload, b.delta.payload, "payload");
        assert_eq!(
            a.delta.has_inline_data, b.delta.has_inline_data,
            "has_inline_data"
        );
        assert_eq!(a.new_chunks.len(), b.new_chunks.len(), "new_chunks len");
        for (x, y) in a.new_chunks.iter().zip(&b.new_chunks) {
            assert_eq!(x.chunk_id, y.chunk_id, "new_chunk.chunk_id");
            assert_eq!(x.placement, y.placement, "new_chunk.placement");
            assert_eq!(x.original_len, y.original_len, "new_chunk.original_len");
        }
    }

    /// `WireIntent` round-trip preserves the append EXACTLY (every field),
    /// plus `perspective_seq` and `idempotency_key`. This is the unit-level pin
    /// of the proto fidelity the transport test asserts end-to-end.
    #[test]
    fn wire_intent_round_trip_preserves_append() {
        let key = [9u8; 16];
        let original = rich_intent(seq(7, 3, 2), Some(key));
        let wire = WireIntent::from(&original);
        // postcard the WireIntent (what crosses the wire inside Vec<WireIntent>).
        let bytes = postcard::to_stdvec(&wire).unwrap();
        let decoded_wire: WireIntent = postcard::from_bytes(&bytes).unwrap();
        let decoded = decoded_wire.into_intent().unwrap();

        assert_eq!(decoded.perspective_seq, original.perspective_seq);
        assert_eq!(decoded.idempotency_key, original.idempotency_key);
        assert_append_eq(&decoded.append, &original.append);
    }

    /// The aux dispatcher answers `gather_pending` and the decoded intents
    /// (via the proto round-trip) equal the stored ones — append fidelity.
    #[tokio::test]
    async fn dispatcher_gather_pending_preserves_appends() {
        let store = Arc::new(InMemIntentStore::new());
        let i1 = rich_intent(seq(1, 0, 1), Some([1u8; 16]));
        let i2 = rich_intent(seq(2, 0, 1), None);
        store.put(i1.clone()).unwrap();
        store.put(i2.clone()).unwrap();
        let dispatch = build_intent_dispatcher(store, None);

        let outcome = dispatch(INTENT_GATHER_PENDING_TAG, &[]).await;
        let DispatchOutcome::Ok(bytes) = outcome else {
            panic!("expected Ok");
        };
        let wire: Vec<WireIntent> = postcard::from_bytes(&bytes).unwrap();
        let decoded: Vec<WriteIntent> = wire
            .into_iter()
            .map(WireIntent::into_intent)
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(decoded.len(), 2);
        // pending() is ascending by seq.
        assert_eq!(decoded[0].perspective_seq, i1.perspective_seq);
        assert_eq!(decoded[0].idempotency_key, i1.idempotency_key);
        assert_append_eq(&decoded[0].append, &i1.append);
        assert_eq!(decoded[1].perspective_seq, i2.perspective_seq);
        assert_eq!(decoded[1].idempotency_key, i2.idempotency_key);
        assert_append_eq(&decoded[1].append, &i2.append);
    }

    /// The `intent_put` arm decodes a fanned `Vec<WireIntent>` and durably
    /// records every intent in the peer's store — the server side of the
    /// producer's quorum intent-write (ADR-047 phase 5c, W12 batched).
    /// The Ok reply is a `Vec<bool>` (one true per durable intent).
    #[tokio::test]
    async fn dispatcher_intent_put_stores_fanned_intent() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let dispatch = build_intent_dispatcher(Arc::clone(&store), None);

        let one = rich_intent(seq(7, 3, 2), Some([0xa1u8; 16]));
        let two = rich_intent(seq(7, 4, 2), Some([0xa2u8; 16]));
        let three = rich_intent(seq(7, 5, 2), None);
        let wires: Vec<WireIntent> = [&one, &two, &three]
            .iter()
            .map(|i| WireIntent::from(*i))
            .collect();
        let payload = postcard::to_stdvec(&wires).unwrap();
        let outcome = dispatch(INTENT_PUT_TAG, &payload).await;
        let DispatchOutcome::Ok(bytes) = outcome else {
            panic!("expected Ok ack");
        };
        // Reply body is `Vec<bool>` — one ack per input intent.
        let acks: Vec<bool> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(acks.len(), 3, "one ack per input");
        assert!(acks.iter().all(|b| *b), "all three durably stored");

        // The intents now round-trip out of the peer's store, appends intact.
        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 3, "all three fanned intents stored");
        assert_eq!(pending[0].perspective_seq, one.perspective_seq);
        assert_append_eq(&pending[0].append, &one.append);
        assert_eq!(pending[1].perspective_seq, two.perspective_seq);
        assert_append_eq(&pending[1].append, &two.append);
        assert_eq!(pending[2].perspective_seq, three.perspective_seq);
        assert_append_eq(&pending[2].append, &three.append);
    }

    /// A malformed `intent_put` payload degrades to `ParseError` (a non-ack)
    /// rather than recording garbage — the producer counts it as a non-ack.
    #[tokio::test]
    async fn dispatcher_intent_put_bad_payload_is_parse_error() {
        let store: Arc<dyn IntentStore> = Arc::new(InMemIntentStore::new());
        let dispatch = build_intent_dispatcher(Arc::clone(&store), None);
        let outcome = dispatch(INTENT_PUT_TAG, &[0xff, 0x00, 0x13, 0x37]).await;
        assert!(matches!(outcome, DispatchOutcome::ParseError));
        assert_eq!(store.pending_len().unwrap(), 0, "nothing recorded");
    }

    /// An unknown tag falls through to `UnknownTag` (the listener maps it to
    /// the same `ParseError` wire status a truly-unknown tag gets).
    #[tokio::test]
    async fn dispatcher_unknown_tag_falls_through() {
        let store = Arc::new(InMemIntentStore::new());
        let dispatch = build_intent_dispatcher(store, None);
        let outcome = dispatch("not_an_intent_tag", &[]).await;
        assert!(matches!(outcome, DispatchOutcome::UnknownTag));
    }

    /// The two intent tags do not collide with the Raft tags or each other.
    #[test]
    fn intent_tags_distinct_from_raft_tags() {
        let intent_tags = [INTENT_GATHER_PENDING_TAG, INTENT_PUT_TAG];
        for raft_tag in ["append_entries", "vote", "full_snapshot"] {
            for it in intent_tags {
                assert_ne!(it, raft_tag);
            }
        }
        // The intent tags are pairwise distinct.
        for i in 0..intent_tags.len() {
            for j in (i + 1)..intent_tags.len() {
                assert_ne!(intent_tags[i], intent_tags[j]);
            }
        }
    }
}
