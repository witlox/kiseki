#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Layer 1 reference tests for **RFC 7530 — Network File System
//! (NFS) Version 4 Protocol** (March 2015).
//!
//! Kiseki advertises NFSv4.1+ (RFC 8881 / RFC 7862) and shares the
//! same `nfs4_server` module for all 4.x minor versions. RFC 7530
//! is therefore the **fallback substrate**: a real-world Linux client
//! that opens a `mount.nfs4 -o vers=4.0` connection, or any client
//! whose first COMPOUND advertises `minor_version=0`, MUST be handled
//! cleanly. The two acceptable outcomes per RFC 7530 §15.1 +
//! RFC 8881 §2.10.5 are:
//!
//! 1. process the COMPOUND as a 4.0 request (if the server supports
//!    4.0), or
//! 2. reject every op with `NFS4ERR_MINOR_VERS_MISMATCH` (10021).
//!
//! What the server MUST NOT do: silently treat 4.0 as 4.1+ and
//! emit a 4.1+ reply shape (e.g. `EXCHANGE_ID` results) the client
//! cannot parse. That's a fidelity gap and a hang at mount time.
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::nfs4_server` — same dispatcher used for
//! 4.1/4.2.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 7530".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc7530>.
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
    clippy::unicode_not_nfc,
    clippy::unusual_byte_groupings
)]

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::nfs::NfsGateway;
use kiseki_gateway::nfs4_server::{
    handle_nfs4_first_compound, nfs4_status, op as v4op, SessionManager,
};
use kiseki_gateway::nfs_ops::NfsContext;
use kiseki_gateway::nfs_xdr::{RpcCallHeader, XdrReader, XdrWriter};

// ===========================================================================
// Sentinel constants — pin the wire registry per RFC 7530 §15
// ===========================================================================

/// RFC 7530 §1.4.1 — NFSv4 program/version sentinels. Same program
/// number as v3 (100003); the version field selects the major
/// version. The minor version rides INSIDE the COMPOUND body.
const NFS4_PROGRAM: u32 = 100003;
const NFS4_VERSION: u32 = 4;

/// RFC 7530 §15.1 — NFSv4 has only TWO procedures at the ONC RPC
/// layer. Everything else is encoded as a COMPOUND op.
const PROC_NULL: u32 = 0;
const PROC_COMPOUND: u32 = 1;

/// RFC 7530 §13.1 — `NFS4ERR_MINOR_VERS_MISMATCH = 10021`. Returned
/// when the server doesn't support the requested minor_version.
/// We assert the wire constant here so a refactor cannot accidentally
/// renumber it.
const NFS4ERR_MINOR_VERS_MISMATCH: u32 = 10021;

/// RFC 7530 §15.1 — pin the procedure registry. NFSv4 has exactly
/// two procedures at the RPC layer; any other procedure number MUST
/// produce PROC_UNAVAIL (RFC 5531 §9.2 accept_stat=3).
#[test]
fn s15_1_procedure_registry_pinned() {
    assert_eq!(NFS4_PROGRAM, 100003, "RFC 7530 §1.4.1: program = 100003");
    assert_eq!(NFS4_VERSION, 4, "RFC 7530 §1.4.1: version = 4");
    assert_eq!(PROC_NULL, 0, "RFC 7530 §15.1: NULL = procedure 0");
    assert_eq!(PROC_COMPOUND, 1, "RFC 7530 §15.1: COMPOUND = procedure 1");
    // The full registry. NFSv4 deliberately collapses the v3
    // procedure list into ops carried inside COMPOUND.
    let registry: &[(u32, &str)] = &[(PROC_NULL, "NULL"), (PROC_COMPOUND, "COMPOUND")];
    assert_eq!(
        registry.len(),
        2,
        "RFC 7530 §15.1: NFSv4 RPC defines exactly two procedures"
    );
}

/// RFC 7530 §13.1 — pin the NFS4ERR_MINOR_VERS_MISMATCH constant.
/// This is the spec-mandated reply when minor_version is unsupported.
#[test]
fn s13_1_minor_vers_mismatch_status_pinned() {
    assert_eq!(
        NFS4ERR_MINOR_VERS_MISMATCH, 10021,
        "RFC 7530 §13.1: NFS4ERR_MINOR_VERS_MISMATCH = 10021"
    );
}

// ===========================================================================
// Test fixtures — context and call frame helpers
// ===========================================================================

const TEST_TENANT_ID: u128 = 0xC0FFEE_DEADBEEF_C0FFEE_DEADBEEFu128;
const TEST_NS_ID: u128 = 0xCAFEBABE_BAADF00D_CAFEBABE_BAADF00Du128;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(TEST_TENANT_ID))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(TEST_NS_ID))
}

fn make_ctx() -> NfsContext<InMemoryGateway> {
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
    let nfs_gw = NfsGateway::new(gw);
    NfsContext::new(nfs_gw, test_tenant(), test_namespace())
}

/// Build an ONC-RPC v2 call frame for an NFSv4 message.
fn build_nfs4_call(xid: u32, procedure: u32, body: &[u8]) -> Vec<u8> {
    let mut w = XdrWriter::new();
    w.write_u32(xid);
    w.write_u32(0); // CALL
    w.write_u32(2); // RPC v2
    w.write_u32(NFS4_PROGRAM);
    w.write_u32(NFS4_VERSION);
    w.write_u32(procedure);
    // AUTH_NONE creds + verifier per RFC 1057 §9.1.
    w.write_u32(0);
    w.write_opaque(&[]);
    w.write_u32(0);
    w.write_opaque(&[]);
    let mut buf = w.into_bytes();
    buf.extend_from_slice(body);
    buf
}

fn make_header(xid: u32, procedure: u32) -> RpcCallHeader {
    RpcCallHeader {
        xid,
        program: NFS4_PROGRAM,
        version: NFS4_VERSION,
        procedure,
    }
}

/// Encode an NFSv4 COMPOUND argument body per RFC 7530 §16.2:
/// `tag(opaque), minor_version(u32), array<nfs_argop4>`. Each
/// `nfs_argop4` is a discriminated union — for ops with no body we
/// emit just the op_code u32.
fn encode_compound_body(tag: &[u8], minor_version: u32, ops: &[u32]) -> Vec<u8> {
    let mut w = XdrWriter::new();
    w.write_opaque(tag);
    w.write_u32(minor_version);
    w.write_u32(ops.len() as u32);
    for op in ops {
        w.write_u32(*op);
        // PUTROOTFH (24) takes no args; GETATTR (9) needs a bitmap.
        if *op == v4op::GETATTR {
            // Empty attribute bitmap: count=0.
            w.write_u32(0);
        }
    }
    w.into_bytes()
}

/// Walk past the ONC-RPC accepted-reply preamble and return a reader
/// positioned at the COMPOUND result (status + tag + resarray_len).
fn reader_at_compound_result(reply: &[u8]) -> XdrReader<'_> {
    let mut r = XdrReader::new(reply);
    let _xid = r.read_u32().expect("xid");
    let _msg_type = r.read_u32().expect("msg_type");
    let _reply_stat = r.read_u32().expect("reply_stat");
    let _vf = r.read_u32().expect("verf flavor");
    let _vlen = r.read_u32().expect("verf length");
    let accept_stat = r.read_u32().expect("accept_stat");
    assert_eq!(
        accept_stat, 0,
        "RFC 5531: COMPOUND envelope MUST be MSG_ACCEPTED + SUCCESS"
    );
    r
}

// ===========================================================================
// §15.1 — NULL procedure (also pinned by RFC 8881 §16.1)
// ===========================================================================

/// RFC 7530 §15.1 — NULL procedure: empty CALL body, empty REPLY
/// body. Linux `mount.nfs4` pings NULL before any COMPOUND; the
/// 2026-04-27 production bug rejected this with PROC_UNAVAIL. The
/// fix landed in commit `5f6fece`. This test pins the contract so it
/// cannot regress.
#[test]
fn s15_1_null_returns_empty_accept_ok() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(1, PROC_NULL);
    let raw = build_nfs4_call(1, PROC_NULL, &[]);

    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = XdrReader::new(&reply);
    let xid = r.read_u32().unwrap();
    assert_eq!(xid, 1, "xid echoed");
    let _msg_type = r.read_u32().unwrap();
    let _reply_stat = r.read_u32().unwrap();
    let _vf = r.read_u32().unwrap();
    let _vlen = r.read_u32().unwrap();
    let accept_stat = r.read_u32().unwrap();
    assert_eq!(
        accept_stat, 0,
        "RFC 7530 §15.1: NULL MUST yield ACCEPT_OK (regression: commit 5f6fece)"
    );
    assert_eq!(
        r.remaining(),
        0,
        "RFC 7530 §15.1: NULL reply MUST have an empty body"
    );
}

// ===========================================================================
// §15.2 — COMPOUND happy path with minor_version=0 (RFC 7530)
// ===========================================================================

/// RFC 7530 §15.2 + §16.2.4 — a 4.0 client sends `COMPOUND{
/// minor_version=0, ops=[PUTROOTFH, GETATTR] }`. A spec-compliant
/// 4.0 server returns a COMPOUND-status of NFS4_OK and the two op
/// results in order.
///
/// Today's `nfs4_server` does NOT branch on `minor_version`: it
/// processes 4.0 and 4.1+ requests through the same dispatcher.
/// That's permissive — the GETATTR result shape is the same shape
/// the 4.1 client expects. A 4.0 client may decode it correctly, OR
/// may choke on a 4.1-only attr field. This positive test asserts
/// the request succeeds end-to-end; the negative test below covers
/// the strict path where the server rejects 4.0 outright.
#[test]
fn s15_2_compound_minor_v0_putrootfh_getattr_succeeds_or_rejects_cleanly() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let body = encode_compound_body(b"", 0, &[v4op::PUTROOTFH, v4op::GETATTR]);
    let header = make_header(2, PROC_COMPOUND);
    let raw = build_nfs4_call(2, PROC_COMPOUND, &body);

    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    let _reply_tag = r.read_opaque().expect("reply tag");
    let resarray_len = r.read_u32().expect("resarray length");

    // Two acceptable outcomes per RFC 7530 §15.1 + RFC 8881 §2.10.5:
    //
    //   (a) compound_status == NFS4_OK and PUTROOTFH/GETATTR results
    //       follow (server treats 4.0 as a supported minor version);
    //   (b) compound_status == NFS4ERR_MINOR_VERS_MISMATCH and the
    //       resarray is empty (server rejects 4.0 cleanly).
    //
    // Today's server takes path (a) without explicit 4.0 support —
    // captured below as the strict-mode RED test.
    assert!(
        compound_status == nfs4_status::NFS4_OK || compound_status == NFS4ERR_MINOR_VERS_MISMATCH,
        "RFC 7530 §15.2: minor_version=0 MUST yield NFS4_OK or \
         NFS4ERR_MINOR_VERS_MISMATCH; got {compound_status}"
    );
    if compound_status == nfs4_status::NFS4_OK {
        assert_eq!(
            resarray_len, 2,
            "RFC 7530 §16.2.4: NFS4_OK COMPOUND MUST emit one result per submitted op"
        );
    } else {
        assert_eq!(
            resarray_len, 0,
            "RFC 7530 §13.1: NFS4ERR_MINOR_VERS_MISMATCH COMPOUND has empty resarray"
        );
    }
}

// ===========================================================================
// §13.1 — strict negotiation: minor_version=0 SHOULD be flagged
// ===========================================================================

/// RFC 7530 §13.1 + RFC 8881 §2.10.5 — a server that doesn't
/// implement minor_version=0 MUST emit `NFS4ERR_MINOR_VERS_MISMATCH`
/// (10021). Kiseki's catalog row says "NFSv4.0 fallback" is required
/// but does NOT claim full 4.0 support. The strict reading is: 4.0
/// requests should fail cleanly with 10021 rather than be silently
/// promoted to 4.1+ semantics.
///
/// Today's server reads `minor_version` and ignores it (see
/// `dispatch_compound`'s `let _minor_version = ...`), so this test
/// is RED until version negotiation is added.
#[test]
fn s13_1_minor_v0_compound_should_return_minor_vers_mismatch() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let body = encode_compound_body(b"", 0, &[v4op::PUTROOTFH]);
    let header = make_header(3, PROC_COMPOUND);
    let raw = build_nfs4_call(3, PROC_COMPOUND, &body);

    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");

    assert_eq!(
        compound_status, NFS4ERR_MINOR_VERS_MISMATCH,
        "RFC 7530 §13.1: minor_version=0 from a 4.0-only client MUST yield \
         NFS4ERR_MINOR_VERS_MISMATCH (10021); kiseki currently advertises 4.1+ \
         and silently promotes 4.0 — fidelity gap"
    );
}

// ===========================================================================
// §15.1 — unknown procedure number → PROC_UNAVAIL
// ===========================================================================

/// RFC 7530 §15.1 + RFC 5531 §9.2 — any procedure number other than
/// 0 (NULL) or 1 (COMPOUND) MUST produce `PROC_UNAVAIL` (accept_stat
/// = 3). This is the wire-side guard that prevents a malformed or
/// extension-probing client from confusing the dispatcher.
#[test]
fn s15_1_unknown_procedure_returns_proc_unavail() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(4, 99); // procedure 99 doesn't exist
    let raw = build_nfs4_call(4, 99, &[]);

    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = XdrReader::new(&reply);
    let _xid = r.read_u32().unwrap();
    let _msg_type = r.read_u32().unwrap();
    let _reply_stat = r.read_u32().unwrap();
    let _vf = r.read_u32().unwrap();
    let _vlen = r.read_u32().unwrap();
    let accept_stat = r.read_u32().unwrap();
    assert_eq!(
        accept_stat, 3,
        "RFC 5531 §9.2 + RFC 7530 §15.1: unknown procedure MUST yield PROC_UNAVAIL (3)"
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 7530 §16 canonical 4.0 COMPOUND
// ===========================================================================

/// RFC 7530 §16.2 / §16.2.4 specifies the COMPOUND argument layout:
///
/// ```text
/// struct COMPOUND4args {
///     utf8str_cs   tag;
///     uint32_t     minorversion;   /* 0 for RFC 7530 */
///     nfs_argop4   argarray<>;
/// };
/// ```
///
/// Concrete byte-for-byte 4.0 seed: empty tag, `minorversion=0`,
/// one op (`PUTROOTFH`, op=24). This is what every Linux 4.0
/// COMPOUND wire log starts with, modulo the empty tag (some
/// clients embed a debug string).
#[test]
fn rfc_7530_seed_canonical_minor_v0_compound_body() {
    let body = encode_compound_body(b"", 0, &[v4op::PUTROOTFH]);
    let expected: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x00, // tag length = 0
        0x00, 0x00, 0x00, 0x00, // minorversion = 0  (RFC 7530!)
        0x00, 0x00, 0x00, 0x01, // argarray length = 1
        0x00, 0x00, 0x00, 0x18, // op = 24 (PUTROOTFH)
    ];
    assert_eq!(
        body, expected,
        "RFC 7530 §16.2: 4.0 COMPOUND body shape pinned byte-for-byte"
    );

    // Drive the seed through the dispatcher and assert the response
    // is one of the two acceptable outcomes.
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(0xCAFE_BABE, PROC_COMPOUND);
    let raw = build_nfs4_call(0xCAFE_BABE, PROC_COMPOUND, &body);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    assert!(
        compound_status == nfs4_status::NFS4_OK || compound_status == NFS4ERR_MINOR_VERS_MISMATCH,
        "RFC 7530 §16.2 seed: dispatcher MUST emit NFS4_OK or \
         NFS4ERR_MINOR_VERS_MISMATCH for a 4.0 PUTROOTFH; got {compound_status}"
    );
}
