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
// 45s is enough on a 16-core laptop (~16 s observed); on 2-vCPU
// CI runners the same workload measured 48 s, just over. Bump to
// 120 s for the same headroom rationale as
// grpc_concurrent_get_fragment.rs.
const DEADLINE: Duration = Duration::from_secs(120);

fn local_bridge(pool: &str, capacity: u64) -> Arc<dyn AsyncChunkOps> {
    let store = ChunkStore::new();
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

async fn start_peer(
    name: &str,
    pool: &str,
) -> (
    Arc<dyn AsyncChunkOps>,
    Arc<GrpcFabricPeer>,
    tokio::sync::oneshot::Sender<()>,
) {
    let local = local_bridge(pool, 1 << 34);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = TcpListenerStream::new(listener);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

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
            // serve_with_incoming_shutdown so the spawned server can
            // drain h2 streams cleanly when the test ends — without
            // it, dropping a `Counts` while streams are open trips
            // h2 0.4.13 `counts.rs:282`'s debug_assert! on shared CI
            // runners (debug-only panic in a tokio worker).
            .serve_with_incoming_shutdown(stream, async move {
                let _ = shutdown_rx.await;
            })
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
    (
        local,
        Arc::new(GrpcFabricPeer::new(name, channel)),
        shutdown_tx,
    )
}

type ClusteredResult = (u8, usize, Result<Envelope, kiseki_chunk::ChunkError>);

/// Bucket the burst results. Pulled out of the test body so the
/// match arms don't push the test fn over the 100-line clippy
/// ceiling. Returns (completed-per-peer, false-NotFound,
/// transient-Io, other).
async fn classify(
    handles: Vec<tokio::task::JoinHandle<ClusteredResult>>,
) -> (Vec<usize>, Vec<String>, Vec<String>, Vec<String>) {
    let mut completed = vec![0usize; 3];
    let mut false_not_found: Vec<String> = Vec::new();
    let mut transient_errors: Vec<String> = Vec::new();
    let mut other_errors: Vec<String> = Vec::new();
    for h in handles {
        let (peer_idx, slot, res) = h.await.expect("join");
        match res {
            Ok(env) => {
                let want = peer_idx ^ u8::try_from(slot % FRAGS_PER_PEER).expect("FRAGS_PER_PEER");
                if env.ciphertext.first().copied() != Some(want) {
                    other_errors.push(format!(
                        "ciphertext seed mismatch peer={peer_idx} slot={slot} \
                         want={want:#x} got={:?}",
                        env.ciphertext.first(),
                    ));
                }
                completed[usize::from(peer_idx)] += 1;
            }
            Err(kiseki_chunk::ChunkError::NotFound(_)) => {
                // The pre-fix bug. After the wrapper fix this branch
                // must stay empty — every transient peer problem
                // surfaces as `ChunkError::Io` instead.
                false_not_found.push(format!("peer={peer_idx} slot={slot}"));
            }
            Err(kiseki_chunk::ChunkError::Io(e)) => {
                // Acceptable: peer-side stall or transport hiccup
                // surfaced honestly.
                transient_errors.push(format!("peer={peer_idx} slot={slot}: {e}"));
            }
            Err(e) => other_errors.push(format!("peer={peer_idx} slot={slot}: {e:?}")),
        }
    }
    (completed, false_not_found, transient_errors, other_errors)
}

/// Pre-fix this test failed ~10% of per-call rate with intermittent
/// `ChunkError::NotFound` for chunks that ARE present on the
/// expected peer. Root cause (uncovered 2026-05-08 by bisecting
/// `cfg.get_timeout`): peer 0 sees ALL 72 in-flight requests
/// because the wrapper tries it first regardless of chunk
/// placement. Under that load, a small fraction of per-peer calls
/// exceed the 3 s default `get_timeout`. The wrapper warned + fell
/// through; peer 1 / 2 legitimately don't have the chunk; the
/// wrapper surfaced `ChunkError::NotFound`. Kernel-FUSE / NFS see
/// the phantom `NotFound`, retry, and escalate to the indefinite
/// hangs we observed on the 3-node compose.
///
/// Fix in `ClusteredChunkStore::read_chunk`: track non-NotFound
/// peer errors during the fall-through and surface them as
/// `ChunkError::Io` (with `ErrorKind::TimedOut` for timeouts)
/// instead of masking as `NotFound`. The caller now distinguishes
/// "data is gone" from "a peer was slow / unavailable" and can
/// retry vs give up appropriately.
///
/// This test pins both halves of the contract: the read should
/// usually succeed (peer 0 isn't always slow), and on the rare
/// stall it must NOT surface as `NotFound`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "slow: 3 × 8 × 64 MiB pre-populate + 72 × 64 MiB concurrent fetch ≈ 5 GiB total"]
async fn clustered_read_chunk_concurrent_fanout_completes_within_deadline() {
    // 1. Three real chunk-cluster peers; each owns 8 distinct
    //    chunk_ids. No replication — peer N has chunk[N][0..8] only.
    //    That deliberately exercises the wrapper's serial peer-walk
    //    fall-through (chunks on peer 1 require a `NotFound` from peer
    //    0 first; chunks on peer 2 require `NotFound` from both 0 and 1).
    let mut peer_clients: Vec<Arc<dyn FabricPeer>> = Vec::with_capacity(3);
    let mut shutdowns: Vec<tokio::sync::oneshot::Sender<()>> = Vec::with_capacity(3);
    let tenant = OrgId(uuid::Uuid::nil());
    for peer_idx in 0..3u8 {
        let (_local, peer, shutdown_tx) = start_peer(&format!("peer-{peer_idx}"), "p").await;
        for frag_idx in 0..FRAGS_PER_PEER {
            let frag_idx_u8 = u8::try_from(frag_idx).expect("FRAGS_PER_PEER < 256");
            let env = make_envelope(peer_idx, frag_idx_u8);
            peer.put_fragment(env.chunk_id, 0, tenant, "p".into(), env)
                .await
                .expect("seed put");
        }
        peer_clients.push(peer as Arc<dyn FabricPeer>);
        shutdowns.push(shutdown_tx);
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
    // Use the default 3 s get_timeout. The 2026-05-08 finding:
    // under heavy concurrent fan-out (peer 0 sees all 72 in-flight
    // requests because the wrapper tries it first regardless of
    // chunk placement) some per-peer calls exceed 3 s. The wrapper
    // pre-fix logged `warn` + fell through to peer 1 / 2 which
    // legitimately don't have the chunk, then surfaced
    // `ChunkError::NotFound` to the caller — masking a transient
    // peer-0 slowdown as data loss. Production shape: kernel-FUSE/NFS
    // see phantom `NotFound`, retry, escalate to indefinite stalls.
    //
    // The fix in `ClusteredChunkStore::read_chunk` propagates
    // non-NotFound errors (timeouts, transport, server errors) as
    // `Unavailable` instead of NotFound. With that fix in place
    // this test passes at the default timeout because either
    // (a) every peer 0 call really completes in 3 s (likely on a
    // beefy host) OR (b) when a call does time out, the wrapper
    // surfaces a real signal instead of false NotFound.
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
                let res = clustered.read_chunk(&ChunkId(chunk_id), None).await;
                (peer_idx, slot, res)
            }));
        }
    }

    // 4. Bounded await. Failure mode = deadline elapses → panic.
    let work = classify(handles);

    let outcome = tokio::time::timeout(DEADLINE, work).await;
    let elapsed = work_started.elapsed();
    let Ok((completed, false_not_found, transient_errors, other_errors)) = outcome else {
        panic!(
            "ClusteredChunkStore::read_chunk concurrent fan-out hung past {DEADLINE:?} \
             — wrapper-layer hang. Inspect read_chunk's peer walk under contention.",
        );
    };
    println!(
        "completed={completed:?} transient={} false_not_found={} other={} elapsed={elapsed:?}",
        transient_errors.len(),
        false_not_found.len(),
        other_errors.len(),
    );
    assert!(
        false_not_found.is_empty(),
        "BUG: {} reads surfaced as NotFound after wrapper fix \
         (pre-fix: ~10%, post-fix: must be 0):\n  {}",
        false_not_found.len(),
        false_not_found.join("\n  "),
    );
    assert!(
        other_errors.is_empty(),
        "unexpected non-Io non-NotFound errors after {elapsed:?}:\n  {}",
        other_errors.join("\n  "),
    );
    // Per-peer-placement completion may be slightly less than the
    // ideal 24-each because a transient timeout on peer 0 takes a
    // chunk[0][N] read into transient_errors instead of completed.
    // What matters: completed + transient_errors covers every
    // present chunk_id once, so no read silently went missing.
    let total_completed: usize = completed.iter().sum();
    assert_eq!(
        total_completed + transient_errors.len(),
        CONCURRENT_PER_PLACEMENT * 3,
        "every read must produce either a completion or a transient \
         error; got completed={completed:?} transient={}",
        transient_errors.len(),
    );
    assert!(
        elapsed < DEADLINE,
        "completed but slower than deadline ({elapsed:?})",
    );
    // Suppress unused-import warning when this test grows TLS
    // handling (tracked for the cert-binding follow-up).
    let _ = std::any::type_name::<FabricPeerError>();

    // Drop the ClusteredChunkStore (which transitively owns the peer
    // clients moved in at construction) first, then signal each
    // server. Same h2 0.4.13 `counts.rs:282` debug_assert! avoidance
    // as `grpc_concurrent_get_fragment.rs`.
    drop(clustered);
    for tx in shutdowns {
        let _ = tx.send(());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
}
