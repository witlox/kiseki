#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Concurrent-`GetFragment` deadline test — repro target for the
//! 2026-05-08 multi-node read-hang observation. `NFSv3` 256 MiB read,
//! FUSE-via-native 256 MiB read, and FUSE-via-HTTP 256 MiB read all
//! intermittently hung indefinitely on the local 3-node compose, with
//! no protocol-level error and no server-side log entries — classic
//! "one chunk fetch silently stalls and the kernel-driven readahead
//! piles every other in-flight read behind it" symptom.
//!
//! This test bypasses every kernel-side abstraction (no FUSE, no NFS,
//! no kernel page cache) and drives `FabricPeer::get_fragment` over
//! the same `tonic::transport::Channel` shape the production gateway
//! uses, against three real `ClusterChunkServer` instances on
//! ephemeral local TCP. If the hang reproduces here, the fix sits in
//! the gateway/fabric layer; if it doesn't, the kernel-side dispatch
//! is implicated.
//!
//! Asserts: 96 concurrent `GetFragment` calls (32 per peer × 3 peers)
//! against pre-populated 64 MiB envelopes finish in under 30 s. The
//! deadline is generous; a healthy fabric does this in a couple of
//! seconds. A regression that re-introduces the hang panics with
//! "deadline exceeded" instead of running forever.
//!
//! Pattern mirrors `grpc_high_rtt.rs` — same in-process
//! `ClusterChunkServer` + `GrpcFabricPeer` stack, no privileged
//! containers, no kernel FUSE state.

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::peer::FabricPeer;
use kiseki_chunk_cluster::{ClusterChunkServer, GrpcFabricPeer};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server, Uri};

/// 64 MiB envelopes match the gateway's `MAX_PLAINTEXT_PER_CHUNK`
/// so the wire frame size is realistic. Smaller envelopes pack
/// many fragments into TCP buffers and may not exercise the same
/// flow-control paths as the production read-256-MiB-file pattern.
const ENVELOPE_BYTES: usize = 64 * 1024 * 1024;

/// Per-peer fragment count. With 3 peers × 8 = 24 distinct chunks,
/// the read pattern walks across all of them.
const FRAGS_PER_PEER: usize = 8;

/// Concurrency factor. 32 in-flight per peer × 3 peers = 96 concurrent
/// `GetFragment` calls. The kernel-readahead pattern that triggered
/// the production hang issues ~16 in-flight per file; a 4-file dd
/// fan-out matches the 64 here.
const CONCURRENT_PER_PEER: usize = 32;

/// Deadline after which we declare the fabric hung. A healthy 3-peer
/// in-process fabric serves 96 × 64 MiB through the H2 stream
/// machinery in ~10 s on a 16-core laptop. On shared 2-vCPU CI
/// runners (GitHub Actions ubuntu-latest) the same workload runs in
/// 30-50 s — scheduler pressure across 96 concurrent tasks + 6 GiB
/// of crypto. The 120 s ceiling is well above any healthy timing
/// (laptop or runner) while still bounded so a true hang regression
/// (h2 stream stall, fabric deadlock) doesn't hang the whole test
/// runner indefinitely.
const DEADLINE: Duration = Duration::from_secs(120);

fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
    let store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: pool.to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: 1 << 34, // 16 GiB — fits 24 × 64 MiB with headroom
        used_bytes: 0,
        ..Default::default()
    });
    Arc::new(SyncBridge::new(store))
}

/// 64 MiB envelope with `chunk_id` derived from `(peer_idx, frag_idx)`.
/// Distinct ids ensure each `get_fragment` returns its own bytes
/// (no cache amalgamation).
fn make_envelope(peer_idx: u8, frag_idx: u8) -> Envelope {
    let mut id = [0u8; 32];
    id[0] = peer_idx;
    id[1] = frag_idx;
    Envelope {
        chunk_id: ChunkId(id),
        // Fill with a peer-distinguishable byte so a regression that
        // returns the wrong fragment under concurrency is visible.
        ciphertext: vec![peer_idx ^ frag_idx; ENVELOPE_BYTES],
        auth_tag: [0u8; 16],
        nonce: [0u8; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

/// Spin up one `ClusterChunkServer` on an ephemeral port; return a
/// connected `GrpcFabricPeer` plus the local-bridge handle and a
/// graceful-shutdown sender so the caller can drain streams before
/// the test exits.
///
/// The shutdown sender matters: without it, when the test function
/// returns the tokio runtime aborts the spawned server task
/// mid-stream and h2's `Counts` drops while still tracking streams.
/// In debug builds (CI's `cargo test --profile slow` default) that
/// trips a `debug_assert!` in h2 0.4.13's `counts.rs:282` and
/// panics in a tokio worker — the symptom seen on shared CI runners
/// where scheduler pressure makes the abort race deterministic.
async fn start_peer(
    name: &str,
    pool: &str,
) -> (
    Arc<dyn AsyncChunkOps>,
    Arc<GrpcFabricPeer>,
    tokio::sync::oneshot::Sender<()>,
) {
    let local = local_bridge(pool);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = TcpListenerStream::new(listener);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        Server::builder()
            // Match the production fabric Server settings — without
            // these, concurrent 64 MiB streams stall on H2 flow
            // control and produce the very hang we're testing for as
            // a *spurious* failure rather than a real one. Shipped in
            // commit f362060 (kiseki-server runtime), pinned by the
            // grpc_high_rtt test as well.
            .initial_stream_window_size(Some(16 * 1024 * 1024))
            .initial_connection_window_size(Some(32 * 1024 * 1024))
            .max_frame_size(Some(64 * 1024))
            .add_service(
                ClusterChunkServiceServer::new(server)
                    // Tonic defaults to 4 MiB; production gateway
                    // bumps to 64 MiB to fit one MAX_PLAINTEXT_PER_CHUNK
                    // envelope per stream. Mirror that here.
                    .max_decoding_message_size(256 * 1024 * 1024)
                    .max_encoding_message_size(256 * 1024 * 1024),
            )
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "slow: 3 × 8 × 64 MiB pre-populate + 96 × 64 MiB concurrent fetch ≈ 6 GiB total"]
async fn concurrent_get_fragment_completes_within_deadline() {
    // 1. Bring up three real chunk-cluster servers.
    let mut peers: Vec<Arc<GrpcFabricPeer>> = Vec::with_capacity(3);
    let mut shutdowns: Vec<tokio::sync::oneshot::Sender<()>> = Vec::with_capacity(3);
    let tenant = OrgId(uuid::Uuid::nil());
    for peer_idx in 0..3u8 {
        let (_local, peer, shutdown_tx) = start_peer(&format!("peer-{peer_idx}"), "p").await;
        // Pre-populate FRAGS_PER_PEER envelopes per peer.
        for frag_idx in 0..FRAGS_PER_PEER {
            let frag_idx_u8 = u8::try_from(frag_idx).expect("FRAGS_PER_PEER < 256");
            let env = make_envelope(peer_idx, frag_idx_u8);
            peer.put_fragment(env.chunk_id, 0, tenant, "p".into(), env)
                .await
                .expect("seed put");
        }
        peers.push(peer);
        shutdowns.push(shutdown_tx);
    }

    // 2. Build the work plan. Each per-peer slot fires
    //    CONCURRENT_PER_PEER concurrent `get_fragment` futures
    //    targeting that peer's pre-populated chunk_ids
    //    round-robin. Total in-flight = 3 × CONCURRENT_PER_PEER.
    let mut handles = Vec::with_capacity(3 * CONCURRENT_PER_PEER);
    let work_started = std::time::Instant::now();
    for (peer_idx, peer) in peers.iter().enumerate() {
        let peer_idx_u8 = u8::try_from(peer_idx).expect("3 peers < 256");
        for slot in 0..CONCURRENT_PER_PEER {
            let peer = Arc::clone(peer);
            let frag_idx = u8::try_from(slot % FRAGS_PER_PEER).expect("FRAGS_PER_PEER < 256");
            let mut chunk_id = [0u8; 32];
            chunk_id[0] = peer_idx_u8;
            chunk_id[1] = frag_idx;
            handles.push(tokio::spawn(async move {
                peer.get_fragment(ChunkId(chunk_id), 0).await
            }));
        }
    }

    // 3. Bounded await. If the fabric is healthy this completes well
    //    before the deadline; if it hangs, the deadline fires and we
    //    panic with the count of completed-vs-stuck calls so the
    //    failure message itself characterizes which peers stuck.
    let work = async {
        let mut completed = vec![0usize; 3];
        let mut errors = Vec::new();
        for (idx, h) in handles.into_iter().enumerate() {
            let peer_idx = idx / CONCURRENT_PER_PEER;
            match h.await {
                Ok(Ok(env)) => {
                    let want_seed = u8::try_from(peer_idx).expect("3 peers")
                        ^ u8::try_from((idx % CONCURRENT_PER_PEER) % FRAGS_PER_PEER)
                            .expect("FRAGS_PER_PEER");
                    if env.ciphertext.first().copied() != Some(want_seed) {
                        errors.push(format!(
                            "ciphertext seed mismatch at idx={idx}: want {want_seed:#x}, got {:?}",
                            env.ciphertext.first(),
                        ));
                    }
                    completed[peer_idx] += 1;
                }
                Ok(Err(e)) => {
                    errors.push(format!("peer={peer_idx} idx={idx} get_fragment err: {e}"));
                }
                Err(e) => errors.push(format!("peer={peer_idx} idx={idx} join err: {e}")),
            }
        }
        (completed, errors)
    };

    let outcome = tokio::time::timeout(DEADLINE, work).await;
    let elapsed = work_started.elapsed();
    let Ok((completed, errors)) = outcome else {
        panic!(
            "concurrent get_fragment hung past {DEADLINE:?} \
             — same shape as the 2026-05-08 NFS / FUSE 256 MiB read hangs. \
             Inspect kiseki-chunk-cluster fabric or H2 settings.",
        );
    };
    assert!(
        errors.is_empty(),
        "concurrent get_fragment surfaced errors after {elapsed:?}:\n  {}",
        errors.join("\n  "),
    );
    assert_eq!(
        completed,
        vec![CONCURRENT_PER_PEER; 3],
        "per-peer completion mismatch",
    );
    // Sanity floor — if any one peer is starved, total time grows
    // without bound. The bound is loose; the strict check is the
    // deadline above.
    assert!(
        elapsed < DEADLINE,
        "completed but slower than deadline ({elapsed:?})",
    );

    // Drop all client channels first so the server side sees clean
    // GOAWAY and drains stream state, then signal each server to
    // gracefully shut down. Without this, the test exit aborts
    // server tasks mid-stream and h2 0.4.13's `Counts::drop`
    // `debug_assert!(!self.has_streams())` panics in a tokio
    // worker — symptom seen on shared CI runners (2 vCPU GitHub
    // Actions) where scheduler pressure makes the abort race
    // deterministic.
    drop(peers);
    for tx in shutdowns {
        let _ = tx.send(());
    }
    // Brief grace period so the spawned server tasks observe the
    // shutdown signal and finish their drain before the test
    // function returns and the runtime tears down.
    tokio::time::sleep(Duration::from_millis(200)).await;
}
