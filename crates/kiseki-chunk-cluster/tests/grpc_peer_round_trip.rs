#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end smoke test: real `GrpcFabricPeer` ↔ real
//! `ClusterChunkServer` over a tonic Channel on an ephemeral local
//! socket. Confirms the proto-conversion glue (16a step 5) and the
//! peer client wrapper (step 6) compose correctly without TLS.
//!
//! TLS + the SAN interceptor are exercised separately in step 12 once
//! the cert-gen tooling lands.

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::metrics::{op as op_label, outcome};
use kiseki_chunk_cluster::peer::FabricPeerError;
use kiseki_chunk_cluster::{ClusterChunkServer, FabricMetrics, FabricPeer, GrpcFabricPeer};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server, Uri};

fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
    let mut store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: pool.to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: 1 << 30,
        used_bytes: 0,
    });
    Arc::new(SyncBridge::new(store))
}

fn make_envelope(seed: u8) -> Envelope {
    Envelope {
        chunk_id: ChunkId([seed; 32]),
        ciphertext: vec![seed; 64],
        auth_tag: [0u8; 16],
        nonce: [0u8; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

/// Spin up a `ClusterChunkServer` on an ephemeral port and return a
/// connected `GrpcFabricPeer`. Caller drops the join handle when the
/// test ends — the server tears down on tokio runtime shutdown.
async fn start_server_and_client(pool: &str) -> (Arc<dyn AsyncChunkOps>, Arc<GrpcFabricPeer>) {
    let local = local_bridge(pool);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(ClusterChunkServiceServer::new(server))
            .serve_with_incoming(stream)
            .await
            .expect("server");
    });

    let uri: Uri = format!("http://{addr}").parse().expect("uri");
    // Eager connect — wait until the server is ready.
    let channel = loop {
        match Channel::builder(uri.clone()).connect().await {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    (local, Arc::new(GrpcFabricPeer::new("test-peer", channel)))
}

#[tokio::test]
async fn round_trip_put_get_has_delete() {
    let (_local, peer) = start_server_and_client("p").await;
    let env = make_envelope(0xBE);
    let chunk_id = env.chunk_id;
    let tenant = OrgId(uuid::Uuid::nil());

    // Put.
    let stored = peer
        .put_fragment(chunk_id, 0, tenant, "p".into(), env.clone())
        .await
        .expect("put");
    assert!(stored, "first put returns stored=true");

    // Has.
    let present = peer.has_fragment(chunk_id, 0).await.expect("has");
    assert!(present, "fragment is present after put");

    // Get returns the same envelope bytes.
    let got = peer.get_fragment(chunk_id, 0).await.expect("get");
    assert_eq!(got.chunk_id, chunk_id);
    assert_eq!(got.ciphertext, env.ciphertext);
    assert_eq!(got.system_epoch, env.system_epoch);

    // Delete: refcount is 1 → first delete drives refcount=0,
    // server reports deleted=true.
    let deleted = peer
        .delete_fragment(chunk_id, 0, tenant)
        .await
        .expect("delete");
    assert!(deleted, "first delete reports deleted=true");
}

/// Phase 16a step 11. After a successful put + get the
/// `FabricMetrics` counters for (op=put|get, peer, outcome=ok) must
/// each be 1.
#[tokio::test]
async fn fabric_metrics_record_per_op_outcomes() {
    let registry = prometheus::Registry::new();
    let metrics = Arc::new(FabricMetrics::register(&registry).expect("register"));

    let (_local, base_peer) = start_server_and_client("p").await;
    // Re-wrap with metrics. `start_server_and_client` returns
    // GrpcFabricPeer without metrics; reach into base_peer's channel
    // is cleaner if we expose it, but for now just rebuild a peer
    // pointing at the server. We need the addr, so do it directly:
    drop(base_peer);
    // Spin a fresh server bound to a known port and connect with
    // metrics attached.
    let local = local_bridge("p");
    let server = ClusterChunkServer::new(Arc::clone(&local), "p");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(ClusterChunkServiceServer::new(server))
            .serve_with_incoming(stream)
            .await
            .expect("server");
    });
    let uri: Uri = format!("http://{addr}").parse().expect("uri");
    let channel = loop {
        match Channel::builder(uri.clone()).connect().await {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    let peer = GrpcFabricPeer::new("metrics-test", channel).with_metrics(Arc::clone(&metrics));

    let env = make_envelope(0x99);
    let chunk_id = env.chunk_id;
    let tenant = OrgId(uuid::Uuid::nil());
    peer.put_fragment(chunk_id, 0, tenant, "p".into(), env)
        .await
        .expect("put");
    peer.get_fragment(chunk_id, 0).await.expect("get");

    let put_ok = metrics
        .ops_total
        .with_label_values(&[op_label::PUT, "metrics-test", outcome::OK])
        .get();
    let get_ok = metrics
        .ops_total
        .with_label_values(&[op_label::GET, "metrics-test", outcome::OK])
        .get();
    assert_eq!(put_ok, 1, "PutFragment OK should increment exactly once");
    assert_eq!(get_ok, 1, "GetFragment OK should increment exactly once");
}

#[tokio::test]
async fn get_missing_chunk_maps_to_fabric_not_found() {
    let (_local, peer) = start_server_and_client("p").await;
    let err = peer
        .get_fragment(ChunkId([0u8; 32]), 0)
        .await
        .expect_err("must not find");
    assert!(
        matches!(err, FabricPeerError::NotFound),
        "Status::not_found must map to FabricPeerError::NotFound, got {err:?}"
    );
}
