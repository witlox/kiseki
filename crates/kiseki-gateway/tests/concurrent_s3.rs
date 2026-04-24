//! Integration test: concurrent S3 writes through the HTTP server.
//!
//! Verifies that multiple simultaneous PUT requests complete without
//! deadlocking. This test caught a real bug where `block_in_place` +
//! `block_on` on the same tokio runtime caused all worker threads to
//! starve under concurrent load.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::s3::S3Gateway;
use kiseki_gateway::s3_server::s3_router;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        b"test-bucket",
    ))
}

fn setup_router() -> axum::Router {
    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
    });

    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, Box::new(chunks), master_key);
    let s3gw = S3Gateway::new(gw);
    s3_router(s3gw, test_tenant())
}

/// Test that a single PUT/GET roundtrip works through the HTTP layer.
#[tokio::test]
async fn single_put_get_roundtrip() {
    let app = setup_router();

    // Create bucket.
    let req = Request::builder()
        .method("PUT")
        .uri("/test-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // PUT object.
    let req = Request::builder()
        .method("PUT")
        .uri("/test-bucket/obj-1")
        .body(Body::from(vec![0xAB; 1024]))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_owned();

    // GET object.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/test-bucket/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 1024);
}

/// Test concurrent PUT requests complete without deadlocking.
///
/// This is the critical test: 32 simultaneous PUT requests must all
/// complete within a reasonable timeout. Before the fix (dedicated
/// Raft runtime), this would deadlock when `block_in_place` +
/// `block_on` starved the shared tokio worker threads.
#[tokio::test]
async fn concurrent_puts_no_deadlock() {
    let app = setup_router();

    // Create bucket first.
    let req = Request::builder()
        .method("PUT")
        .uri("/test-bucket")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Spawn 32 concurrent PUT requests.
    let mut handles = Vec::new();
    for i in 0u8..32 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let data = vec![i; 4096]; // 4KB per object
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/test-bucket/concurrent-{i}"))
                .body(Body::from(data))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "PUT concurrent-{i} failed with {}",
                resp.status()
            );
        }));
    }

    // All 32 must complete within 10 seconds (would deadlock before fix).
    let results = tokio::time::timeout(Duration::from_secs(10), async {
        for (i, handle) in handles.into_iter().enumerate() {
            handle
                .await
                .unwrap_or_else(|e| panic!("concurrent PUT {i} panicked: {e}"));
        }
    })
    .await;

    assert!(
        results.is_ok(),
        "concurrent PUTs timed out — likely deadlock"
    );
}

/// Test concurrent PUT + GET (mixed read/write) under load.
#[tokio::test]
async fn concurrent_mixed_read_write() {
    let app = setup_router();

    // Create bucket.
    let req = Request::builder()
        .method("PUT")
        .uri("/test-bucket")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // Write 10 objects first.
    let mut etags = Vec::new();
    for i in 0u8..10 {
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/test-bucket/mix-{i}"))
            .body(Body::from(vec![i; 2048]))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = resp
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .trim_matches('"')
            .to_owned();
        etags.push(etag);
    }

    // Now do 20 concurrent writes + 10 concurrent reads simultaneously.
    let mut handles = Vec::new();

    // 20 writes.
    for i in 10u8..30 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/test-bucket/mix-{i}"))
                .body(Body::from(vec![i; 2048]))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }));
    }

    // 10 reads of previously written objects.
    for etag in &etags {
        let app = app.clone();
        let etag = etag.clone();
        handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method("GET")
                .uri(format!("/test-bucket/{etag}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.len(), 2048);
        }));
    }

    let results = tokio::time::timeout(Duration::from_secs(10), async {
        for handle in handles {
            handle.await.unwrap();
        }
    })
    .await;

    assert!(
        results.is_ok(),
        "mixed read/write timed out — likely deadlock"
    );
}
