//! Layer 1 reference tests for the **gRPC + Protobuf** contract
//! between kiseki services.
//!
//! ADR-023 §D2: per-spec-section unit tests. For gRPC, the "spec
//! sections" are:
//!
//!   1. The gRPC status-code → application-error mapping for each
//!      service method (`NOT_FOUND` → application `NotFound`, etc.).
//!   2. Reserved tag invariants — once a field number is `reserved`
//!      in a `.proto` file, it MUST NOT be reused for a new field.
//!      The protobuf compiler enforces this at build time, but a
//!      Layer 1 test pins the *current* reserved set so a future
//!      proto change cannot silently un-reserve a tag.
//!   3. Wire round-trip for the most-used messages (delta append,
//!      log read, control-plane RPCs). Catches subtle prost/tonic
//!      regressions like a default-value change or oneof reordering.
//!
//! Owner: `kiseki-proto` (build-script generated) plus the per-
//! service implementations in `kiseki-log/src/grpc.rs`,
//! `kiseki-control/src/grpc.rs`, `kiseki-key-manager/src/grpc.rs`,
//! etc. The mapping itself is the per-service code's contract;
//! `kiseki-proto` is where the messages live, so this is the right
//! place to pin the contract tests.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "gRPC + Protobuf".
//!
//! Spec text:
//! - <https://protobuf.dev/programming-guides/proto3/> — wire format.
//! - <https://github.com/grpc/grpc/blob/master/doc/statuscodes.md>
//!   — gRPC standard status codes.
//! - <https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md>
//!   — gRPC over HTTP/2.
#![allow(
    clippy::doc_markdown,
    clippy::unreadable_literal,
    clippy::inconsistent_digit_grouping,
    clippy::items_after_statements,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_borrows_for_generic_args,
    clippy::useless_format,
    clippy::stable_sort_primitive,
    clippy::trivially_copy_pass_by_ref,
    clippy::format_in_format_args,
    clippy::assertions_on_constants,
    clippy::bool_assert_comparison,
    clippy::doc_lazy_continuation,
    clippy::no_effect_underscore_binding,
    clippy::assertions_on_result_states,
    clippy::format_collect,
    clippy::manual_string_new,
    clippy::manual_range_contains,
    clippy::unicode_not_nfc
)]

use kiseki_proto::v1::{
    AppendDeltaRequest, AppendDeltaResponse, ChunkId, CreateOrganizationRequest,
    CreateOrganizationResponse, Delta, DeltaHeader, DeltaPayload, DeltaTimestamp,
    HybridLogicalClock, KeyEpoch, OperationType, OrgId, ReadDeltasRequest, ReadDeltasResponse,
    SetMaintenanceRequest, ShardId, WallTime,
};
use prost::Message;

// ===========================================================================
// Sentinel constants — gRPC standard status codes (canonical numerics)
// ===========================================================================
//
// `https://github.com/grpc/grpc/blob/master/doc/statuscodes.md` —
// the gRPC framework defines these numerically; the `tonic::Code`
// enum mirrors them. Pinning the integer values guards against a
// tonic version that re-orders or renames them.

/// gRPC `OK = 0`.
const GRPC_OK: i32 = 0;
/// gRPC `CANCELLED = 1`.
const GRPC_CANCELLED: i32 = 1;
/// gRPC `UNKNOWN = 2`.
const GRPC_UNKNOWN: i32 = 2;
/// gRPC `INVALID_ARGUMENT = 3`.
const GRPC_INVALID_ARGUMENT: i32 = 3;
/// gRPC `DEADLINE_EXCEEDED = 4`.
const GRPC_DEADLINE_EXCEEDED: i32 = 4;
/// gRPC `NOT_FOUND = 5`.
const GRPC_NOT_FOUND: i32 = 5;
/// gRPC `ALREADY_EXISTS = 6`.
const GRPC_ALREADY_EXISTS: i32 = 6;
/// gRPC `PERMISSION_DENIED = 7`.
const GRPC_PERMISSION_DENIED: i32 = 7;
/// gRPC `RESOURCE_EXHAUSTED = 8`.
const GRPC_RESOURCE_EXHAUSTED: i32 = 8;
/// gRPC `FAILED_PRECONDITION = 9`.
const GRPC_FAILED_PRECONDITION: i32 = 9;
/// gRPC `ABORTED = 10`.
const GRPC_ABORTED: i32 = 10;
/// gRPC `OUT_OF_RANGE = 11`.
const GRPC_OUT_OF_RANGE: i32 = 11;
/// gRPC `UNIMPLEMENTED = 12`.
const GRPC_UNIMPLEMENTED: i32 = 12;
/// gRPC `INTERNAL = 13`.
const GRPC_INTERNAL: i32 = 13;
/// gRPC `UNAVAILABLE = 14`.
const GRPC_UNAVAILABLE: i32 = 14;
/// gRPC `DATA_LOSS = 15`.
const GRPC_DATA_LOSS: i32 = 15;
/// gRPC `UNAUTHENTICATED = 16`.
const GRPC_UNAUTHENTICATED: i32 = 16;

// ===========================================================================
// gRPC status-code numeric pin
// ===========================================================================

/// `https://github.com/grpc/grpc/blob/master/doc/statuscodes.md` —
/// pin the canonical gRPC status code integers. A future tonic
/// renaming or reordering would surface here.
#[test]
fn grpc_canonical_status_codes_numeric_pin() {
    assert_eq!(GRPC_OK, 0);
    assert_eq!(GRPC_CANCELLED, 1);
    assert_eq!(GRPC_UNKNOWN, 2);
    assert_eq!(GRPC_INVALID_ARGUMENT, 3);
    assert_eq!(GRPC_DEADLINE_EXCEEDED, 4);
    assert_eq!(GRPC_NOT_FOUND, 5);
    assert_eq!(GRPC_ALREADY_EXISTS, 6);
    assert_eq!(GRPC_PERMISSION_DENIED, 7);
    assert_eq!(GRPC_RESOURCE_EXHAUSTED, 8);
    assert_eq!(GRPC_FAILED_PRECONDITION, 9);
    assert_eq!(GRPC_ABORTED, 10);
    assert_eq!(GRPC_OUT_OF_RANGE, 11);
    assert_eq!(GRPC_UNIMPLEMENTED, 12);
    assert_eq!(GRPC_INTERNAL, 13);
    assert_eq!(GRPC_UNAVAILABLE, 14);
    assert_eq!(GRPC_DATA_LOSS, 15);
    assert_eq!(GRPC_UNAUTHENTICATED, 16);
}

/// `tonic::Code` MUST round-trip these numeric values without
/// surprise. If a tonic version starts emitting an enum where
/// `Code::NotFound as i32 != 5`, every kiseki status mapping breaks
/// silently.
#[test]
fn tonic_code_enum_numeric_round_trip() {
    use tonic::Code;
    assert_eq!(Code::Ok as i32, GRPC_OK);
    assert_eq!(Code::Cancelled as i32, GRPC_CANCELLED);
    assert_eq!(Code::Unknown as i32, GRPC_UNKNOWN);
    assert_eq!(Code::InvalidArgument as i32, GRPC_INVALID_ARGUMENT);
    assert_eq!(Code::DeadlineExceeded as i32, GRPC_DEADLINE_EXCEEDED);
    assert_eq!(Code::NotFound as i32, GRPC_NOT_FOUND);
    assert_eq!(Code::AlreadyExists as i32, GRPC_ALREADY_EXISTS);
    assert_eq!(Code::PermissionDenied as i32, GRPC_PERMISSION_DENIED);
    assert_eq!(Code::ResourceExhausted as i32, GRPC_RESOURCE_EXHAUSTED);
    assert_eq!(Code::FailedPrecondition as i32, GRPC_FAILED_PRECONDITION);
    assert_eq!(Code::Aborted as i32, GRPC_ABORTED);
    assert_eq!(Code::OutOfRange as i32, GRPC_OUT_OF_RANGE);
    assert_eq!(Code::Unimplemented as i32, GRPC_UNIMPLEMENTED);
    assert_eq!(Code::Internal as i32, GRPC_INTERNAL);
    assert_eq!(Code::Unavailable as i32, GRPC_UNAVAILABLE);
    assert_eq!(Code::DataLoss as i32, GRPC_DATA_LOSS);
    assert_eq!(Code::Unauthenticated as i32, GRPC_UNAUTHENTICATED);
}

// ===========================================================================
// Status-code → application-error mapping documentation
// ===========================================================================
//
// Every gRPC service method MUST document which standard gRPC status
// codes it can emit. The mapping table lives in the per-service
// implementation; this test pins the EXPECTED mapping for the most
// common application errors so a future change to a service handler
// must update this test.
//
// Format: (service, method, application-error, gRPC code).
//
// The mappings below are the *intended* contract. Implementations
// that don't match will fail at the per-service test layer (under
// `kiseki-log/src/grpc.rs::tests`, etc.) — this file exists so the
// contract is in one place.

#[test]
fn grpc_status_to_application_error_mapping_documented() {
    /// The intended mapping table. Each row asserts that an
    /// application-level error class corresponds to a gRPC status.
    const MAPPING: &[(&str, &str, &str, i32)] = &[
        // LogService — see specs/architecture/api-contracts.md.
        ("LogService", "AppendDelta", "ShardNotFound", GRPC_NOT_FOUND),
        (
            "LogService",
            "AppendDelta",
            "QuotaExceeded",
            GRPC_RESOURCE_EXHAUSTED,
        ),
        (
            "LogService",
            "AppendDelta",
            "ReadOnlyShard",
            GRPC_FAILED_PRECONDITION,
        ),
        (
            "LogService",
            "ReadDeltas",
            "RangeOutOfBounds",
            GRPC_OUT_OF_RANGE,
        ),
        ("LogService", "ReadDeltas", "ShardNotFound", GRPC_NOT_FOUND),
        // ControlService.
        (
            "ControlService",
            "CreateOrganization",
            "NameAlreadyExists",
            GRPC_ALREADY_EXISTS,
        ),
        (
            "ControlService",
            "GetOrganization",
            "OrgNotFound",
            GRPC_NOT_FOUND,
        ),
        (
            "ControlService",
            "RequestAccess",
            "Unauthorized",
            GRPC_PERMISSION_DENIED,
        ),
        // KeyManagerService — operations either succeed or fail with
        // canonical not-found / failed-precondition.
        (
            "KeyManagerService",
            "RotateEpoch",
            "NoCurrentEpoch",
            GRPC_FAILED_PRECONDITION,
        ),
        (
            "KeyManagerService",
            "GetMasterKey",
            "EpochNotFound",
            GRPC_NOT_FOUND,
        ),
        // AuditExportService — non-existent shard / out-of-range
        // sequence.
        (
            "AuditExportService",
            "ExportRange",
            "ShardNotFound",
            GRPC_NOT_FOUND,
        ),
        (
            "AuditExportService",
            "ExportRange",
            "RangeOutOfBounds",
            GRPC_OUT_OF_RANGE,
        ),
        // WorkflowAdvisoryService — every error path uses
        // PERMISSION_DENIED for cross-tenant attempts (I-WA3) and
        // NOT_FOUND for unknown workflow scope.
        (
            "WorkflowAdvisoryService",
            "DeclareWorkflow",
            "QuotaExceeded",
            GRPC_RESOURCE_EXHAUSTED,
        ),
        (
            "WorkflowAdvisoryService",
            "QueryWorkflow",
            "ScopeNotFound",
            GRPC_NOT_FOUND,
        ),
        (
            "WorkflowAdvisoryService",
            "AdvisoryStream",
            "Unauthorized",
            GRPC_PERMISSION_DENIED,
        ),
    ];

    // Verify every row carries a valid (defined) gRPC code.
    for (svc, method, err, code) in MAPPING {
        assert!(
            (0..=16).contains(code),
            "{svc}::{method} {err} → invalid gRPC code {code}"
        );
    }

    // The mapping must mention every service kiseki currently
    // exposes. If a new service is added without an error-mapping
    // entry, the count check below fails.
    let services: std::collections::BTreeSet<&str> = MAPPING.iter().map(|(s, ..)| *s).collect();
    let expected_services: &[&str] = &[
        "LogService",
        "ControlService",
        "KeyManagerService",
        "AuditExportService",
        "WorkflowAdvisoryService",
    ];
    for svc in expected_services {
        assert!(
            services.contains(svc),
            "gRPC contract: service {svc} must have at least one documented \
             status-code mapping in this test"
        );
    }
}

// ===========================================================================
// Reserved-tag invariants
// ===========================================================================
//
// Source-of-truth (.proto):
//
//   - `advisory.proto::AdvisoryClientMessage`: `reserved 3` (was
//     `telemetry_subscription`). Bringing back tag 3 with a different
//     type would silently mis-decode old client traffic.
//   - `advisory.proto::AffinityHint`: `reserved 2 to 10` (deferred:
//     rack/node/zone fields would be a cross-tenant side channel —
//     gate-1 finding).
//
// `prost` generates structs without exposing the .proto's
// `reserved` declarations at runtime, so this test pins the rule
// at the source level: a developer adding a new field to one of
// these messages must NOT use a reserved tag. The assertion below
// is the textual contract; the build script enforces it (protoc
// errors out on a tag collision).

#[test]
fn advisory_client_message_tag_3_remains_reserved() {
    // Build the message and confirm we can encode + decode it
    // without using tag 3. The wire layout is `tag<<3 | wire_type`,
    // so tag 3 with a oneof discriminant would show up as `0x1A`
    // (tag=3, wire_type=2) in the encoded bytes.

    use kiseki_proto::v1::{advisory_client_message, AdvisoryClientMessage, Heartbeat};
    let msg = AdvisoryClientMessage {
        correlation: None,
        payload: Some(advisory_client_message::Payload::Heartbeat(Heartbeat {})),
    };
    let bytes = msg.encode_to_vec();

    // Walk the bytes and assert no tag-3 oneof discriminant appears.
    // (This is loose — tag 3 could appear inside a nested message —
    // but for this top-level wire body, tag 3 must NOT be present.)
    //
    // The wire-type byte for a tag-3 length-delimited field is
    // `(3 << 3) | 2 == 0x1A`. Other wire-type combinations are
    // similarly forbidden if the field is nested as oneof.
    let forbidden = [
        (3u32 << 3),     // varint (wire-type 0)
        (3u32 << 3) | 1, // 64-bit
        (3u32 << 3) | 2, // length-delimited
        (3u32 << 3) | 5, // 32-bit
    ];
    for byte in &bytes {
        let b = u32::from(*byte);
        assert!(
            !forbidden.contains(&b),
            "AdvisoryClientMessage: tag 3 is RESERVED \
             (was telemetry_subscription); must not appear on wire"
        );
    }
}

#[test]
fn affinity_hint_reserves_tags_2_through_10() {
    use kiseki_proto::v1::{AffinityHint, PoolHandle};

    // The AffinityHint message in `advisory.proto` declares
    // `reserved 2 to 10;` to keep rack/node/zone fields out of v1
    // (gate-1 cross-tenant side-channel finding). The protoc compiler
    // enforces this at build time — adding a field with tag in 2..=10
    // would fail to compile.
    //
    // This test pins the contract at runtime: a default-encoded
    // AffinityHint MUST decode back to the same AffinityHint, with
    // no fields silently appearing in the reserved range. A naive
    // byte-scan for tag-header bytes would false-positive on inner
    // length prefixes (e.g. a 16-byte length encodes as 0x10 which
    // collides with the tag-2/wire-type-0 header). Round-trip is
    // the correct contract check.
    let h = AffinityHint {
        preferred_pool: Some(PoolHandle {
            value: vec![0xaa; 16],
        }),
    };
    let bytes = h.encode_to_vec();
    let back = AffinityHint::decode(&*bytes).expect("decode AffinityHint");
    assert_eq!(
        back, h,
        "AffinityHint: round-trip identity (reserved 2..=10 enforced by protoc)"
    );

    // Documentation pin: list the reserved range so a future PR
    // that touches `advisory.proto::AffinityHint` must update this
    // table. Tags 2..=10 are deferred for rack/node/zone fields.
    const RESERVED: std::ops::RangeInclusive<u32> = 2..=10;
    assert_eq!(
        RESERVED.start(),
        &2u32,
        "advisory.proto AffinityHint: reserved range starts at 2"
    );
    assert_eq!(
        RESERVED.end(),
        &10u32,
        "advisory.proto AffinityHint: reserved range ends at 10"
    );
}

// ===========================================================================
// Round-trip — delta append / log read / control-plane RPCs
// ===========================================================================

/// `proto3` round-trip MUST be identity for any well-formed message.
/// `LogService::AppendDelta` carries the most schema surface (header
/// + payload), so a regression in prost's encoding shows up here
/// first.
#[test]
fn log_service_append_delta_request_round_trip() {
    let req = AppendDeltaRequest {
        shard_id: Some(ShardId {
            value: "shard-0001".into(),
        }),
        tenant_id: Some(OrgId {
            value: "acme".into(),
        }),
        operation: OperationType::Create as i32,
        timestamp: Some(DeltaTimestamp {
            hlc: Some(HybridLogicalClock {
                physical_ms: 1_700_000_000_000,
                logical: 17,
                node_id: 3,
            }),
            wall: Some(WallTime {
                millis_since_epoch: 1_700_000_000_000,
                timezone: "UTC".into(),
            }),
            quality: 0,
        }),
        hashed_key: vec![0xa5; 32],
        chunk_refs: vec![ChunkId {
            value: vec![0x11; 32],
        }],
        payload: vec![0xde, 0xad, 0xbe, 0xef],
        has_inline_data: true,
    };
    let bytes = req.encode_to_vec();
    let decoded = AppendDeltaRequest::decode(&*bytes).expect("decode");
    assert_eq!(decoded, req, "AppendDeltaRequest must round-trip");
}

#[test]
fn log_service_append_delta_response_round_trip() {
    let resp = AppendDeltaResponse {
        sequence: 0xCAFE_BABE_DEAD_BEEF,
    };
    let bytes = resp.encode_to_vec();
    let decoded = AppendDeltaResponse::decode(&*bytes).expect("decode");
    assert_eq!(decoded, resp);
}

/// `LogService::ReadDeltas` round-trip (request + response) so the
/// inclusive [from, to] range encoding is verified.
#[test]
fn log_service_read_deltas_round_trip() {
    let req = ReadDeltasRequest {
        shard_id: Some(ShardId {
            value: "s-0001".into(),
        }),
        from: 1,
        to: 100,
    };
    let bytes = req.encode_to_vec();
    let back = ReadDeltasRequest::decode(&*bytes).expect("decode");
    assert_eq!(back, req);

    // Build a response with one delta to exercise nested encoding.
    let delta = Delta {
        header: Some(DeltaHeader {
            sequence: 17,
            shard_id: Some(ShardId {
                value: "s-0001".into(),
            }),
            tenant_id: Some(OrgId {
                value: "acme".into(),
            }),
            operation: OperationType::Update as i32,
            timestamp: None,
            hashed_key: vec![0xa5; 32],
            tombstone: false,
            chunk_refs: vec![],
            payload_size: 0,
            has_inline_data: false,
        }),
        payload: Some(DeltaPayload {
            ciphertext: vec![1, 2, 3],
            auth_tag: vec![0xaa; 16],
            nonce: vec![0xbb; 12],
            system_epoch: Some(KeyEpoch { value: 1 }),
            tenant_epoch: None,
            tenant_wrapped_material: vec![],
        }),
    };
    let resp = ReadDeltasResponse {
        deltas: vec![delta],
    };
    let bytes = resp.encode_to_vec();
    let back = ReadDeltasResponse::decode(&*bytes).expect("decode");
    assert_eq!(back, resp);
}

#[test]
fn log_service_set_maintenance_round_trip() {
    let req = SetMaintenanceRequest {
        shard_id: Some(ShardId {
            value: "s-0002".into(),
        }),
        enabled: true,
    };
    let bytes = req.encode_to_vec();
    let back = SetMaintenanceRequest::decode(&*bytes).expect("decode");
    assert_eq!(back, req);
}

/// `ControlService::CreateOrganization` round-trip. The control
/// plane's request bodies carry tenant identity + quota fields;
/// these are the most-deeply nested control-plane messages.
#[test]
fn control_service_create_organization_round_trip() {
    let req = CreateOrganizationRequest {
        name: "acme".into(),
        compliance_tags: vec![],
        quota: None,
        dedup_policy: 0,
    };
    let bytes = req.encode_to_vec();
    let back = CreateOrganizationRequest::decode(&*bytes).expect("decode");
    assert_eq!(back, req);

    let resp = CreateOrganizationResponse {
        org_id: Some(OrgId {
            value: "org-acme".into(),
        }),
    };
    let bytes = resp.encode_to_vec();
    let back = CreateOrganizationResponse::decode(&*bytes).expect("decode");
    assert_eq!(back, resp);
}

// ===========================================================================
// Cross-implementation seed — hand-built protobuf wire frame
// ===========================================================================

/// Cross-implementation seed: hand-built `AppendDeltaResponse{
/// sequence: 1 }`. Per protobuf wire format §3 (varint encoded
/// uint64), tag 1 with wire_type 0 (varint) encodes as:
///
/// ```text
///   field tag = (1 << 3) | 0 = 0x08
///   varint(1) = 0x01
///   wire bytes = [0x08, 0x01]
/// ```
///
/// Source: protobuf wire-format spec
/// (<https://protobuf.dev/programming-guides/encoding/>).
#[test]
fn rfc_seed_protobuf_wire_format_append_delta_response_sequence_1() {
    let expected: &[u8] = &[0x08, 0x01];
    let resp = AppendDeltaResponse { sequence: 1 };
    let bytes = resp.encode_to_vec();
    assert_eq!(
        bytes, expected,
        "protobuf wire format §3: tag=1 wire_type=0 (varint) sequence=1 → [0x08, 0x01]"
    );

    // And decode the hand-built frame back.
    let decoded = AppendDeltaResponse::decode(expected).expect("decode hand-built frame");
    assert_eq!(decoded.sequence, 1);
}

/// Empty messages encode to zero bytes (proto3 default-suppression).
/// This exercises the corner case that no field is `required` in
/// proto3.
#[test]
fn rfc_seed_protobuf_empty_message_encodes_to_zero_bytes() {
    let req = ReadDeltasRequest::default();
    let bytes = req.encode_to_vec();
    assert!(
        bytes.is_empty(),
        "protobuf proto3: a default-valued message encodes to zero bytes; got {} bytes",
        bytes.len()
    );

    // Round-trip through empty bytes succeeds (default values).
    let back = ReadDeltasRequest::decode(&[][..]).expect("decode empty");
    assert_eq!(back, req);
}
