#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests: concurrent FUSE native client operations.
//!
//! Tests the `KisekiFuse` filesystem under concurrent access patterns
//! matching `fuser`'s thread pool model. The `FuseDaemon` wraps
//! `KisekiFuse` in a `Mutex`, so concurrent FUSE ops serialize —
//! these tests verify no deadlock or corruption under that model.

use std::sync::{Arc, Mutex};
use std::thread;

use kiseki_chunk::store::ChunkStore;
use kiseki_client::fuse_fs::KisekiFuse;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn setup_fuse() -> KisekiFuse<InMemoryGateway> {
    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });

    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
    KisekiFuse::new(gw, test_tenant(), test_namespace())
}

/// Single-threaded create/read/unlink roundtrip.
#[test]
fn fuse_create_read_unlink() {
    let mut fs = setup_fuse();
    let data = b"hello fuse".to_vec();

    let ino = fs.create("test.txt", data.clone()).unwrap();
    let read_data = fs.read(ino, 0, 1024).unwrap();
    assert_eq!(read_data, data);

    fs.unlink("test.txt").unwrap();
    assert!(fs.read(ino, 0, 1024).is_err());
}

/// Single-threaded mkdir + create + readdir.
#[test]
fn fuse_mkdir_create_readdir() {
    let mut fs = setup_fuse();

    let dir_ino = fs.mkdir("subdir").unwrap();
    let _file_ino = fs
        .create_in(dir_ino, "file.txt", b"content".to_vec())
        .unwrap();

    let entries = fs.readdir();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"."), "missing . entry");
    assert!(names.contains(&".."), "missing .. entry");
    assert!(names.contains(&"subdir"), "missing subdir entry");
}

/// Concurrent file creation from multiple threads (Mutex-serialized,
/// matching `FuseDaemon`'s architecture).
#[test]
fn concurrent_fuse_creates() {
    let fs = Arc::new(Mutex::new(setup_fuse()));
    let mut handles = Vec::new();

    for t in 0u8..8 {
        let fs = Arc::clone(&fs);
        handles.push(thread::spawn(move || {
            for i in 0..10 {
                let name = format!("t{t}-f{i}.dat");
                let data = vec![t.wrapping_add(i); 512];
                let ino = fs.lock().unwrap().create(&name, data.clone()).unwrap();
                let read_data = fs.lock().unwrap().read(ino, 0, 512).unwrap();
                assert_eq!(read_data, data, "data mismatch for {name}");
            }
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("FUSE create thread {i} panicked"));
    }

    // Verify total file count.
    let entries = fs.lock().unwrap().readdir();
    // 80 files + . + ..
    let file_count = entries
        .iter()
        .filter(|e| e.name != "." && e.name != "..")
        .count();
    assert_eq!(file_count, 80, "expected 80 files, got {file_count}");
}

/// Concurrent read + write (mixed workload) on shared filesystem.
#[test]
fn concurrent_fuse_mixed_read_write() {
    let fs = Arc::new(Mutex::new(setup_fuse()));

    // Pre-create files for readers.
    let mut inos = Vec::new();
    for i in 0u8..10 {
        let ino = fs
            .lock()
            .unwrap()
            .create(&format!("pre-{i}.dat"), vec![i; 1024])
            .unwrap();
        inos.push(ino);
    }

    let inos = Arc::new(inos);
    let mut handles = Vec::new();

    // 4 writer threads.
    for t in 0u8..4 {
        let fs = Arc::clone(&fs);
        handles.push(thread::spawn(move || {
            for i in 0..10 {
                let name = format!("w{t}-{i}.dat");
                fs.lock()
                    .unwrap()
                    .create(&name, vec![t.wrapping_add(i); 2048])
                    .unwrap();
            }
        }));
    }

    // 4 reader threads.
    for t in 0..4 {
        let fs = Arc::clone(&fs);
        let inos = Arc::clone(&inos);
        handles.push(thread::spawn(move || {
            for _ in 0..10 {
                let ino = inos[t % inos.len()];
                let data = fs.lock().unwrap().read(ino, 0, 1024).unwrap();
                assert_eq!(data.len(), 1024);
            }
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("FUSE mixed thread {i} panicked"));
    }
}

/// Concurrent metadata operations: mkdir + rename + unlink.
#[test]
fn concurrent_fuse_metadata_ops() {
    let fs = Arc::new(Mutex::new(setup_fuse()));
    let mut handles = Vec::new();

    // 4 threads creating directories + files, then renaming.
    for t in 0u8..4 {
        let fs = Arc::clone(&fs);
        handles.push(thread::spawn(move || {
            for i in 0..5 {
                let dir_name = format!("dir-{t}-{i}");
                let file_name = format!("file-{t}-{i}.dat");
                let renamed = format!("renamed-{t}-{i}.dat");

                fs.lock().unwrap().mkdir(&dir_name).unwrap();
                fs.lock().unwrap().create(&file_name, vec![t; 256]).unwrap();
                fs.lock().unwrap().rename(&file_name, &renamed).unwrap();
            }
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("FUSE metadata thread {i} panicked"));
    }

    // Verify: 20 dirs + 20 renamed files + . + ..
    let entries = fs.lock().unwrap().readdir();
    let non_dot: Vec<&str> = entries
        .iter()
        .filter(|e| e.name != "." && e.name != "..")
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        non_dot.len(),
        40,
        "expected 40 entries, got {}",
        non_dot.len()
    );

    // All renamed files should exist, original names should not.
    for t in 0u8..4 {
        for i in 0..5 {
            let renamed = format!("renamed-{t}-{i}.dat");
            let original = format!("file-{t}-{i}.dat");
            assert!(
                non_dot.contains(&renamed.as_str()),
                "missing renamed file {renamed}"
            );
            assert!(
                !non_dot.contains(&original.as_str()),
                "original {original} should not exist"
            );
        }
    }
}
