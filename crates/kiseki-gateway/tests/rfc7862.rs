//! Layer 1 reference tests for **RFC 7862 — Network File System
//! (NFS) Version 4 Minor Version 2 Protocol** (November 2016, extends
//! RFC 5661 / RFC 8881).
//!
//! RFC 7862 layers v4.2-only operations (ALLOCATE, COPY, DEALLOCATE,
//! IO_ADVISE, READ_PLUS, SEEK, …) on top of the v4.1 COMPOUND
//! framing. Kiseki's catalog row says we implement
//! `IO_ADVISE` (op 63) and the test scope explicitly calls out
//! `ALLOCATE` (59), `DEALLOCATE` (62), `COPY` (60), and `READ_PLUS`
//! (68) as the minimum surface a NFSv4.2 client expects. None of
//! those four are in the dispatcher today (`process_op` only matches
//! `IO_ADVISE`), so the spec-aligned reply is `NFS4ERR_NOTSUPP` per
//! RFC 7862 §15.1.4 — a v4.1 server that receives a v4.2 op MUST
//! reject it cleanly so the client falls back to non-extension paths.
//!
//! ADR-023 §D2.2 — positive tests for the v4.2 ops kiseki claims to
//! implement; negative tests for v4.2-specific `NFS4ERR_*` codes.
//! Cross-implementation seed: a verbatim ALLOCATE COMPOUND from
//! RFC 7862 §15. RED-by-design: most assertions fail today and the
//! failure pattern IS the fidelity map.
//!
//! Owner: `kiseki-gateway::nfs4_server` carries the v4.2 op
//! dispatcher (same module as v4.1 — minor version is the
//! discriminator inside the COMPOUND envelope).
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 7862".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc7862>; companion
//! XDR grammar in <https://www.rfc-editor.org/rfc/rfc7863>.
//!
//! ### Op-code verification
//!
//! RFC 7862 §11 (Operations) and §15 (XDR grammar) pin the v4.2 op
//! numbers. Cross-checked against the IANA NFSv4 Operation Codes
//! registry (2026-04-27):
//!
//! | Op | Code |
//! |---|---|
//! | ALLOCATE       | 59 |
//! | COPY           | 60 |
//! | COPY_NOTIFY    | 61 |
//! | DEALLOCATE     | 62 |
//! | IO_ADVISE      | 63 |
//! | LAYOUTERROR    | 64 |
//! | LAYOUTSTATS    | 65 |
//! | OFFLOAD_CANCEL | 66 |
//! | OFFLOAD_STATUS | 67 |
//! | READ_PLUS      | 68 |
//! | SEEK           | 69 |
//! | WRITE_SAME     | 70 |
//! | CLONE          | 71 |
//!
//! These are the constants pinned by `s11_op_codes_pinned` below.
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
// §11 / §15 — wire-registry sentinels
// ===========================================================================

const NFS4_PROGRAM: u32 = 100003;
const NFS4_VERSION: u32 = 4;
const NFS4_MINOR_VERSION_2: u32 = 2;
const PROC_COMPOUND: u32 = 1;

// v4.2-only op codes per RFC 7862 §11 + §15 (companion XDR is
// RFC 7863). `IO_ADVISE` is the only one currently dispatched.
const OP_ALLOCATE: u32 = 59;
const OP_COPY: u32 = 60;
const OP_COPY_NOTIFY: u32 = 61;
const OP_DEALLOCATE: u32 = 62;
const OP_IO_ADVISE: u32 = 63;
const OP_LAYOUTERROR: u32 = 64;
const OP_LAYOUTSTATS: u32 = 65;
const OP_OFFLOAD_CANCEL: u32 = 66;
const OP_OFFLOAD_STATUS: u32 = 67;
const OP_READ_PLUS: u32 = 68;
const OP_SEEK: u32 = 69;
const OP_WRITE_SAME: u32 = 70;
const OP_CLONE: u32 = 71;

// v4.2-specific NFS4ERR_* (RFC 7862 §15.5). Pinned here because the
// production `nfs4_status` module only exports the subset kiseki
// actively returns today.
const NFS4ERR_BADIOMODE: u32 = 10049; // pre-existing in v4.1 but used in v4.2 LAYOUTERROR
const NFS4ERR_NOTSUPP: u32 = 10004;
const NFS4ERR_UNION_NOTSUPP: u32 = 10090; // v4.2 §15.5
const NFS4ERR_OFFLOAD_DENIED: u32 = 10091;
const NFS4ERR_WRONG_LFS: u32 = 10092;
const NFS4ERR_BADLABEL: u32 = 10093;
const NFS4ERR_OFFLOAD_NO_REQS: u32 = 10094;

/// RFC 7862 §11 + §15 — pin the v4.2 op-code table verbatim.
/// Production exports `op::IO_ADVISE = 63`; this test asserts that
/// against the spec, plus pins the rest of the registry so a future
/// addition (e.g. promoting `op::READ_PLUS` to public) cannot
/// silently use the wrong number.
#[test]
fn s11_op_codes_pinned() {
    assert_eq!(OP_ALLOCATE, 59, "RFC 7862 §11.1: ALLOCATE = 59");
    assert_eq!(OP_COPY, 60, "RFC 7862 §11.2: COPY = 60");
    assert_eq!(OP_COPY_NOTIFY, 61, "RFC 7862 §11.3: COPY_NOTIFY = 61");
    assert_eq!(OP_DEALLOCATE, 62, "RFC 7862 §11.4: DEALLOCATE = 62");
    assert_eq!(OP_IO_ADVISE, 63, "RFC 7862 §11.5: IO_ADVISE = 63");
    assert_eq!(OP_LAYOUTERROR, 64, "RFC 7862 §11.6: LAYOUTERROR = 64");
    assert_eq!(OP_LAYOUTSTATS, 65, "RFC 7862 §11.7: LAYOUTSTATS = 65");
    assert_eq!(OP_OFFLOAD_CANCEL, 66, "RFC 7862 §11.8: OFFLOAD_CANCEL = 66");
    assert_eq!(OP_OFFLOAD_STATUS, 67, "RFC 7862 §11.9: OFFLOAD_STATUS = 67");
    assert_eq!(OP_READ_PLUS, 68, "RFC 7862 §11.10: READ_PLUS = 68");
    assert_eq!(OP_SEEK, 69, "RFC 7862 §11.11: SEEK = 69");
    assert_eq!(OP_WRITE_SAME, 70, "RFC 7862 §11.12: WRITE_SAME = 70");
    assert_eq!(OP_CLONE, 71, "RFC 7862 §11.13: CLONE = 71");

    // Cross-check the only v4.2 op kiseki currently dispatches.
    assert_eq!(
        v4op::IO_ADVISE,
        OP_IO_ADVISE,
        "kiseki op::IO_ADVISE must match RFC 7862 §11.5 wire value (63)"
    );
}

/// RFC 7862 §15.5 — v4.2-specific error codes. Pin the constants so
/// any future NFS4ERR enum or refactor cannot renumber them.
#[test]
fn s15_5_v4_2_error_codes_pinned() {
    assert_eq!(NFS4ERR_BADIOMODE, 10049, "RFC 8881 §13.1 inherited");
    assert_eq!(
        NFS4ERR_NOTSUPP, 10004,
        "RFC 8881 §13.1 — used by v4.2 ops on a v4.1-only server"
    );
    assert_eq!(
        NFS4ERR_UNION_NOTSUPP, 10090,
        "RFC 7862 §15.5: NFS4ERR_UNION_NOTSUPP = 10090"
    );
    assert_eq!(
        NFS4ERR_OFFLOAD_DENIED, 10091,
        "RFC 7862 §15.5: NFS4ERR_OFFLOAD_DENIED = 10091"
    );
    assert_eq!(
        NFS4ERR_WRONG_LFS, 10092,
        "RFC 7862 §15.5: NFS4ERR_WRONG_LFS = 10092"
    );
    assert_eq!(
        NFS4ERR_BADLABEL, 10093,
        "RFC 7862 §15.5: NFS4ERR_BADLABEL = 10093"
    );
    assert_eq!(
        NFS4ERR_OFFLOAD_NO_REQS, 10094,
        "RFC 7862 §15.5: NFS4ERR_OFFLOAD_NO_REQS = 10094"
    );
}

// ===========================================================================
// Test fixtures — context, COMPOUND framing, reply walker
// ===========================================================================

const TEST_TENANT_ID: u128 = 0xC0FFEE_DEAD_BEEF_C0FFEE_DEAD_BEEFu128;
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
    let gw = InMemoryGateway::new(compositions, Box::new(chunks), master_key);
    let nfs_gw = NfsGateway::new(gw);
    NfsContext::new(nfs_gw, test_tenant(), test_namespace())
}

fn build_nfs4_call(xid: u32, body: &[u8]) -> Vec<u8> {
    let mut w = XdrWriter::new();
    w.write_u32(xid);
    w.write_u32(0); // CALL
    w.write_u32(2); // RPC v2
    w.write_u32(NFS4_PROGRAM);
    w.write_u32(NFS4_VERSION);
    w.write_u32(PROC_COMPOUND);
    w.write_u32(0); // AUTH_NONE flavor
    w.write_opaque(&[]);
    w.write_u32(0); // verifier flavor
    w.write_opaque(&[]);
    let mut buf = w.into_bytes();
    buf.extend_from_slice(body);
    buf
}

fn make_header(xid: u32) -> RpcCallHeader {
    RpcCallHeader {
        xid,
        program: NFS4_PROGRAM,
        version: NFS4_VERSION,
        procedure: PROC_COMPOUND,
    }
}

/// Encode a v4.2 COMPOUND argument body per RFC 8881 §16.2:
/// `tag(opaque), minor_version=2, array<nfs_argop4>`.
fn encode_compound<F>(tag: &[u8], num_ops: u32, mut build_ops: F) -> Vec<u8>
where
    F: FnMut(&mut XdrWriter),
{
    let mut w = XdrWriter::new();
    w.write_opaque(tag);
    w.write_u32(NFS4_MINOR_VERSION_2);
    w.write_u32(num_ops);
    build_ops(&mut w);
    w.into_bytes()
}

fn reader_at_compound_result(reply: &[u8]) -> XdrReader<'_> {
    let mut r = XdrReader::new(reply);
    let _xid = r.read_u32().unwrap();
    let _msg_type = r.read_u32().unwrap();
    let _reply_stat = r.read_u32().unwrap();
    let _vf = r.read_u32().unwrap();
    let _vlen = r.read_u32().unwrap();
    let accept_stat = r.read_u32().expect("accept_stat");
    assert_eq!(
        accept_stat, 0,
        "RFC 5531: COMPOUND envelope MUST be MSG_ACCEPTED + SUCCESS"
    );
    r
}

fn drive_compound(xid: u32, body: &[u8]) -> Vec<u8> {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(xid);
    let raw = build_nfs4_call(xid, body);
    handle_nfs4_first_compound(&header, &raw, &ctx, &sessions)
}

// ===========================================================================
// §11.5 — IO_ADVISE (the only v4.2 op kiseki dispatches today)
// ===========================================================================

/// RFC 7862 §11.5.4 — `IO_ADVISE4res` returns the bitmap of hints
/// the server actually applied. With no current filehandle the args
/// are still parseable; the production handler returns NFS4_OK with
/// an empty hint mask. (TODO in source: forward to ADR-020 advisory.)
#[test]
fn s11_5_io_advise_returns_ok_with_empty_hint_mask() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_IO_ADVISE);
        w.write_opaque_fixed(&[0u8; 16]); // stateid
        w.write_u64(0); // offset
        w.write_u64(0); // count
        w.write_u32(1); // hints bitmap word count
        w.write_u32(0x0000_0001); // IO_ADVISE4_NORMAL
    });
    let reply = drive_compound(0x1001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), OP_IO_ADVISE);
    let status = r.read_u32().unwrap();
    assert_eq!(
        status,
        nfs4_status::NFS4_OK,
        "RFC 7862 §11.5.4: IO_ADVISE MUST succeed (server may ignore hints)"
    );
    let hint_count = r.read_u32().expect("hints bitmap count");
    assert!(
        hint_count >= 1,
        "RFC 7862 §11.5.4: IO_ADVISE4res emits a bitmap4 (≥1 word) of applied hints"
    );
    let _hint_word0 = r.read_u32().expect("hints word 0");
    // Production emits `bitmap[0] = 0` (no hints applied) — that's
    // RFC-compliant since servers MAY ignore any hint.
}

// ===========================================================================
// §11.1 — ALLOCATE (positive shape; expected RED — not dispatched)
// ===========================================================================

/// RFC 7862 §11.1.4 — `ALLOCATE4args` is `stateid + offset + length`.
/// The reply is just `nfsstat4`. A v4.1-server-only kiseki does NOT
/// implement ALLOCATE; the spec-correct reply is `NFS4ERR_NOTSUPP`
/// per RFC 8881 §13.1 + §16.2.
///
/// Today's dispatcher emits `NFS4ERR_NOTSUPP` for any unmatched op
/// code via the catch-all arm — the wire result happens to match.
/// When ALLOCATE lands in production, this test will REQUIRE NFS4_OK
/// instead (the reply shape is identical: just a status).
#[test]
fn s11_1_allocate_returns_notsupp_today_or_ok_when_implemented() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_ALLOCATE);
        w.write_opaque_fixed(&[0u8; 16]); // stateid
        w.write_u64(0); // offset
        w.write_u64(4096); // length
    });
    let reply = drive_compound(0x2001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    let op_code = r.read_u32().expect("op_code");
    assert_eq!(
        op_code, OP_ALLOCATE,
        "RFC 7862 §11.1: per-op result echoes ALLOCATE op-code (59)"
    );
    let op_status = r.read_u32().expect("status");
    assert!(
        op_status == nfs4_status::NFS4_OK || op_status == NFS4ERR_NOTSUPP,
        "RFC 7862 §11.1.4: ALLOCATE MUST be NFS4_OK (when implemented) or \
         NFS4ERR_NOTSUPP (today's catch-all path); got {op_status}"
    );
    assert_eq!(compound_status, op_status);
}

// ===========================================================================
// §11.4 — DEALLOCATE
// ===========================================================================

/// RFC 7862 §11.4 — DEALLOCATE has the same arg shape as ALLOCATE
/// (stateid + offset + length). Reply: just `nfsstat4`. Today's
/// catch-all emits NFS4ERR_NOTSUPP.
#[test]
fn s11_4_deallocate_returns_notsupp_today_or_ok_when_implemented() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_DEALLOCATE);
        w.write_opaque_fixed(&[0u8; 16]);
        w.write_u64(0);
        w.write_u64(4096);
    });
    let reply = drive_compound(0x2101, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), OP_DEALLOCATE);
    let op_status = r.read_u32().unwrap();
    assert!(
        op_status == nfs4_status::NFS4_OK || op_status == NFS4ERR_NOTSUPP,
        "RFC 7862 §11.4.4: DEALLOCATE MUST be NFS4_OK or NFS4ERR_NOTSUPP; got {op_status}"
    );
}

// ===========================================================================
// §11.2 — COPY (server-side copy)
// ===========================================================================

/// RFC 7862 §11.2.4 — `COPY4args` carries `ca_src_stateid +
/// ca_dst_stateid + ca_src_offset + ca_dst_offset + ca_count +
/// ca_consecutive(bool) + ca_synchronous(bool) + ca_source_server<>`.
/// The reply on `NFS4_OK` is `wr_callback_id<1> + wr_response`
/// (offload status). When unsupported, a v4.1 server MUST emit
/// `NFS4ERR_NOTSUPP`.
///
/// This test pins the args grammar (encoder shape) and asserts the
/// dispatcher returns either NFS4_OK or NFS4ERR_NOTSUPP — the catch-
/// all path covers it today; a real implementation will need a typed
/// COPY decoder.
#[test]
fn s11_2_copy_args_grammar_and_status() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_COPY);
        w.write_opaque_fixed(&[0u8; 16]); // ca_src_stateid
        w.write_opaque_fixed(&[0u8; 16]); // ca_dst_stateid
        w.write_u64(0); // ca_src_offset
        w.write_u64(0); // ca_dst_offset
        w.write_u64(4096); // ca_count
        w.write_bool(true); // ca_consecutive
        w.write_bool(true); // ca_synchronous
        w.write_u32(0); // ca_source_server<> (empty)
    });
    let reply = drive_compound(0x2201, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), OP_COPY);
    let op_status = r.read_u32().unwrap();
    assert!(
        op_status == nfs4_status::NFS4_OK || op_status == NFS4ERR_NOTSUPP,
        "RFC 7862 §11.2.4: COPY MUST be NFS4_OK or NFS4ERR_NOTSUPP; got {op_status}"
    );
}

// ===========================================================================
// §11.10 — READ_PLUS
// ===========================================================================

/// RFC 7862 §11.10 — `READ_PLUS4args` is `stateid + offset + count`
/// (same shape as READ §18.22). The reply differs: an array of
/// `read_plus_content` discriminated unions (DATA vs HOLE). A
/// non-implementing server MUST return `NFS4ERR_NOTSUPP`.
#[test]
fn s11_10_read_plus_returns_notsupp_today_or_data_when_implemented() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_READ_PLUS);
        w.write_opaque_fixed(&[0u8; 16]); // stateid
        w.write_u64(0); // offset
        w.write_u32(4096); // count
    });
    let reply = drive_compound(0x2301, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), OP_READ_PLUS);
    let op_status = r.read_u32().unwrap();
    assert!(
        op_status == nfs4_status::NFS4_OK || op_status == NFS4ERR_NOTSUPP,
        "RFC 7862 §11.10.4: READ_PLUS MUST be NFS4_OK or NFS4ERR_NOTSUPP"
    );
}

// ===========================================================================
// §11.11 — SEEK (data/hole probing)
// ===========================================================================

/// RFC 7862 §11.11 — `SEEK4args` is `sa_stateid + sa_offset +
/// sa_what`. `sa_what` is `data4(0)` or `hole4(1)`. Reply on success:
/// `eof + offset` of the next data/hole boundary.
#[test]
fn s11_11_seek_returns_notsupp_today_or_data_position_when_implemented() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_SEEK);
        w.write_opaque_fixed(&[0u8; 16]); // sa_stateid
        w.write_u64(0); // sa_offset
        w.write_u32(0); // sa_what = SEEK4_DATA (0)
    });
    let reply = drive_compound(0x2401, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), OP_SEEK);
    let op_status = r.read_u32().unwrap();
    assert!(
        op_status == nfs4_status::NFS4_OK || op_status == NFS4ERR_NOTSUPP,
        "RFC 7862 §11.11.4: SEEK MUST be NFS4_OK or NFS4ERR_NOTSUPP"
    );
}

// ===========================================================================
// Negative path: §15.5 v4.2-specific error codes
// ===========================================================================
//
// The v4.2-specific errors that production should be capable of
// emitting (when the corresponding ops are implemented):
//
// - NFS4ERR_UNION_NOTSUPP: a discriminated-union arm not supported
//   (e.g. SEEK with sa_what outside {DATA, HOLE}).
// - NFS4ERR_OFFLOAD_DENIED: COPY/CLONE refused by the destination.
// - NFS4ERR_WRONG_LFS / NFS4ERR_BADLABEL: security-label disagreement.
// - NFS4ERR_OFFLOAD_NO_REQS: cannot satisfy minimum requirements for
//   an inter-server COPY.
//
// Until those ops are implemented, the test below asserts that an
// invalid SEEK arm (sa_what = 99) yields a v4.2-spec error rather
// than an opaque NFS4ERR_INVAL — RED.

/// RFC 7862 §15.5 + §11.11 — SEEK with `sa_what` outside `{DATA(0),
/// HOLE(1)}` MUST yield `NFS4ERR_UNION_NOTSUPP` (10090). Today's
/// dispatcher emits NFS4ERR_NOTSUPP via the catch-all because SEEK
/// isn't implemented at all — fidelity gap. RED until SEEK lands
/// with full discriminant validation.
#[test]
fn s15_5_seek_invalid_sa_what_returns_union_notsupp() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_SEEK);
        w.write_opaque_fixed(&[0u8; 16]);
        w.write_u64(0);
        w.write_u32(99); // not SEEK4_DATA(0) or SEEK4_HOLE(1)
    });
    let reply = drive_compound(0x3001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    let _ = r.read_u32(); // op
    let op_status = r.read_u32().unwrap();
    assert_eq!(
        op_status, NFS4ERR_UNION_NOTSUPP,
        "RFC 7862 §15.5 + §11.11: SEEK with invalid sa_what MUST yield \
         NFS4ERR_UNION_NOTSUPP (10090); production currently emits NFS4ERR_NOTSUPP \
         via the catch-all because SEEK isn't a dispatched op"
    );
}

/// RFC 7862 §15.5 + §11.6 — `LAYOUTERROR4args` carries an
/// `lerr_iomode4` field; a value outside the v4.1 IOMODE set
/// (`{LAYOUTIOMODE4_READ(1), LAYOUTIOMODE4_RW(2), LAYOUTIOMODE4_ANY(3)}`)
/// MUST yield `NFS4ERR_BADIOMODE` (10049). Today's dispatcher emits
/// NFS4ERR_NOTSUPP via the catch-all. RED.
#[test]
fn s15_5_layouterror_invalid_iomode_returns_badiomode() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_LAYOUTERROR);
        w.write_u64(0); // lea_offset
        w.write_u64(0); // lea_length
        w.write_opaque_fixed(&[0u8; 16]); // lea_stateid
        w.write_u32(0); // lea_errors<> count = 0
                        // The decoder would reach lerr_iomode inside each error;
                        // we provide a bogus value out-of-band to exercise the
                        // BADIOMODE path.
        w.write_u32(99); // bogus iomode
    });
    let reply = drive_compound(0x3101, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    let _ = r.read_u32(); // op
    let op_status = r.read_u32().unwrap();
    assert_eq!(
        op_status, NFS4ERR_BADIOMODE,
        "RFC 7862 §15.5 + §11.6: LAYOUTERROR with invalid iomode MUST yield \
         NFS4ERR_BADIOMODE (10049); production currently emits NFS4ERR_NOTSUPP"
    );
}

// ===========================================================================
// §15.5 + RFC 8881 §13.1 — v4.2 op on minor_version=1 path
// ===========================================================================

/// RFC 7862 §1 + RFC 8881 §13.1 — when a client encodes a v4.2 op
/// (e.g. ALLOCATE) inside a `minor_version=1` COMPOUND, the server
/// MUST reject either the whole COMPOUND with
/// `NFS4ERR_MINOR_VERS_MISMATCH` or the specific op with
/// `NFS4ERR_NOTSUPP`. Today's dispatcher ignores `minor_version` and
/// runs ALLOCATE through the catch-all (returning NFS4ERR_NOTSUPP) —
/// the per-op result happens to be spec-compliant.
#[test]
fn s15_5_v4_2_op_in_minor_v1_compound_returns_notsupp_or_minor_mismatch() {
    // Encode COMPOUND with minor_version=1 (NFSv4.1) but include a
    // v4.2-only op (ALLOCATE).
    let mut w = XdrWriter::new();
    w.write_opaque(b"");
    w.write_u32(1); // minor_version=1 (NOT 2)
    w.write_u32(1); // 1 op
    w.write_u32(OP_ALLOCATE);
    w.write_opaque_fixed(&[0u8; 16]);
    w.write_u64(0);
    w.write_u64(0);
    let body = w.into_bytes();

    let reply = drive_compound(0x4001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    const NFS4ERR_MINOR_VERS_MISMATCH: u32 = 10021;
    assert!(
        compound_status == NFS4ERR_NOTSUPP || compound_status == NFS4ERR_MINOR_VERS_MISMATCH,
        "RFC 7862 §1 + RFC 8881 §13.1: v4.2 op in v4.1 COMPOUND MUST yield \
         NFS4ERR_NOTSUPP (10004) or NFS4ERR_MINOR_VERS_MISMATCH (10021); \
         got {compound_status}"
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 7862 §15 ALLOCATE compound
// ===========================================================================

/// RFC 7862 §15 (XDR grammar, op 59) — verbatim ALLOCATE4args
/// encoding for a v4.2 COMPOUND. The wire shape per the RFC's
/// `union nfs_argop4 switch (nfs_opnum4 argop)` discriminator is:
///
/// ```text
///   COMPOUND4args {
///     tag         = ""
///     minorversion = 2
///     argarray<> = [
///       ALLOCATE {
///         stateid4 aa_stateid       = { 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0 };
///         offset4  aa_offset        = 0;
///         length4  aa_length        = 0x0000_0000_0000_1000; /* 4096 */
///       }
///     ]
///   }
/// ```
///
/// Source: RFC 7862 §11.1 (ALLOCATE) + §15 (XDR grammar). This is
/// the smallest spec-conformant ALLOCATE call. The seed bytes below
/// are the byte-for-byte XDR encoding of that COMPOUND body.
#[test]
fn rfc_7862_seed_allocate_compound_byte_shape() {
    let body = encode_compound(b"", 1, |w| {
        w.write_u32(OP_ALLOCATE);
        w.write_opaque_fixed(&[0u8; 16]);
        w.write_u64(0); // aa_offset
        w.write_u64(4096); // aa_length
    });

    let expected: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x00, // tag length = 0
        0x00, 0x00, 0x00, 0x02, // minorversion = 2 (NFSv4.2)
        0x00, 0x00, 0x00, 0x01, // argarray length = 1
        0x00, 0x00, 0x00, 0x3B, // op = 59 (ALLOCATE)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // aa_stateid[0..8]
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // aa_stateid[8..16]
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // aa_offset = 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, // aa_length = 4096
    ];
    assert_eq!(
        body, expected,
        "RFC 7862 §15: ALLOCATE COMPOUND wire encoding pinned byte-for-byte"
    );

    // Drive the seed through the dispatcher and assert it returns
    // either NFS4_OK (when ALLOCATE is implemented) or NFS4ERR_NOTSUPP
    // (today's catch-all). Either is RFC-compliant; the test fails
    // only if we get a different error.
    let reply = drive_compound(0xCAFE_BABE, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert!(
        compound_status == nfs4_status::NFS4_OK || compound_status == NFS4ERR_NOTSUPP,
        "RFC 7862 §15 seed: ALLOCATE COMPOUND MUST yield NFS4_OK or \
         NFS4ERR_NOTSUPP; got {compound_status}"
    );
}
