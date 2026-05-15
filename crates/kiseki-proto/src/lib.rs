//! Generated protobuf/gRPC types for Kiseki.
//!
//! Source of truth: `specs/architecture/proto/kiseki/v1/*.proto`. Rust
//! code is generated at build time by `build.rs` via `tonic-build` +
//! `prost-build`. Do not hand-edit anything under `v1` — edit the
//! `.proto` in `specs/architecture/proto/` and let the build emit new
//! output.
//!
//! The Go side of the boundary generates into `control/proto/kiseki/v1/`
//! from the same canonical `.proto` files.

#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]
#![allow(missing_docs, rust_2018_idioms)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

/// Transport-agnostic contract types for the native gateway data
/// service (ADR-042 §1.7 / §1.8). Carried verbatim by every binding
/// (gRPC, TCP-framed-postcard, ibverbs, libfabric).
pub mod native_contract;

/// v1 protobuf types and gRPC services.
pub mod v1 {
    tonic::include_proto!("kiseki.v1");

    /// Native gateway data service (ADR-042). Sub-namespace
    /// `kiseki.v1.native` so its message names don't collide with
    /// the rest of `kiseki.v1` (e.g., `Empty`, `AbortMultipartRequest`).
    /// Shared ID types (`OrgId`, `ShardId`, `NamespaceId`,
    /// `CompositionId`, `ChunkId`) come from the parent `kiseki.v1`.
    pub mod native {
        tonic::include_proto!("kiseki.v1.native");
    }
}

#[cfg(test)]
mod tests {
    use super::v1;
    use prost::Message;

    #[test]
    fn org_id_roundtrip() {
        let original = v1::OrgId {
            value: "org-12345".into(),
        };

        let mut buf = Vec::new();
        original.encode(&mut buf).expect("encode failed");
        assert!(!buf.is_empty());

        let decoded = v1::OrgId::decode(&buf[..]).expect("decode failed");
        assert_eq!(original, decoded);
    }

    #[test]
    fn chunk_id_roundtrip() {
        let original = v1::ChunkId {
            value: vec![0xaa; 32],
        };

        let mut buf = Vec::new();
        original.encode(&mut buf).expect("encode failed");
        let decoded = v1::ChunkId::decode(&buf[..]).expect("decode failed");
        assert_eq!(original, decoded);
    }

    #[test]
    fn hlc_roundtrip() {
        let original = v1::HybridLogicalClock {
            physical_ms: 1_700_000_000_000,
            logical: 42,
            node_id: 7,
        };

        let mut buf = Vec::new();
        original.encode(&mut buf).expect("encode failed");
        let decoded = v1::HybridLogicalClock::decode(&buf[..]).expect("decode failed");
        assert_eq!(original, decoded);
    }

    #[test]
    fn empty_message_deserializes_without_panic() {
        // An empty byte slice should decode to the default message.
        let decoded = v1::OrgId::decode(&[][..]).expect("empty decode should succeed");
        assert_eq!(decoded.value, "");

        let decoded = v1::NodeId::decode(&[][..]).expect("empty decode should succeed");
        assert_eq!(decoded.value, 0);

        let decoded = v1::Quota::decode(&[][..]).expect("empty decode should succeed");
        assert_eq!(decoded.capacity_bytes, 0);
        assert_eq!(decoded.iops, 0);
    }

    /// ADR-042 §2.2 TCP-framed binding requires postcard codec on
    /// the same prost-generated request/response types the gRPC
    /// binding uses. Pin a representative round-trip here so a
    /// build.rs regression that drops the serde derive fails the
    /// test loudly.
    #[test]
    fn prost_native_types_postcard_roundtrip() {
        use super::v1;
        use super::v1::native as np;
        let req = np::PutObjectRequest {
            control: Some(np::ControlFields {
                tenant_id: Some(v1::OrgId {
                    value: "tenant-postcard".into(),
                }),
                idempotency_key: vec![1, 2, 3],
                workflow_ref: String::new(),
                cache_hint: None,
                conditional: None,
            }),
            namespace_id: Some(v1::NamespaceId {
                value: "ns-postcard".into(),
            }),
            name: "obj-key".into(),
            data: vec![0xAB; 64],
        };
        let bytes = postcard::to_allocvec(&req).expect("postcard encode on serde-derived type");
        let decoded: np::PutObjectRequest =
            postcard::from_bytes(&bytes).expect("postcard decode on serde-derived type");
        assert_eq!(req, decoded);
    }

    /// RED-first: ControlFields MUST carry `forwarded_from_node` so the
    /// leader's audit record for a proxied write can attribute the
    /// originating tenant AND the forwarding node (per the
    /// `2026-05-15-leader-forwarding-posture.md` finding §M2). proto3
    /// optional field: encoded as a single u64; absent => `None`,
    /// present => `Some(node_id)`. Round-trips through prost encode/decode
    /// AND through postcard (TCP-framed binding, ADR-042 §2.2).
    #[test]
    fn control_fields_forwarded_from_node_roundtrips() {
        use super::v1;
        use super::v1::native as np;

        // Case A: unset — proxy hop didn't happen. `forwarded_from_node`
        // is None on the wire.
        let cf_no_forward = np::ControlFields {
            tenant_id: Some(v1::OrgId {
                value: "tenant-roundtrip".into(),
            }),
            idempotency_key: vec![1, 2, 3],
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
            forwarded_from_node: None,
        };
        let mut buf = Vec::new();
        cf_no_forward.encode(&mut buf).expect("encode no_forward");
        let decoded = np::ControlFields::decode(&buf[..]).expect("decode no_forward");
        assert_eq!(decoded.forwarded_from_node, None);

        // Case B: proxy hop happened — `forwarded_from_node = Some(7)`.
        // The leader's audit record consumes this value.
        let cf_forwarded = np::ControlFields {
            tenant_id: Some(v1::OrgId {
                value: "tenant-roundtrip".into(),
            }),
            idempotency_key: vec![1, 2, 3],
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
            forwarded_from_node: Some(7),
        };
        let mut buf2 = Vec::new();
        cf_forwarded.encode(&mut buf2).expect("encode forwarded");
        let decoded2 = np::ControlFields::decode(&buf2[..]).expect("decode forwarded");
        assert_eq!(decoded2.forwarded_from_node, Some(7));

        // Postcard (TCP-framed binding) preserves the same shape.
        let pc_bytes = postcard::to_allocvec(&cf_forwarded).expect("postcard encode");
        let pc_decoded: np::ControlFields =
            postcard::from_bytes(&pc_bytes).expect("postcard decode");
        assert_eq!(pc_decoded.forwarded_from_node, Some(7));
    }

    #[test]
    fn native_gateway_data_service_module_compiles() {
        // Smoke test: the kiseki.v1.native sub-namespace generated
        // the ServerImpl + ClientImpl traits. Constructing a request
        // proves the messages exist with the expected field names.
        use super::v1::native;
        let req = native::GetTopologyRequest {
            known_topology_version: 0,
            tenant_id: Some(v1::OrgId {
                value: "org-perf".into(),
            }),
        };
        assert_eq!(req.known_topology_version, 0);
        let _topo = native::TopologyInfo {
            topology_version: 1,
            nodes: Vec::new(),
            shards: Vec::new(),
        };
        let _grant = native::LeaseGrant {
            lease_id: vec![0xab; 16],
            fencing_token: 42,
            ttl_ms: 30_000,
            expires_at_millis_since_epoch: 1_700_000_000_000,
        };
    }

    #[test]
    fn delta_timestamp_with_nested_messages() {
        let original = v1::DeltaTimestamp {
            hlc: Some(v1::HybridLogicalClock {
                physical_ms: 1000,
                logical: 1,
                node_id: 3,
            }),
            wall: Some(v1::WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            }),
            quality: v1::ClockQuality::Ntp as i32,
        };

        let mut buf = Vec::new();
        original.encode(&mut buf).expect("encode failed");
        let decoded = v1::DeltaTimestamp::decode(&buf[..]).expect("decode failed");
        assert_eq!(original, decoded);
        assert_eq!(decoded.hlc.unwrap().physical_ms, 1000);
    }
}
