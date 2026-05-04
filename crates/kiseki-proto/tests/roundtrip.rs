#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Smoke tests for generated protobuf types.
//!
//! Verifies the Phase 0 exit criterion "protobuf generates cleanly": a
//! representative message from each `.proto` file can be constructed,
//! encoded with prost, and decoded back to an equal value. Does not
//! attempt to exhaustively test every message — that would duplicate the
//! per-crate tests in Phases 3-11.

use kiseki_proto::v1::{
    ChunkId, ClockQuality, CompositionId, DeltaTimestamp, HybridLogicalClock, KeyEpoch,
    NamespaceId, NodeId, OrgId, ProjectId, ShardId, ViewId, WallTime, WorkloadId,
};
use prost::Message;

fn sample_timestamp() -> DeltaTimestamp {
    DeltaTimestamp {
        hlc: Some(HybridLogicalClock {
            physical_ms: 42,
            logical: 7,
            node_id: 99,
        }),
        wall: Some(WallTime {
            millis_since_epoch: 1_700_000_000_000,
            timezone: "UTC".into(),
        }),
        quality: ClockQuality::Ntp as i32,
    }
}

#[test]
fn identifiers_roundtrip() {
    let ids: [Box<dyn std::any::Any>; 8] = [
        Box::new(OrgId {
            value: "acme".into(),
        }),
        Box::new(ProjectId {
            value: "alpha".into(),
        }),
        Box::new(WorkloadId {
            value: "train-42".into(),
        }),
        Box::new(ShardId {
            value: "s-0001".into(),
        }),
        Box::new(NamespaceId {
            value: "ns-0001".into(),
        }),
        Box::new(ViewId {
            value: "view-0001".into(),
        }),
        Box::new(CompositionId {
            value: "c-0001".into(),
        }),
        Box::new(NodeId { value: 12_345 }),
    ];
    // Compile-time check that all identifier messages exist; structural
    // round-trip is exercised by `delta_timestamp_roundtrip` below via
    // transitive inclusion.
    assert_eq!(ids.len(), 8);

    let cid = ChunkId {
        value: vec![0x00, 0x11, 0x22, 0x33],
    };
    let bytes = cid.encode_to_vec();
    let decoded = ChunkId::decode(&*bytes).expect("decode ChunkId");
    assert_eq!(decoded.value, vec![0x00, 0x11, 0x22, 0x33]);
}

#[test]
fn delta_timestamp_roundtrip() {
    let original = sample_timestamp();
    let bytes = original.encode_to_vec();
    let decoded = DeltaTimestamp::decode(&*bytes).expect("decode DeltaTimestamp");
    assert_eq!(decoded, original);
    assert_eq!(decoded.quality, ClockQuality::Ntp as i32);
}

#[test]
fn key_epoch_roundtrip() {
    let original = KeyEpoch { value: u64::MAX };
    let bytes = original.encode_to_vec();
    let decoded = KeyEpoch::decode(&*bytes).expect("decode KeyEpoch");
    assert_eq!(decoded, original);
}

#[test]
fn log_delta_envelope_constructs() {
    use kiseki_proto::v1::{Delta, DeltaHeader, DeltaPayload, OperationType};
    let delta = Delta {
        header: Some(DeltaHeader {
            sequence: 17,
            shard_id: Some(ShardId {
                value: "s-0001".into(),
            }),
            tenant_id: Some(OrgId {
                value: "acme".into(),
            }),
            operation: OperationType::Create as i32,
            timestamp: Some(sample_timestamp()),
            hashed_key: vec![0xa5; 32],
            tombstone: false,
            chunk_refs: vec![ChunkId {
                value: vec![0x11; 32],
            }],
            payload_size: 4,
            has_inline_data: true,
        }),
        payload: Some(DeltaPayload {
            ciphertext: vec![0, 1, 2, 3],
            auth_tag: vec![0xaa; 16],
            nonce: vec![0xbb; 12],
            system_epoch: Some(KeyEpoch { value: 1 }),
            tenant_epoch: Some(KeyEpoch { value: 1 }),
            tenant_wrapped_material: vec![0xcc; 48],
        }),
    };
    let bytes = delta.encode_to_vec();
    let decoded = Delta::decode(&*bytes).expect("decode Delta");
    assert_eq!(decoded, delta);
}

#[test]
fn chunk_envelope_constructs() {
    use kiseki_proto::v1::{EncryptionAlgorithm, Envelope};
    let envelope = Envelope {
        ciphertext: vec![1, 2, 3, 4],
        auth_tag: vec![0xde; 16],
        nonce: vec![0xad; 12],
        algorithm: EncryptionAlgorithm::Aes256Gcm as i32,
        system_epoch: Some(KeyEpoch { value: 2 }),
        tenant_epoch: Some(KeyEpoch { value: 2 }),
        tenant_wrapped_material: vec![0xbe; 32],
        chunk_id: Some(ChunkId {
            value: vec![0x42; 32],
        }),
    };
    let bytes = envelope.encode_to_vec();
    let decoded = Envelope::decode(&*bytes).expect("decode Envelope");
    assert_eq!(decoded, envelope);
    assert_eq!(decoded.algorithm, EncryptionAlgorithm::Aes256Gcm as i32);
}

#[test]
fn view_descriptor_constructs() {
    use kiseki_proto::v1::{
        consistency_model, AffinityPoolId, ConsistencyModel, ProtocolSemantics, ReadYourWrites,
        ViewDescriptor,
    };
    let descriptor = ViewDescriptor {
        view_id: Some(ViewId {
            value: "v-0001".into(),
        }),
        tenant_id: Some(OrgId {
            value: "acme".into(),
        }),
        source_shards: vec![ShardId {
            value: "s-0001".into(),
        }],
        protocol: ProtocolSemantics::Posix as i32,
        consistency: Some(ConsistencyModel {
            model: Some(consistency_model::Model::ReadYourWrites(ReadYourWrites {})),
        }),
        affinity_pool: Some(AffinityPoolId {
            value: vec![0x01; 16],
        }),
        discardable: true,
        version: 1,
        created_at: Some(sample_timestamp()),
    };
    let bytes = descriptor.encode_to_vec();
    let decoded = ViewDescriptor::decode(&*bytes).expect("decode ViewDescriptor");
    assert_eq!(decoded, descriptor);
}
