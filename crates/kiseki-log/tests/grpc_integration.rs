//! gRPC integration test: write a delta via `LogService`, read it back.

use std::sync::Arc;

use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_log::grpc::LogGrpc;
use kiseki_log::shard::ShardConfig;
use kiseki_log::store::MemShardStore;
use kiseki_proto::v1::log_service_server::LogServiceServer;
use kiseki_proto::v1::{self as proto};
use tonic::transport::Server;

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(1))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn make_timestamp() -> proto::DeltaTimestamp {
    proto::DeltaTimestamp {
        hlc: Some(proto::HybridLogicalClock {
            physical_ms: 1000,
            logical: 0,
            node_id: 1,
        }),
        wall: Some(proto::WallTime {
            millis_since_epoch: 1000,
            timezone: "UTC".into(),
        }),
        quality: 1, // NTP
    }
}

#[tokio::test]
async fn grpc_append_and_read_roundtrip() {
    // Set up in-memory store with one shard.
    let store = Arc::new(MemShardStore::new());
    store.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
    );

    let log_grpc = LogGrpc::new(Arc::clone(&store));

    // Start gRPC server on ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(LogServiceServer::new(log_grpc))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    // Give server a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Connect client.
    let mut client = proto::log_service_client::LogServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    // Append a delta.
    let append_resp = client
        .append_delta(proto::AppendDeltaRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
            tenant_id: Some(proto::OrgId {
                value: test_tenant().0.to_string(),
            }),
            operation: 1, // Create
            timestamp: Some(make_timestamp()),
            hashed_key: vec![0x42; 32],
            chunk_refs: vec![],
            payload: b"encrypted-payload".to_vec(),
            has_inline_data: true,
        })
        .await
        .unwrap();

    assert_eq!(append_resp.into_inner().sequence, 1);

    // Append a second delta.
    let append_resp2 = client
        .append_delta(proto::AppendDeltaRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
            tenant_id: Some(proto::OrgId {
                value: test_tenant().0.to_string(),
            }),
            operation: 2, // Update
            timestamp: Some(make_timestamp()),
            hashed_key: vec![0x43; 32],
            chunk_refs: vec![],
            payload: b"second-payload".to_vec(),
            has_inline_data: false,
        })
        .await
        .unwrap();

    assert_eq!(append_resp2.into_inner().sequence, 2);

    // Read back both deltas.
    let read_resp = client
        .read_deltas(proto::ReadDeltasRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
            from: 1,
            to: 2,
        })
        .await
        .unwrap();

    let deltas = read_resp.into_inner().deltas;
    assert_eq!(deltas.len(), 2);

    // Verify first delta.
    let d1 = &deltas[0];
    let h1 = d1.header.as_ref().unwrap();
    assert_eq!(h1.sequence, 1);
    assert_eq!(h1.operation, 1); // Create
    assert!(h1.has_inline_data);
    assert_eq!(
        d1.payload.as_ref().unwrap().ciphertext,
        b"encrypted-payload"
    );

    // Verify second delta.
    let d2 = &deltas[1];
    let h2 = d2.header.as_ref().unwrap();
    assert_eq!(h2.sequence, 2);
    assert_eq!(h2.operation, 2); // Update
    assert!(!h2.has_inline_data);
}

#[tokio::test]
async fn grpc_shard_health() {
    let store = Arc::new(MemShardStore::new());
    store.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
    );

    let log_grpc = LogGrpc::new(Arc::clone(&store));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(LogServiceServer::new(log_grpc))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = proto::log_service_client::LogServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    let resp = client
        .shard_health(proto::ShardHealthRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
        })
        .await
        .unwrap();

    let info = resp.into_inner().info.unwrap();
    assert_eq!(info.state, 1); // Healthy
    assert_eq!(info.tip, 0);
}

#[tokio::test]
async fn grpc_maintenance_mode() {
    let store = Arc::new(MemShardStore::new());
    store.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
    );

    let log_grpc = LogGrpc::new(Arc::clone(&store));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(LogServiceServer::new(log_grpc))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = proto::log_service_client::LogServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    // Enable maintenance.
    client
        .set_maintenance(proto::SetMaintenanceRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
            enabled: true,
        })
        .await
        .unwrap();

    // Health should show maintenance.
    let resp = client
        .shard_health(proto::ShardHealthRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
        })
        .await
        .unwrap();
    assert_eq!(resp.into_inner().info.unwrap().state, 5); // Maintenance

    // Writes should fail.
    let err = client
        .append_delta(proto::AppendDeltaRequest {
            shard_id: Some(proto::ShardId {
                value: test_shard().0.to_string(),
            }),
            tenant_id: Some(proto::OrgId {
                value: test_tenant().0.to_string(),
            }),
            operation: 1,
            timestamp: Some(make_timestamp()),
            hashed_key: vec![0x42; 32],
            chunk_refs: vec![],
            payload: vec![],
            has_inline_data: false,
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

#[tokio::test]
async fn grpc_shard_not_found() {
    let store = Arc::new(MemShardStore::new());
    // No shards created.

    let log_grpc = LogGrpc::new(store);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(LogServiceServer::new(log_grpc))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = proto::log_service_client::LogServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    let err = client
        .shard_health(proto::ShardHealthRequest {
            shard_id: Some(proto::ShardId {
                value: uuid::Uuid::from_u128(999).to_string(),
            }),
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), tonic::Code::NotFound);
}
