//! End-to-end data path tests: write via gateway → encrypt → store → read back → decrypt.

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::ops::GatewayOps;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn setup_gateway() -> InMemoryGateway {
    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
    });

    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));

    InMemoryGateway::new(compositions, Box::new(chunks), master_key)
}

#[test]
fn write_then_read_roundtrip() {
    let gw = setup_gateway();
    let plaintext = b"hello kiseki world";

    // Write via gateway — plaintext is encrypted and stored.
    let write_resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: plaintext.to_vec(),
        })
        .unwrap();

    assert_eq!(write_resp.bytes_written, plaintext.len() as u64);

    // Read back — encrypted chunks are decrypted, plaintext returned.
    let read_resp = gw
        .read(kiseki_gateway::ReadRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            composition_id: write_resp.composition_id,
            offset: 0,
            length: u64::MAX,
        })
        .unwrap();

    assert_eq!(read_resp.data, plaintext);
    assert!(read_resp.eof);
}

#[test]
fn read_with_offset_and_length() {
    let gw = setup_gateway();
    let plaintext = b"abcdefghijklmnop";

    let write_resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: plaintext.to_vec(),
        })
        .unwrap();

    // Read middle portion.
    let read_resp = gw
        .read(kiseki_gateway::ReadRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            composition_id: write_resp.composition_id,
            offset: 4,
            length: 4,
        })
        .unwrap();

    assert_eq!(read_resp.data, b"efgh");
    assert!(!read_resp.eof);
}

#[test]
fn read_past_eof_returns_empty() {
    let gw = setup_gateway();

    let write_resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: b"short".to_vec(),
        })
        .unwrap();

    let read_resp = gw
        .read(kiseki_gateway::ReadRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            composition_id: write_resp.composition_id,
            offset: 100,
            length: 10,
        })
        .unwrap();

    assert!(read_resp.data.is_empty());
    assert!(read_resp.eof);
}

#[test]
fn tenant_mismatch_rejected() {
    let gw = setup_gateway();

    let write_resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: b"secret".to_vec(),
        })
        .unwrap();

    // Try to read with a different tenant.
    let wrong_tenant = OrgId(uuid::Uuid::from_u128(999));
    let err = gw
        .read(kiseki_gateway::ReadRequest {
            tenant_id: wrong_tenant,
            namespace_id: test_namespace(),
            composition_id: write_resp.composition_id,
            offset: 0,
            length: u64::MAX,
        })
        .unwrap_err();

    assert!(
        matches!(err, kiseki_gateway::GatewayError::AuthenticationFailed(_)),
        "expected AuthenticationFailed, got: {err:?}"
    );
}

#[test]
fn bucket_isolation_list_returns_only_own_objects() {
    // Create two namespaces (buckets), write to each, verify list
    // returns only the objects belonging to each namespace.
    let ns1 = NamespaceId(uuid::Uuid::from_u128(201));
    let ns2 = NamespaceId(uuid::Uuid::from_u128(202));
    let tenant = test_tenant();

    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: ns1,
        tenant_id: tenant,
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
    });
    compositions.add_namespace(Namespace {
        id: ns2,
        tenant_id: tenant,
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, Box::new(chunks), master_key);

    // Write to ns1.
    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: tenant,
        namespace_id: ns1,
        data: b"object-in-bucket1".to_vec(),
    })
    .unwrap();

    // Write to ns2.
    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: tenant,
        namespace_id: ns2,
        data: b"object-in-bucket2".to_vec(),
    })
    .unwrap();

    // List ns1 — should see only its object.
    let list1 = gw.list(tenant, ns1).unwrap();
    assert_eq!(list1.len(), 1, "ns1 should have exactly 1 object");

    // List ns2 — should see only its object.
    let list2 = gw.list(tenant, ns2).unwrap();
    assert_eq!(list2.len(), 1, "ns2 should have exactly 1 object");

    // The composition IDs should be different.
    assert_ne!(list1[0].0, list2[0].0);
}

#[cfg(feature = "s3")]
mod s3_tests {
    use super::*;
    use kiseki_gateway::s3::{GetObjectRequest, PutObjectRequest, S3Gateway};

    #[test]
    fn s3_put_then_get() {
        let gw = setup_gateway();
        let s3 = S3Gateway::new(gw);

        let put_resp = s3
            .put_object(PutObjectRequest {
                tenant_id: test_tenant(),
                namespace_id: test_namespace(),
                body: b"s3 object data".to_vec(),
            })
            .unwrap();

        assert!(!put_resp.etag.is_empty());

        // Parse the etag back to composition ID to read.
        let comp_id =
            kiseki_common::ids::CompositionId(uuid::Uuid::parse_str(&put_resp.etag).unwrap());

        let get_resp = s3
            .get_object(GetObjectRequest {
                tenant_id: test_tenant(),
                namespace_id: test_namespace(),
                composition_id: comp_id,
            })
            .unwrap();

        assert_eq!(get_resp.body, b"s3 object data");
        assert_eq!(get_resp.content_length, 14);
    }
}

#[cfg(feature = "nfs")]
mod nfs_tests {
    use super::*;
    use kiseki_gateway::nfs::{NfsGateway, NfsReadRequest, NfsWriteRequest};
    use kiseki_gateway::nfs_ops::{FileType, NfsContext};
    use std::sync::Arc;

    #[test]
    fn nfs_write_then_read() {
        let gw = setup_gateway();
        let nfs = NfsGateway::new(gw);

        let write_resp = nfs
            .write(NfsWriteRequest {
                tenant_id: test_tenant(),
                namespace_id: test_namespace(),
                data: b"nfs file content".to_vec(),
            })
            .unwrap();

        assert_eq!(write_resp.count, 16);

        let read_resp = nfs
            .read(NfsReadRequest {
                tenant_id: test_tenant(),
                namespace_id: test_namespace(),
                composition_id: write_resp.composition_id,
                offset: 0,
                count: 1024,
            })
            .unwrap();

        assert_eq!(read_resp.data, b"nfs file content");
        assert!(read_resp.eof);
    }

    fn setup_nfs_ctx() -> NfsContext<Arc<InMemoryGateway>> {
        let gw = Arc::new(setup_gateway());
        let nfs_gw = NfsGateway::new(Arc::clone(&gw));
        NfsContext::new(nfs_gw, test_tenant(), test_namespace())
    }

    #[test]
    fn nfs3_getattr_root_is_directory() {
        let ctx = setup_nfs_ctx();
        let root_fh = ctx.handles.root_handle(test_namespace(), test_tenant());
        let attrs = ctx.getattr(&root_fh).unwrap();
        assert_eq!(attrs.file_type, FileType::Directory);
        assert_eq!(attrs.mode, 0o755);
        assert_eq!(attrs.nlink, 2);
    }

    #[test]
    fn nfs3_write_named_then_lookup() {
        let ctx = setup_nfs_ctx();
        let (fh, resp) = ctx.write_named("test.dat", b"hello".to_vec()).unwrap();
        assert_eq!(resp.count, 5);

        let (lookup_fh, attrs) = ctx.lookup_by_name("test.dat").unwrap();
        assert_eq!(lookup_fh, fh);
        assert_eq!(attrs.file_type, FileType::Regular);
        assert_eq!(attrs.size, 5);
    }

    #[test]
    fn nfs3_readdir_includes_created_files() {
        let ctx = setup_nfs_ctx();
        ctx.write_named("a.txt", b"aaa".to_vec()).unwrap();
        ctx.write_named("b.txt", b"bbb".to_vec()).unwrap();

        let entries = ctx.readdir();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."));
        assert!(names.contains(&".."));
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn nfs3_remove_file() {
        let ctx = setup_nfs_ctx();
        ctx.write_named("gone.txt", b"bye".to_vec()).unwrap();
        assert!(ctx.lookup_by_name("gone.txt").is_some());

        ctx.remove_file("gone.txt").unwrap();
        assert!(ctx.lookup_by_name("gone.txt").is_none());
    }

    #[test]
    fn nfs3_rename_file() {
        let ctx = setup_nfs_ctx();
        ctx.write_named("old.txt", b"data".to_vec()).unwrap();

        ctx.rename_file("old.txt", "new.txt").unwrap();
        assert!(ctx.lookup_by_name("old.txt").is_none());
        assert!(ctx.lookup_by_name("new.txt").is_some());
    }

    #[test]
    fn nfs3_mkdir_and_rmdir() {
        let ctx = setup_nfs_ctx();
        let (_fh, attrs) = ctx.mkdir("subdir").unwrap();
        assert_eq!(attrs.file_type, FileType::Directory);
        assert_eq!(attrs.mode, 0o755);

        // Should appear in readdir.
        let entries = ctx.readdir();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"subdir"));

        // Remove it.
        ctx.rmdir("subdir").unwrap();
        let entries = ctx.readdir();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"subdir"));
    }

    #[test]
    fn nfs3_setattr_returns_attrs() {
        let ctx = setup_nfs_ctx();
        let root_fh = ctx.handles.root_handle(test_namespace(), test_tenant());
        let attrs = ctx.setattr(&root_fh, Some(0o700)).unwrap();
        // setattr is advisory — returns current attrs.
        assert_eq!(attrs.file_type, FileType::Directory);
    }

    #[test]
    fn nfs3_access_grants_all() {
        let ctx = setup_nfs_ctx();
        let root_fh = ctx.handles.root_handle(test_namespace(), test_tenant());
        let bits = ctx.access(&root_fh).unwrap();
        assert_eq!(bits, 0x3F); // all access bits
    }

    #[test]
    fn nfs3_symlink_and_readlink() {
        let ctx = setup_nfs_ctx();
        let (fh, attrs) = ctx.symlink("link.txt", "/target/path").unwrap();
        assert_eq!(attrs.size, 12); // "/target/path".len()

        let target = ctx.readlink(&fh).unwrap();
        assert_eq!(target, "/target/path");
    }

    #[test]
    fn nfs3_link_creates_hard_link() {
        let ctx = setup_nfs_ctx();
        let (fh, _) = ctx
            .write_named("original.txt", b"content".to_vec())
            .unwrap();

        ctx.link(&fh, "hardlink.txt").unwrap();
        let (link_fh, _) = ctx.lookup_by_name("hardlink.txt").unwrap();
        assert_eq!(link_fh, fh); // same handle
    }

    #[test]
    fn nfs3_commit_is_noop() {
        let ctx = setup_nfs_ctx();
        ctx.commit().unwrap();
    }

    #[test]
    fn nfs3_stale_handle_returns_error() {
        let ctx = setup_nfs_ctx();
        let bogus_fh = [0xDEu8; 32];
        assert!(ctx.getattr(&bogus_fh).is_err());
        assert!(ctx.read(&bogus_fh, 0, 100).is_err());
        assert!(ctx.access(&bogus_fh).is_err());
    }
}
