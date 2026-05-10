#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Single-peer concurrent `get_fragment` with mixed present/absent
//! `chunk_id`s. Layer-1 (`grpc_concurrent_get_fragment`) drove only
//! present-chunk reads at 32/peer concurrency and passed cleanly.
//! Layer-2 (`clustered_read_concurrency`) drives 72 concurrent reads
//! at peer 0 where ~⅔ target absent chunks (fall-through walks the
//! wrapper picks up via the serial peer-loop) and FAILS with
//! intermittent false-`NotFound` for present chunks.
//!
//! This test isolates the difference. One peer, 8 present chunks
//! pre-populated, plus N concurrent reads where exactly half hit
//! present `chunk_id`s and half hit absent `chunk_id`s. If the
//! bug reproduces, it's purely server-side concurrency under
//! mixed-presence load. If it doesn't, the wrapper's peer-walk
//! is implicated.

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::peer::{FabricPeer, FabricPeerError};
use kiseki_chunk_cluster::{ClusterChunkServer, GrpcFabricPeer};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server, Uri};

const ENVELOPE_BYTES: usize = 64 * 1024 * 1024;
const PRESENT_CHUNKS: usize = 8;
const CONCURRENT_READS: usize = 64;
const DEADLINE: Duration = Duration::from_secs(45);

fn local_bridge() -> Arc<dyn AsyncChunkOps> {
    let mut store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: "p".to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: 1 << 33,
        used_bytes: 0,
    });
    Arc::new(SyncBridge::new(store))
}

/// Mark `chunk_id` with the kind in byte[0] (`'P'` = present, `'A'` =
/// absent) and an index in byte[1]. Distinct bytes mean a wrong-
/// chunk response is visible as a ciphertext-seed mismatch.
fn make_chunk_id(kind: u8, idx: u8) -> ChunkId {
    let mut id = [0u8; 32];
    id[0] = kind;
    id[1] = idx;
    ChunkId(id)
}

fn make_present_envelope(idx: u8) -> Envelope {
    Envelope {
        chunk_id: make_chunk_id(b'P', idx),
        ciphertext: vec![idx; ENVELOPE_BYTES],
        auth_tag: [0u8; 16],
        nonce: [0u8; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

async fn start_peer() -> (Arc<GrpcFabricPeer>, tokio::sync::oneshot::Sender<()>) {
    let local = local_bridge();
    let server = ClusterChunkServer::new(Arc::clone(&local), "p");
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
            // serve_with_incoming_shutdown so streams drain cleanly
            // when the test ends — see h2 0.4.13 `counts.rs:282`
            // debug_assert! note in `grpc_concurrent_get_fragment.rs`.
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
    let _ = local; // keep alive — server holds its own Arc::clone
    (
        Arc::new(GrpcFabricPeer::new("peer-0", channel)),
        shutdown_tx,
    )
}

type BurstResult = (usize, bool, u8, Result<Envelope, FabricPeerError>);

/// Classify burst results into the four buckets the test cares
/// about. Pulled out of the test body so the per-call match arms
/// don't blow the test fn over the 100-line clippy ceiling.
async fn classify(
    handles: Vec<tokio::task::JoinHandle<BurstResult>>,
) -> (usize, usize, usize, Vec<String>) {
    let mut ok_present = 0usize;
    let mut not_found_present = 0usize;
    let mut not_found_absent = 0usize;
    let mut other_errors: Vec<String> = Vec::new();
    for h in handles {
        let (slot, is_present, idx, res) = h.await.expect("join");
        match (is_present, res) {
            (true, Ok(env)) => {
                if env.ciphertext.first().copied() != Some(idx) {
                    other_errors.push(format!(
                        "slot={slot} present idx={idx} ciphertext mismatch: {:?}",
                        env.ciphertext.first(),
                    ));
                }
                ok_present += 1;
            }
            (true, Err(FabricPeerError::NotFound)) => not_found_present += 1,
            (false, Err(FabricPeerError::NotFound)) => not_found_absent += 1,
            (true, Err(e)) => {
                other_errors.push(format!("slot={slot} present non-NotFound: {e}"));
            }
            (false, Ok(_)) => other_errors.push(format!("slot={slot} absent unexpectedly Ok")),
            (false, Err(e)) => other_errors.push(format!("slot={slot} absent: {e}")),
        }
    }
    (
        ok_present,
        not_found_present,
        not_found_absent,
        other_errors,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "slow: 8 × 64 MiB pre-populate + 64 mixed-presence concurrent fetch ≈ 2.5 GiB"]
async fn mixed_presence_concurrent_does_not_false_not_found() {
    let (peer, shutdown_tx) = start_peer().await;
    let tenant = OrgId(uuid::Uuid::nil());

    // 1. Pre-populate 8 present chunks on the peer.
    for idx in 0..PRESENT_CHUNKS {
        let env = make_present_envelope(u8::try_from(idx).expect("idx<8"));
        peer.put_fragment(env.chunk_id, 0, tenant, "p".into(), env)
            .await
            .expect("seed put");
    }

    // 2. Sanity: every present chunk round-trips serially.
    for idx in 0..PRESENT_CHUNKS {
        let idx_u8 = u8::try_from(idx).expect("idx");
        let env = peer
            .get_fragment(make_chunk_id(b'P', idx_u8), 0)
            .await
            .expect("sanity");
        assert_eq!(env.ciphertext.first().copied(), Some(idx_u8));
    }

    // 3. Concurrent burst: alternating present/absent chunk_ids.
    //    Even slot = present (chunk 0..8 cycling); odd slot = absent
    //    (different `kind` byte so it's distinct from any present id).
    let mut handles = Vec::with_capacity(CONCURRENT_READS);
    for slot in 0..CONCURRENT_READS {
        let peer = Arc::clone(&peer);
        let is_present = slot % 2 == 0;
        let idx = u8::try_from((slot / 2) % PRESENT_CHUNKS).expect("idx");
        let chunk_id = if is_present {
            make_chunk_id(b'P', idx)
        } else {
            make_chunk_id(b'A', idx)
        };
        handles.push(tokio::spawn(async move {
            let res = peer.get_fragment(chunk_id, 0).await;
            (slot, is_present, idx, res)
        }));
    }

    // 4. Bounded await + classify.
    let work = classify(handles);

    let outcome = tokio::time::timeout(DEADLINE, work).await;
    let Ok((ok_p, not_found_p, not_found_a, errors)) = outcome else {
        panic!("mixed-presence concurrent burst hung past {DEADLINE:?}");
    };

    let half = CONCURRENT_READS / 2;
    println!(
        "ok_present={ok_p}/{half} not_found_present(BUG)={not_found_p} \
         not_found_absent(expected)={not_found_a}/{half} \
         other_errors={}",
        errors.len(),
    );
    for e in &errors {
        eprintln!("  {e}");
    }
    assert!(
        errors.is_empty(),
        "non-NotFound errors surfaced: {} entries",
        errors.len(),
    );
    assert_eq!(
        not_found_a, half,
        "absent reads should all NotFound; got {not_found_a}",
    );
    assert_eq!(
        not_found_p, 0,
        "BUG: {not_found_p} present chunks falsely returned NotFound (sanity probe earlier confirmed all present)",
    );
    assert_eq!(ok_p, half, "present reads ok mismatch");

    // Drop the client first, then signal the server. h2 0.4.13
    // `counts.rs:282` debug_assert! avoidance — see the same
    // pattern in `grpc_concurrent_get_fragment.rs`.
    drop(peer);
    let _ = shutdown_tx.send(());
    tokio::time::sleep(Duration::from_millis(200)).await;
}
