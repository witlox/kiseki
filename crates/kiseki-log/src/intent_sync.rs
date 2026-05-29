//! ADR-047 phase 5b-rpc — the `IntentSync` auxiliary RPC.
//!
//! The phase 5b *core* ([`crate::shard_committer`]) defines the abstract
//! [`PeerIntentGatherer`](crate::shard_committer::PeerIntentGatherer) seam: a
//! shard's committer fans the peers' intent-store reports over the wire to
//! advance the stability watermark, and gathers each peer's full pending set
//! for election intent-recovery. This module is the *RPC* half — the concrete
//! transport that rides the ADR-041 multiplexed Raft listener's auxiliary
//! tag mechanism (`RegistryHandle::register_aux` /
//! [`DispatchOutcome::UnknownTag`](kiseki_raft::tcp_transport::DispatchOutcome)):
//!
//! - The **server** side ([`build_intent_dispatcher`]) is an aux
//!   [`ShardDispatch`](kiseki_raft::tcp_transport::ShardDispatch) closure over
//!   a shard's [`IntentStore`]. It answers the two intent tags and falls
//!   through (`UnknownTag`) for everything else, so a peer can SERVE its
//!   intent state without the Raft path ever touching it.
//! - The **client** side ([`TransportIntentGatherer`]) implements
//!   [`PeerIntentGatherer`](crate::shard_committer::PeerIntentGatherer) by
//!   fanning the two tags out to the shard's voter peers over
//!   [`rpc_call`](kiseki_raft::tcp_transport::rpc_call).
//!
//! **Inert in production after this phase.** `create_shard` wires the
//! dispatcher + an (empty) per-shard [`IntentStore`] so peers *can* serve, but
//! no producer writes intents and no committer task queries — the gatherer is
//! only invoked by the 5c/5d committer task. The synchronous write path,
//! gateway, and startup wiring are untouched (ADR-047 "Follow-ups" / #140).
//!
//! # Wire encoding
//! Both tags ride postcard (the transport's codec). `next_pending` is a bare
//! `postcard(Option<HybridLogicalClock>)`. `gather_pending` cannot postcard a
//! [`WriteIntent`] directly — its `append` is not serde — so each intent is
//! reshaped into a [`WireIntent`] whose `append` is carried as its prost proto
//! bytes (`append_chunk_and_delta_request_to_proto(..).encode_to_vec()`),
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

/// Aux tag: "your lowest pending perspective seq for this shard" — one
/// replica's contribution to the majority stability watermark (ADR-047 §3).
/// MUST NOT collide with the Raft tags
/// (`append_entries` / `vote` / `full_snapshot`).
pub const INTENT_NEXT_PENDING_TAG: &str = "intent_next_pending";

/// Aux tag: "your full pending intent set for this shard" — for election
/// intent-recovery (gate-1 O2). MUST NOT collide with the Raft tags.
pub const INTENT_GATHER_PENDING_TAG: &str = "intent_gather_pending";

/// Wire form of a [`WriteIntent`] for the `gather_pending` response.
///
/// A `WriteIntent` is not directly serde (its `append` carries domain types
/// that derive no serde). The order (`seq`) and the idempotency key serialize
/// natively; the built append rides as its prost proto bytes — the SAME byte
/// form [`crate::intent::FjallIntentStore`] persists — so the proto's own
/// forward-compat covers the append and the round-trip is exact.
#[derive(Debug, Serialize, Deserialize)]
struct WireIntent {
    /// The ingress-assigned perspective seq (the order).
    seq: HybridLogicalClock,
    /// The client idempotency key, if any.
    idem: Option<[u8; 16]>,
    /// `append_chunk_and_delta_request_to_proto(&wi.append).encode_to_vec()`.
    append_proto: Vec<u8>,
}

impl WireIntent {
    /// Reshape a [`WriteIntent`] into its wire form (proto-encode the append).
    fn from_intent(wi: &WriteIntent) -> Self {
        Self {
            seq: wi.perspective_seq.0,
            idem: wi.idempotency_key,
            append_proto: append_chunk_and_delta_request_to_proto(&wi.append).encode_to_vec(),
        }
    }

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
/// A store [`IntentError`] is logged and mapped to
/// [`DispatchOutcome::ParseError`] — the *client* treats any non-`Ok` peer as
/// absent (a conservative pad in the watermark), so a store fault never
/// fabricates a report. Response encoding is wrapped so an encode fault also
/// degrades to `ParseError` rather than escaping as a panic.
#[must_use]
pub fn build_intent_dispatcher(store: Arc<dyn IntentStore>) -> ShardDispatch {
    Arc::new(
        move |tag: &str, _payload: &[u8]| -> BoxFuture<'_, DispatchOutcome> {
            let store = Arc::clone(&store);
            let tag = tag.to_owned();
            Box::pin(async move {
                match tag.as_str() {
                    INTENT_NEXT_PENDING_TAG => match store.next_pending_seq() {
                        Ok(opt) => {
                            // Wire form: postcard(Option<HybridLogicalClock>).
                            let hlc: Option<HybridLogicalClock> = opt.map(|s| s.0);
                            encode_ok(&hlc)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, tag = %tag, "IntentSync next_pending failed");
                            DispatchOutcome::ParseError
                        }
                    },
                    INTENT_GATHER_PENDING_TAG => match store.pending() {
                        Ok(intents) => {
                            let wire: Vec<WireIntent> =
                                intents.iter().map(WireIntent::from_intent).collect();
                            encode_ok(&wire)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, tag = %tag, "IntentSync gather_pending failed");
                            DispatchOutcome::ParseError
                        }
                    },
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

/// The client half of `IntentSync` (ADR-047 phase 5b-rpc).
///
/// Implements [`PeerIntentGatherer`] by fanning the two intent tags out to a
/// shard's voter peers (minus the local node) over the multiplexed Raft
/// transport. An unreachable peer (connect/transport failure or a non-`Ok`
/// status) is **skipped**, never an error and never a fabricated report:
/// [`compute_stability_watermark`](crate::intent_committer::compute_stability_watermark)
/// pads an absent member conservatively, so a partial gather can only lower
/// the watermark, never raise it past what the membership permits.
///
/// Each entry in the result is keyed by the peer's [`NodeId`], and each voter
/// appears at most once (the peer set is built from a deduped voter list).
pub struct TransportIntentGatherer {
    shard_id: ShardId,
    /// The distinct voter peers to query — voters minus the local node, each
    /// resolved to an addr. Built once at construction from the shard's live
    /// membership; a peer that has no addr in the node map is dropped.
    peers: Vec<Peer>,
    /// Optional TLS client config (mirrors the Raft network's mTLS). `None`
    /// in dev / plaintext clusters.
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl TransportIntentGatherer {
    /// Build a gatherer for `shard_id` from an explicit peer set.
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
            tls_config,
        }
    }

    /// The number of distinct voter peers this gatherer fans out to (NOT
    /// counting the local node). The committer's `cluster_size` is this + 1.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

impl PeerIntentGatherer for TransportIntentGatherer {
    async fn gather_next_pending_seqs(
        &self,
    ) -> Result<Vec<(NodeId, Option<PerspectiveSeq>)>, IntentError> {
        let mut out = Vec::with_capacity(self.peers.len());
        for peer in &self.peers {
            // postcard(Option<HybridLogicalClock>) on the wire.
            let resp: Result<Option<HybridLogicalClock>, _> = rpc_call(
                &peer.addr,
                self.shard_id,
                INTENT_NEXT_PENDING_TAG,
                self.tls_config.as_ref(),
                &(),
            )
            .await;
            match resp {
                Ok(opt) => out.push((peer.node_id, opt.map(PerspectiveSeq))),
                // Skip an unreachable / non-Ok peer: absent is the conservative
                // pad. Never fabricate a report.
                Err(e) => {
                    tracing::debug!(
                        node = peer.node_id.0,
                        addr = %peer.addr,
                        error = %e,
                        "IntentSync gather_next_pending: peer unreachable, skipping",
                    );
                }
            }
        }
        Ok(out)
    }

    async fn gather_pending(&self) -> Result<Vec<(NodeId, Vec<WriteIntent>)>, IntentError> {
        let mut out = Vec::with_capacity(self.peers.len());
        for peer in &self.peers {
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
        let wire = WireIntent::from_intent(&original);
        // postcard the WireIntent (what crosses the wire inside Vec<WireIntent>).
        let bytes = postcard::to_stdvec(&wire).unwrap();
        let decoded_wire: WireIntent = postcard::from_bytes(&bytes).unwrap();
        let decoded = decoded_wire.into_intent().unwrap();

        assert_eq!(decoded.perspective_seq, original.perspective_seq);
        assert_eq!(decoded.idempotency_key, original.idempotency_key);
        assert_append_eq(&decoded.append, &original.append);
    }

    /// The aux dispatcher answers `next_pending` from a populated store, and
    /// the postcard bytes decode to that seq's HLC.
    #[tokio::test]
    async fn dispatcher_next_pending_returns_lowest_seq() {
        let store = Arc::new(InMemIntentStore::new());
        store.put(rich_intent(seq(5, 0, 1), None)).unwrap();
        store.put(rich_intent(seq(2, 0, 1), None)).unwrap();
        let dispatch = build_intent_dispatcher(store);

        let outcome = dispatch(INTENT_NEXT_PENDING_TAG, &[]).await;
        let DispatchOutcome::Ok(bytes) = outcome else {
            panic!("expected Ok");
        };
        let decoded: Option<HybridLogicalClock> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, Some(seq(2, 0, 1).0), "lowest pending seq");
    }

    /// An empty store's `next_pending` answers `None`.
    #[tokio::test]
    async fn dispatcher_next_pending_empty_is_none() {
        let store = Arc::new(InMemIntentStore::new());
        let dispatch = build_intent_dispatcher(store);
        let outcome = dispatch(INTENT_NEXT_PENDING_TAG, &[]).await;
        let DispatchOutcome::Ok(bytes) = outcome else {
            panic!("expected Ok");
        };
        let decoded: Option<HybridLogicalClock> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, None);
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
        let dispatch = build_intent_dispatcher(store);

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

    /// An unknown tag falls through to `UnknownTag` (the listener maps it to
    /// the same `ParseError` wire status a truly-unknown tag gets).
    #[tokio::test]
    async fn dispatcher_unknown_tag_falls_through() {
        let store = Arc::new(InMemIntentStore::new());
        let dispatch = build_intent_dispatcher(store);
        let outcome = dispatch("not_an_intent_tag", &[]).await;
        assert!(matches!(outcome, DispatchOutcome::UnknownTag));
    }

    /// The two intent tags do not collide with the Raft tags.
    #[test]
    fn intent_tags_distinct_from_raft_tags() {
        for raft_tag in ["append_entries", "vote", "full_snapshot"] {
            assert_ne!(INTENT_NEXT_PENDING_TAG, raft_tag);
            assert_ne!(INTENT_GATHER_PENDING_TAG, raft_tag);
        }
        assert_ne!(INTENT_NEXT_PENDING_TAG, INTENT_GATHER_PENDING_TAG);
    }
}
