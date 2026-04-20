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

    InMemoryGateway::new(compositions, chunks, master_key)
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
}
