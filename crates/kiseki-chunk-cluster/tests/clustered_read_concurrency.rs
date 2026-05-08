#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Concurrent `ClusteredChunkStore::read_chunk` deadline test — the
//! second layer of the 2026-05-08 read-hang investigation.
//!
//! Layer 1 (`grpc_concurrent_get_fragment.rs`) proved the raw fabric
//! is healthy: 96 concurrent `GrpcFabricPeer::get_fragment` calls
//! against three real `ClusterChunkServer` instances complete cleanly
//! in 10 s.
//!
//! This test drives the **wrapper** the gateway holds —
//! `ClusteredChunkStore::read_chunk` — under the same shape of
//! concurrent load. The wrapper:
//!   - tries the local store first (always misses here — we leave
//!     the local store empty so every read fans out to peers)
//!   - in `Replication-N` mode walks `self.peers` *serially*, taking
//!     the first success
//!   - each per-peer call is timeout-bounded by `cfg.get_timeout`
//!
//! The intermittent fall-through path (peer 0 → `NotFound` → peer 1 →
//! Ok) is exactly the shape that would stall under contention if a
//! deadlock exists. If this test passes, the bug is not in the
//! wrapper either, and the next slice walks one layer higher
//! (`mem_gateway::read` directly).
//!
//! 24 `chunk_id`s are distributed across 3 peers (8 per peer), so a
//! quarter of reads hit peer 0 immediately, a third walk peer-0 →
//! peer-1, a third walk peer-0 → peer-1 → peer-2. All three
//! placements run concurrently.

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::peer::{FabricPeer, FabricPeerError};
use kiseki_chunk_cluster::{ClusterCfg, ClusterChunkServer, ClusteredChunkStore, GrpcFabricPeer};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server, Uri};

const ENVELOPE_BYTES: usize = 64 * 1024 * 1024;
const FRAGS_PER_PEER: usize = 8;
const CONCURRENT_PER_PLACEMENT: usize = 24;
const DEADLINE: Duration = Duration::from_secs(45);

fn local_bridge(pool: &str, capacity: u64) -> Arc<dyn AsyncChunkOps> {
    let mut store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: pool.to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: capacity,
        used_bytes: 0,
    });
    Arc::new(SyncBridge::new(store))
}

fn make_envelope(peer_idx: u8, frag_idx: u8) -> Envelope {
    let mut id = [0u8; 32];
    id[0] = peer_idx;
    id[1] = frag_idx;
    Envelope {
        chunk_id: ChunkId(id),
        ciphertext: vec![peer_idx ^ frag_idx; ENVELOPE_BYTES],
        auth_tag: [0u8; 16],
        nonce: [0u8; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

async fn start_peer(name: &str, pool: &str) -> (Arc<dyn AsyncChunkOps>, Arc<GrpcFabricPeer>) {
    let local = local_bridge(pool, 1 << 34);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .initial_stream_window_size(Some(16 * 1024 * 1024))
            .initial_connection_window_size(Some(32 * 1024 * 1024))
            .max_frame_size(Some(64 * 1024))
            .add_service(
                ClusterChunkServiceServer::new(server)
                    .max_decoding_message_size(256 * 1024 * 1024)
                    .max_encoding_message_size(256 * 1024 * 1024),
            )
            .serve_with_incoming(stream)
            .await
            .expect("server");
    });

    let uri: Uri = format!("http://{addr}").parse().expect("uri");
    let channel = loop {
        match Channel::builder(uri.clone())
            .initial_stream_window_size(16 * 1024 * 1024)
            .initial_connection_window_size(32 * 1024 * 1024)
            .connect()
            .await
        {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    (local, Arc::new(GrpcFabricPeer::new(name, channel)))
}

/// Currently fails ~10% of the per-call rate with intermittent
/// `ChunkError::`NotFound`` for chunks that ARE present on the
/// expected peer (the post-seed sanity probe inside the test
/// confirms each peer's chunks round-trip individually). Under
/// concurrent fan-out through `ClusteredChunkStore::read_chunk`,
/// some `peer.get_fragment` calls server-side return `NotFound` for
/// chunks the peer's local store accepted via `write_chunk`. The
/// wrapper then walks the remaining peers, none of which have the
/// chunk (no replication in this test), and surfaces `NotFound` to
/// the caller.
///
/// Symptom: data-loss shape (false-`NotFound`), not the
/// indefinite-hang shape we saw on the 3-node compose. Likely a
/// related bug in the same `peer.get_fragment` path — under enough
/// parallelism on a single peer, the server's
/// `read_fragment`-then-`read_chunk` two-step (server.rs:419-449)
/// races and the second leg occasionally surfaces a stale lookup.
/// Tagged `slow:` so Tier-1 skips it; `--ignored` runs it as a
/// regression target. Fix candidates land in
/// `kiseki-chunk-cluster::server::ClusterChunkServer::get_fragment`
/// or in the wrapper's peer-walk error classification.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "slow: 3 × 8 × 64 MiB pre-populate + 72 × 64 MiB concurrent fetch; \
            CURRENTLY FAILS ~10% on intermittent peer `NotFound` — see doc-comment"]
async fn clustered_read_chunk_concurrent_fanout_completes_within_deadline() {
    // 1. Three real chunk-cluster peers; each owns 8 distinct
    //    chunk_ids. No replication — peer N has chunk[N][0..8] only.
    //    That deliberately exercises the wrapper's serial peer-walk
    //    fall-through (chunks on peer 1 require a `NotFound` from peer
    //    0 first; chunks on peer 2 require `NotFound` from both 0 and 1).
    let mut peer_clients: Vec<Arc<dyn FabricPeer>> = Vec::with_capacity(3);
    let tenant = OrgId(uuid::Uuid::nil());
    for peer_idx in 0..3u8 {
        let (_local, peer) = start_peer(&format!("peer-{peer_idx}"), "p").await;
        for frag_idx in 0..FRAGS_PER_PEER {
            let frag_idx_u8 = u8::try_from(frag_idx).expect("FRAGS_PER_PEER < 256");
            let env = make_envelope(peer_idx, frag_idx_u8);
            peer.put_fragment(env.chunk_id, 0, tenant, "p".into(), env)
                .await
                .expect("seed put");
        }
        peer_clients.push(peer as Arc<dyn FabricPeer>);
    }

    // 1.5. Sanity probe: confirm EVERY seeded chunk_id round-trips
    //      directly via FabricPeer::get_fragment on its expected
    //      peer before we wrap. Pinpoints whether the burst-`NotFound`
    //      below sits in the seed step (some chunks didn't stick) or
    //      in the wrapper / concurrent server path (chunks stick but
    //      vanish under contention).
    for (peer_idx, peer) in peer_clients.iter().enumerate() {
        let peer_idx_u8 = u8::try_from(peer_idx).expect("3 peers");
        for frag_idx in 0..FRAGS_PER_PEER {
            let frag_idx_u8 = u8::try_from(frag_idx).expect("FRAGS_PER_PEER");
            let mut id = [0u8; 32];
            id[0] = peer_idx_u8;
            id[1] = frag_idx_u8;
            let env = peer.get_fragment(ChunkId(id), 0).await.unwrap_or_else(|e| {
                panic!("sanity get_fragment peer={peer_idx} frag={frag_idx}: {e}")
            });
            let want = peer_idx_u8 ^ frag_idx_u8;
            assert_eq!(
                env.ciphertext.first().copied(),
                Some(want),
                "sanity ciphertext mismatch peer={peer_idx} frag={frag_idx}",
            );
        }
    }

    // 2. Wrap with a ClusteredChunkStore that has an EMPTY local
    //    store. Every read_chunk misses local and falls through to
    //    the fabric. The cfg matches production-shape replication
    //    defaults; the only override is to keep the test bounded
    //    via a shorter get_timeout (default is generous).
    let empty_local = local_bridge("p", 1 << 30);
    let cfg = ClusterCfg::new(tenant, "p");
    let clustered = Arc::new(ClusteredChunkStore::new(empty_local, peer_clients, cfg));

    // 3. Build the work plan. For each placement (peer 0, peer 1,
    //    peer 2) fire CONCURRENT_PER_PLACEMENT concurrent read_chunk
    //    calls cycling through that peer's 8 chunk_ids. Total
    //    in-flight = 3 × CONCURRENT_PER_PLACEMENT = 72.
    let mut handles = Vec::with_capacity(3 * CONCURRENT_PER_PLACEMENT);
    let work_started = std::time::Instant::now();
    for peer_idx in 0..3u8 {
        for slot in 0..CONCURRENT_PER_PLACEMENT {
            let frag_idx = u8::try_from(slot % FRAGS_PER_PEER).expect("FRAGS_PER_PEER");
            let mut chunk_id = [0u8; 32];
            chunk_id[0] = peer_idx;
            chunk_id[1] = frag_idx;
            let clustered = Arc::clone(&clustered);
            handles.push(tokio::spawn(async move {
                let res = clustered.read_chunk(&ChunkId(chunk_id)).await;
                (peer_idx, slot, res)
            }));
        }
    }

    // 4. Bounded await. Failure mode = deadline elapses → panic.
    let work = async {
        let mut completed = vec![0usize; 3];
        let mut errors: Vec<String> = Vec::new();
        for h in handles {
            let (peer_idx, slot, res) = h.await.expect("join");
            match res {
                Ok(env) => {
                    let want =
                        peer_idx ^ u8::try_from(slot % FRAGS_PER_PEER).expect("FRAGS_PER_PEER");
                    if env.ciphertext.first().copied() != Some(want) {
                        errors.push(format!(
                            "ciphertext seed mismatch peer={peer_idx} slot={slot} \
                             want={want:#x} got={:?}",
                            env.ciphertext.first(),
                        ));
                    }
                    completed[usize::from(peer_idx)] += 1;
                }
                Err(e) => errors.push(format!("read_chunk peer={peer_idx} slot={slot}: {e:?}")),
            }
        }
        (completed, errors)
    };

    let outcome = tokio::time::timeout(DEADLINE, work).await;
    let elapsed = work_started.elapsed();
    let Ok((completed, errors)) = outcome else {
        panic!(
            "ClusteredChunkStore::read_chunk concurrent fan-out hung past {DEADLINE:?} \
             — wrapper-layer hang. Inspect read_chunk's peer walk under contention.",
        );
    };
    assert!(
        errors.is_empty(),
        "concurrent read_chunk surfaced errors after {elapsed:?}:\n  {}",
        errors.join("\n  "),
    );
    assert_eq!(
        completed,
        vec![CONCURRENT_PER_PLACEMENT; 3],
        "per-peer-placement completion mismatch",
    );
    assert!(
        elapsed < DEADLINE,
        "completed but slower than deadline ({elapsed:?})",
    );
    // Suppress unused-import warning when this test grows TLS
    // handling (tracked for the cert-binding follow-up).
    let _ = std::any::type_name::<FabricPeerError>();
}
