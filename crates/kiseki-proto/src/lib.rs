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

/// v1 protobuf types and gRPC services.
pub mod v1 {
    tonic::include_proto!("kiseki.v1");
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
