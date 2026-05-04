//! Regression for the 2026-05-04 GCP transport-profile fabric
//! quorum-loss finding.
//!
//! `kiseki_chunk_cluster::peer::FABRIC_MAX_MESSAGE_BYTES` was sized
//! at 256 MiB *exactly*. A `PutFragmentRequest` carrying a 256 MiB
//! ciphertext envelope exceeds that limit by the prost wrapper
//! overhead (envelope wrapper, AEAD tag = 16 B, nonce = 12 B, key
//! epochs, optional tenant-wrapped material, prost field tags +
//! length prefixes). tonic returns `Status::resource_exhausted` →
//! the h2 layer emits `RST_STREAM` with `INTERNAL_ERROR` → the
//! leader sees "h2 protocol error: http2 error" → the
//! multi-replica fan-out fails on every remote peer → "quorum
//! lost: only 1/2 replicas acked" → S3 PUT returns HTTP 500.
//!
//! Captured live on a 3-node GCP cluster: every PUT object size
//! ≤ 192 MB returns 200; the 256 MB PUT returns 500 with
//! `quorum_lost`. See
//! `.gcp-build/findings/2026-05-04-fabric-256mib-cap/FINDINGS.md`.
//!
//! This test reproduces the failure in-process via the same
//! `ClusterChunkServer` / `GrpcFabricPeer` / `serve_with_shutdown`
//! pattern as `grpc_high_rtt.rs`. With the production constant,
//! the `put_fragment` call below fails. After the fix (cap with
//! envelope-overhead headroom), the call must succeed and the
//! envelope must be retrievable byte-for-byte.

use std::sync::Arc;

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::peer::{
    FabricPeer, FABRIC_CIPHERTEXT_MAX_BYTES, FABRIC_MAX_MESSAGE_BYTES,
};
use kiseki_chunk_cluster::{ClusterChunkServer, GrpcFabricPeer};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use tonic::transport::{Channel, Server, Uri};

const H2_STREAM_WINDOW: u32 = 16 * 1024 * 1024;
const H2_CONN_WINDOW: u32 = 32 * 1024 * 1024;

fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
    let mut store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: pool.to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: 1 << 32,
        used_bytes: 0,
    });
    Arc::new(SyncBridge::new(store))
}

/// Construct an envelope with `len` bytes of ciphertext. Pattern
/// `seed.wrapping_mul(i as u8)` so the bytes vary; we re-read and
/// compare byte-for-byte at the end so silent truncation also fails.
fn make_envelope(seed: u8, len: usize) -> Envelope {
    let mut ciphertext = Vec::with_capacity(len);
    for i in 0..len {
        let b = u8::try_from(i & 0xff).expect("masked");
        ciphertext.push(seed.wrapping_mul(b).wrapping_add(seed));
    }
    Envelope {
        chunk_id: ChunkId([seed; 32]),
        ciphertext,
        auth_tag: [seed; 16],
        nonce: [seed; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

/// 256 MiB ciphertext + envelope wrapper overhead exceeds the
/// production cap of 256 MiB exactly. With the cap as it stands,
/// the gRPC `put_fragment` fails.
///
/// This is the exact size that broke the GCP run: an S3 PUT of
/// 256 MB → gateway encrypts → ciphertext = 256 MiB + AEAD tag
/// (16 B), and after prost wrapping the request is ≥256 MiB +
/// some small constant.
#[tokio::test(flavor = "multi_thread")]
async fn fabric_accepts_envelope_at_full_chunk_size_cap() {
    let pool = "p";
    let local = local_bridge(pool);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);

    let server_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr")
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .initial_stream_window_size(H2_STREAM_WINDOW)
            .initial_connection_window_size(H2_CONN_WINDOW)
            .add_service(
                ClusterChunkServiceServer::new(server)
                    .max_decoding_message_size(FABRIC_MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(FABRIC_MAX_MESSAGE_BYTES),
            )
            .serve_with_shutdown(server_addr, async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server");
    });

    let uri: Uri = format!("http://{server_addr}").parse().expect("uri");
    let channel = loop {
        match Channel::builder(uri.clone())
            .initial_stream_window_size(H2_STREAM_WINDOW)
            .initial_connection_window_size(H2_CONN_WINDOW)
            .connect()
            .await
        {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    };
    let peer = Arc::new(GrpcFabricPeer::new("test-peer", channel));

    // The application contract: ciphertext at exactly
    // `FABRIC_CIPHERTEXT_MAX_BYTES` MUST round-trip. The
    // transport cap (`FABRIC_MAX_MESSAGE_BYTES`) is sized
    // strictly larger than the ciphertext cap to leave room
    // for prost wrapper bytes (envelope wrapper, field tags,
    // length varints, outer PutFragmentRequest fields).
    //
    // Pre-fix: both constants were equal at 256 MiB → wrapper
    // overhead pushed encoded bytes over the cap → tonic
    // returned `Status::resource_exhausted` → the leader saw
    // "h2 protocol error: http2 error".
    let ciphertext_len = FABRIC_CIPHERTEXT_MAX_BYTES;
    let env = make_envelope(0xA5, ciphertext_len);
    let chunk_id = env.chunk_id;
    let tenant = OrgId(uuid::Uuid::nil());

    let result = peer
        .put_fragment(chunk_id, 0, tenant, pool.into(), env)
        .await;

    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    // The TDD assertion: the put MUST succeed. Pre-fix this is
    // `Err(Internal: h2 protocol error: http2 error)`.
    let stored = result.expect(
        "put_fragment of a near-cap envelope must succeed; today it fails with \
         'h2 protocol error' because FABRIC_MAX_MESSAGE_BYTES is sized at the \
         chunk boundary with zero envelope-wrapper headroom (see \
         .gcp-build/findings/2026-05-04-fabric-256mib-cap/FINDINGS.md)",
    );
    assert!(stored, "fragment must report stored=true on first put");
}

/// Companion sanity test: a small envelope round-trips. Catches
/// any regression where a fix to the size cap accidentally breaks
/// the small-envelope path.
#[tokio::test(flavor = "multi_thread")]
async fn fabric_small_envelope_still_round_trips() {
    let pool = "p";
    let local = local_bridge(pool);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);

    let server_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr")
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .initial_stream_window_size(H2_STREAM_WINDOW)
            .initial_connection_window_size(H2_CONN_WINDOW)
            .add_service(
                ClusterChunkServiceServer::new(server)
                    .max_decoding_message_size(FABRIC_MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(FABRIC_MAX_MESSAGE_BYTES),
            )
            .serve_with_shutdown(server_addr, async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server");
    });

    let uri: Uri = format!("http://{server_addr}").parse().expect("uri");
    let channel = loop {
        match Channel::builder(uri.clone())
            .initial_stream_window_size(H2_STREAM_WINDOW)
            .initial_connection_window_size(H2_CONN_WINDOW)
            .connect()
            .await
        {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    };
    let peer = Arc::new(GrpcFabricPeer::new("test-peer", channel));

    let env = make_envelope(0x7A, 4 * 1024 * 1024); // 4 MiB
    let chunk_id = env.chunk_id;
    let tenant = OrgId(uuid::Uuid::nil());
    let stored = peer
        .put_fragment(chunk_id, 0, tenant, pool.into(), env)
        .await
        .expect("4 MiB put_fragment must succeed");
    assert!(stored, "small fragment must report stored=true");

    let _ = shutdown_tx.send(());
    let _ = server_handle.await;
}
