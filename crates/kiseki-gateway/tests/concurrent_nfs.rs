//! Integration tests: concurrent NFS and pNFS operations.
//!
//! Tests NFS gateway under concurrent load (multi-threaded, matching
//! the actual NFS server's thread-per-connection model). Also tests
//! pNFS layout delegation and segment resolution.

use std::sync::Arc;
use std::thread;

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::nfs::{NfsGateway, NfsReadRequest, NfsWriteRequest};

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn setup_nfs_gateway() -> Arc<NfsGateway<InMemoryGateway>> {
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
    Arc::new(NfsGateway::new(gw))
}

/// NFS write/read roundtrip on a single thread.
#[tokio::test(flavor = "multi_thread")]
async fn nfs_write_read_roundtrip() {
    let nfs = setup_nfs_gateway();
    let data = vec![0xAB; 4096];

    let write_resp = nfs
        .write(NfsWriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: data.clone(),
        })
        .await
        .unwrap();

    assert_eq!(write_resp.count, 4096);

    let read_resp = nfs
        .read(NfsReadRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            composition_id: write_resp.composition_id,
            offset: 0,
            count: 4096,
        })
        .await
        .unwrap();

    assert_eq!(read_resp.data, data);
    assert!(read_resp.eof);
}

/// Concurrent NFS writes from multiple threads (mimics thread-per-connection model).
///
/// The NFS server spawns an OS thread per connection. This test verifies
/// that 16 concurrent writer threads don't deadlock or corrupt state.
#[test]
fn concurrent_nfs_writes_no_deadlock() {
    let nfs = setup_nfs_gateway();
    let mut handles = Vec::new();

    for i in 0u8..16 {
        let nfs = Arc::clone(&nfs);
        handles.push(thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            for j in 0..10 {
                let data = vec![i.wrapping_add(j); 1024];
                let resp = rt
                    .block_on(nfs.write(NfsWriteRequest {
                        tenant_id: test_tenant(),
                        namespace_id: test_namespace(),
                        data,
                    }))
                    .unwrap();
                assert!(resp.count > 0, "write returned 0 bytes");
            }
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("NFS writer thread {i} panicked"));
    }
}

/// Concurrent NFS read + write (mixed workload).
#[test]
fn concurrent_nfs_mixed_read_write() {
    let nfs = setup_nfs_gateway();

    // Pre-write objects for readers.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut comp_ids = Vec::new();
    for i in 0u8..10 {
        let resp = rt
            .block_on(nfs.write(NfsWriteRequest {
                tenant_id: test_tenant(),
                namespace_id: test_namespace(),
                data: vec![i; 2048],
            }))
            .unwrap();
        comp_ids.push(resp.composition_id);
    }
    drop(rt);

    let comp_ids = Arc::new(comp_ids);
    let mut handles = Vec::new();

    // 8 writer threads.
    for i in 0u8..8 {
        let nfs = Arc::clone(&nfs);
        handles.push(thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            for j in 0..5 {
                let data = vec![i.wrapping_add(j); 4096];
                rt.block_on(nfs.write(NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data,
                }))
                .unwrap();
            }
        }));
    }

    // 8 reader threads.
    for i in 0..8 {
        let nfs = Arc::clone(&nfs);
        let comp_ids = Arc::clone(&comp_ids);
        handles.push(thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let comp_id = comp_ids[i % comp_ids.len()];
            for _ in 0..5 {
                let resp = rt
                    .block_on(nfs.read(NfsReadRequest {
                        tenant_id: test_tenant(),
                        namespace_id: test_namespace(),
                        composition_id: comp_id,
                        offset: 0,
                        count: 2048,
                    }))
                    .unwrap();
                assert_eq!(resp.data.len(), 2048);
            }
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("NFS mixed thread {i} panicked"));
    }
}

/// pNFS layout manager: concurrent layout requests.
#[test]
fn pnfs_layout_delegation() {
    use kiseki_gateway::pnfs::{IoMode, LayoutManager};

    let storage_nodes = vec![
        "10.0.0.10:9100".to_owned(),
        "10.0.0.11:9100".to_owned(),
        "10.0.0.12:9100".to_owned(),
    ];
    let mut mgr = LayoutManager::new(storage_nodes);

    // Request a file layout.
    let layout = mgr.layout_get(42, 0, 4 * 1024 * 1024, IoMode::ReadWrite);
    assert!(!layout.segments.is_empty(), "layout should have segments");
    assert_eq!(layout.file_id, 42);

    // Verify round-robin striping across nodes.
    let addrs: Vec<&str> = layout
        .segments
        .iter()
        .map(|s| s.device_addr.as_str())
        .collect();
    // With 3 nodes and 4MB, we should see striping.
    assert!(
        addrs.len() >= 2,
        "expected multiple segments for 4MB file, got {}",
        addrs.len()
    );

    // Verify segments cover the full range.
    let total_covered: u64 = layout.segments.iter().map(|s| s.length).sum();
    assert_eq!(total_covered, 4 * 1024 * 1024);

    // Return layout.
    assert!(mgr.layout_return(42));
    // Second return should indicate no active layout.
    assert!(!mgr.layout_return(42));
}

/// pNFS: concurrent layout get/return from multiple threads.
#[test]
fn pnfs_concurrent_layout_requests() {
    use kiseki_gateway::pnfs::IoMode;
    use std::sync::Mutex;

    let storage_nodes = vec![
        "10.0.0.10:9100".to_owned(),
        "10.0.0.11:9100".to_owned(),
        "10.0.0.12:9100".to_owned(),
        "10.0.0.20:9100".to_owned(),
        "10.0.0.21:9100".to_owned(),
    ];
    let mgr = Arc::new(Mutex::new(kiseki_gateway::pnfs::LayoutManager::new(
        storage_nodes,
    )));

    let mut handles = Vec::new();

    for t in 0u64..8 {
        let mgr = Arc::clone(&mgr);
        handles.push(thread::spawn(move || {
            for i in 0u64..10 {
                let file_id = t * 100 + i;
                let layout = mgr
                    .lock()
                    .unwrap()
                    .layout_get(file_id, 0, 1024 * 1024, IoMode::Read);
                assert!(!layout.segments.is_empty());
                assert_eq!(layout.file_id, file_id);

                // Return immediately.
                assert!(mgr.lock().unwrap().layout_return(file_id));
            }
        }));
    }

    for handle in handles {
        handle.join().expect("pNFS layout thread panicked");
    }
}
