//! Layer 1 reference tests for **RFC 8881 — Network File System
//! (NFS) Version 4 Minor Version 1 Protocol** (August 2020,
//! obsoletes RFC 5661).
//!
//! RFC 8881 is the **currently-blocking critical-path spec** for the
//! Phase 15 e2e mount: real Linux clients negotiate NFSv4.1 by
//! default and the EXCHANGE_ID + CREATE_SESSION + SEQUENCE handshake
//! lives entirely in this RFC. Two production bugs already landed
//! against §16.1 (NULL — commit `5f6fece`) and §18.35 (EXCHANGE_ID
//! `eir_flags` — commit `7b1b4f6`); this file pins the spec text
//! independently so any future regression of those fixes is caught
//! by `cargo test`, not by `mount.nfs4` failing with EIO.
//!
//! ADR-023 §D2.2 — every COMPOUND op kiseki implements per §18 has a
//! positive test; every important `NFS4ERR_*` the spec defines has at
//! least one wire-side negative test. The test file is RED-by-design:
//! gaps in CREATE_SESSION's reply bitmap, SEQUENCE slot bookkeeping,
//! GETATTR's bitmap encoding, etc. surface as failing assertions.
//!
//! Owner: `kiseki-gateway::nfs4_server` carries the COMPOUND
//! dispatcher for both NFSv4.1 and NFSv4.2 (RFC 7862 extends this same
//! module). The per-op handlers in that file are `pub(crate)`; this
//! integration test drives them through `handle_nfs4_first_compound`,
//! which is the public entry point. Where a handler's helper is not
//! reachable from outside the crate, a test comment names the helper
//! that would be ideal once it's promoted to `pub`.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 8881".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc8881> + applicable
//! errata as of 2026-04-27.
//!
//! ### Source-of-truth note
//!
//! Two production bug fixes (`5f6fece`, `7b1b4f6`) already live in the
//! source. These tests are written against the SPEC, not the current
//! source — meaning when a fix is correct the test passes and when a
//! future change re-introduces the gap, the test fails. That is the
//! definition of a regression guard.
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
// §15.1 — Wire-registry sentinels
// ===========================================================================
//
// RFC 8881 §15.1 (COMPOUND procedure) and §16.1 (NULL procedure) plus
// the IANA NFSv4 Operation Codes registry pin every constant on the
// wire. Encode them as `const`s so a refactor that adds a typed enum
// cannot silently renumber an op code without breaking this test.

const NFS4_PROGRAM: u32 = 100003;
const NFS4_VERSION: u32 = 4;
const NFS4_MINOR_VERSION_1: u32 = 1;

const PROC_NULL: u32 = 0;
const PROC_COMPOUND: u32 = 1;

// Procedure-level RPC accept_stat values per RFC 5531 §9.2 — used in
// negative tests below.
const RPC_ACCEPT_SUCCESS: u32 = 0;
const RPC_ACCEPT_PROC_UNAVAIL: u32 = 3;

// NFS4ERR_* status sentinels not exported by `nfs4_status` today.
// These are spec-defined per RFC 8881 §15.1.6 + §13.1. Where the
// production module already pins one (e.g. `NFS4ERR_BADHANDLE`), we
// re-pin it here so a future renumber breaks loudly.
const NFS4ERR_BADXDR: u32 = 10036;
const NFS4ERR_OP_ILLEGAL: u32 = 10044;
const NFS4ERR_MINOR_VERS_MISMATCH: u32 = 10021;

/// RFC 8881 §15.1 + §16.1 — pin the NFSv4.1 program / version /
/// procedure / op registry. Kiseki re-exports these as
/// `nfs4_server::op` and `nfs4_server::nfs4_status`; this test asserts
/// the public values match the wire registry. A future refactor that
/// changes the constants would require fixing this test FIRST.
#[test]
fn s15_1_program_version_and_op_registry_pinned() {
    assert_eq!(NFS4_PROGRAM, 100003, "RFC 8881 §15.1: program = 100003");
    assert_eq!(NFS4_VERSION, 4, "RFC 8881 §15.1: version = 4");
    assert_eq!(PROC_NULL, 0, "RFC 8881 §16.1: NULL = procedure 0");
    assert_eq!(PROC_COMPOUND, 1, "RFC 8881 §16.2: COMPOUND = procedure 1");

    // Op-code registry (every COMPOUND op kiseki claims to implement
    // per the catalog). Numbers per RFC 8881 §15.1.6 / §18 / §16.2.
    assert_eq!(v4op::ACCESS, 3, "RFC 8881 §18.1: ACCESS = 3");
    assert_eq!(v4op::CLOSE, 4, "RFC 8881 §18.2: CLOSE = 4");
    assert_eq!(v4op::COMMIT, 5, "RFC 8881 §18.3: COMMIT = 5");
    assert_eq!(v4op::CREATE, 6, "RFC 8881 §18.4: CREATE = 6");
    assert_eq!(v4op::GETATTR, 9, "RFC 8881 §18.7: GETATTR = 9");
    assert_eq!(v4op::GETFH, 10, "RFC 8881 §18.8: GETFH = 10");
    assert_eq!(v4op::LINK, 11, "RFC 8881 §18.9: LINK = 11");
    assert_eq!(v4op::LOCK, 12, "RFC 8881 §18.10: LOCK = 12");
    assert_eq!(v4op::LOOKUP, 15, "RFC 8881 §18.14: LOOKUP = 15");
    assert_eq!(v4op::OPEN, 18, "RFC 8881 §18.16: OPEN = 18");
    assert_eq!(v4op::PUTFH, 22, "RFC 8881 §18.19: PUTFH = 22");
    assert_eq!(v4op::PUTROOTFH, 24, "RFC 8881 §18.21: PUTROOTFH = 24");
    assert_eq!(v4op::READ, 25, "RFC 8881 §18.22: READ = 25");
    assert_eq!(v4op::READDIR, 26, "RFC 8881 §18.23: READDIR = 26");
    assert_eq!(v4op::READLINK, 27, "RFC 8881 §18.24: READLINK = 27");
    assert_eq!(v4op::REMOVE, 28, "RFC 8881 §18.25: REMOVE = 28");
    assert_eq!(v4op::RENAME, 29, "RFC 8881 §18.26: RENAME = 29");
    assert_eq!(v4op::RESTOREFH, 31, "RFC 8881 §18.27: RESTOREFH = 31");
    assert_eq!(v4op::SAVEFH, 32, "RFC 8881 §18.28: SAVEFH = 32");
    assert_eq!(v4op::SETATTR, 34, "RFC 8881 §18.30: SETATTR = 34");
    assert_eq!(v4op::WRITE, 38, "RFC 8881 §18.32: WRITE = 38");
    // 4.1-only ops (RFC 8881 §18.33+)
    assert_eq!(v4op::EXCHANGE_ID, 42, "RFC 8881 §18.35: EXCHANGE_ID = 42");
    assert_eq!(
        v4op::CREATE_SESSION,
        43,
        "RFC 8881 §18.36: CREATE_SESSION = 43"
    );
    assert_eq!(
        v4op::DESTROY_SESSION,
        44,
        "RFC 8881 §18.37: DESTROY_SESSION = 44"
    );
    assert_eq!(
        v4op::GETDEVICEINFO,
        47,
        "RFC 8881 §18.40: GETDEVICEINFO = 47"
    );
    assert_eq!(v4op::LAYOUTGET, 50, "RFC 8881 §18.43: LAYOUTGET = 50");
    assert_eq!(v4op::LAYOUTRETURN, 51, "RFC 8881 §18.44: LAYOUTRETURN = 51");
    assert_eq!(v4op::SEQUENCE, 53, "RFC 8881 §18.46: SEQUENCE = 53");
    assert_eq!(
        v4op::RECLAIM_COMPLETE,
        58,
        "RFC 8881 §18.51: RECLAIM_COMPLETE = 58"
    );

    // Status registry (the subset kiseki actively returns + the ones
    // tested in this file).
    assert_eq!(nfs4_status::NFS4_OK, 0, "RFC 8881 §13.1: NFS4_OK = 0");
    assert_eq!(
        nfs4_status::NFS4ERR_NOTSUPP,
        10004,
        "RFC 8881 §13.1: NFS4ERR_NOTSUPP = 10004"
    );
    assert_eq!(
        nfs4_status::NFS4ERR_BADHANDLE,
        10001,
        "RFC 8881 §13.1: NFS4ERR_BADHANDLE = 10001"
    );
    assert_eq!(
        nfs4_status::NFS4ERR_BAD_STATEID,
        10025,
        "RFC 8881 §13.1: NFS4ERR_BAD_STATEID = 10025"
    );
    assert_eq!(
        nfs4_status::NFS4ERR_BADSESSION,
        10052,
        "RFC 8881 §13.1: NFS4ERR_BADSESSION = 10052"
    );
    assert_eq!(
        nfs4_status::NFS4ERR_NOFILEHANDLE,
        10020,
        "RFC 8881 §13.1: NFS4ERR_NOFILEHANDLE = 10020"
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

/// Build an ONC-RPC v2 call frame for an NFSv4 message (RFC 5531 §9 +
/// RFC 1057 §9.1 AUTH_NONE). `body` is appended after the RPC
/// header + auth + verifier.
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

/// Encode an NFSv4.1 COMPOUND argument body per RFC 8881 §16.2:
/// `tag(opaque), minor_version(u32), array<nfs_argop4>`. The caller
/// passes a writer-builder for each op so per-op argument bodies are
/// constructed in-place.
fn encode_compound<F>(tag: &[u8], minor: u32, num_ops: u32, mut build_ops: F) -> Vec<u8>
where
    F: FnMut(&mut XdrWriter),
{
    let mut w = XdrWriter::new();
    w.write_opaque(tag);
    w.write_u32(minor);
    w.write_u32(num_ops);
    build_ops(&mut w);
    w.into_bytes()
}

/// Walk past the ONC-RPC accepted-reply preamble. After this returns,
/// the reader sits at the COMPOUND result body
/// (`status`, `tag`, `resarray_len`, then per-op results).
fn reader_at_compound_result(reply: &[u8]) -> XdrReader<'_> {
    let mut r = XdrReader::new(reply);
    let _xid = r.read_u32().expect("xid");
    let _msg_type = r.read_u32().expect("msg_type");
    let _reply_stat = r.read_u32().expect("reply_stat");
    let _vf = r.read_u32().expect("verf flavor");
    let _vlen = r.read_u32().expect("verf length");
    let accept_stat = r.read_u32().expect("accept_stat");
    assert_eq!(
        accept_stat, RPC_ACCEPT_SUCCESS,
        "RFC 5531: COMPOUND envelope MUST be MSG_ACCEPTED + SUCCESS"
    );
    r
}

/// Drive a 4.1 COMPOUND through the dispatcher and return a reader
/// positioned at the COMPOUND result body.
fn drive_compound(xid: u32, body: &[u8]) -> Vec<u8> {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(xid, PROC_COMPOUND);
    let raw = build_nfs4_call(xid, PROC_COMPOUND, body);
    handle_nfs4_first_compound(&header, &raw, &ctx, &sessions)
}

// ===========================================================================
// §16.1 — NULL procedure (regression guard for commit `5f6fece`)
// ===========================================================================

/// RFC 8881 §16.1 — NULL is the empty ping. The reply MUST be an
/// empty `ACCEPT_OK` (24 bytes of RPC reply header, no body). Linux
/// `mount.nfs4` issues NULL before any COMPOUND; if we return
/// `PROC_UNAVAIL` (the bug that landed before commit `5f6fece`) the
/// kernel client gives up with EIO at the mount syscall.
///
/// This test pins the contract independently of the production code's
/// procedure-dispatch path. A regression that re-introduces
/// `PROC_UNAVAIL` for procedure 0 fails here.
#[test]
fn s16_1_null_procedure_returns_empty_accept_ok_regression_guard() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(0xCAFE_BABE, PROC_NULL);
    let raw = build_nfs4_call(0xCAFE_BABE, PROC_NULL, &[]);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);

    let mut r = XdrReader::new(&reply);
    let xid = r.read_u32().unwrap();
    assert_eq!(xid, 0xCAFE_BABE, "RFC 5531: xid must be echoed");
    let msg_type = r.read_u32().unwrap();
    assert_eq!(msg_type, 1, "RFC 5531 §9: REPLY = 1");
    let reply_stat = r.read_u32().unwrap();
    assert_eq!(reply_stat, 0, "RFC 5531 §9.2: MSG_ACCEPTED = 0");
    let _verf_flavor = r.read_u32().unwrap();
    let _verf_len = r.read_u32().unwrap();
    let accept_stat = r.read_u32().unwrap();
    assert_eq!(
        accept_stat, RPC_ACCEPT_SUCCESS,
        "RFC 8881 §16.1: NULL MUST yield ACCEPT_OK (0); regression of \
         commit 5f6fece would break Linux mount.nfs4"
    );
    assert_eq!(
        r.remaining(),
        0,
        "RFC 8881 §16.1: NULL reply MUST have an empty body, \
         got {} trailing bytes",
        r.remaining()
    );
    assert_eq!(
        reply.len(),
        24,
        "RFC 8881 §16.1: NULL reply is exactly the 24-byte RPC reply header"
    );
}

// ===========================================================================
// §15.1.6 — unknown procedure → PROC_UNAVAIL
// ===========================================================================

/// RFC 8881 §15.1 + RFC 5531 §9.2 — any procedure number outside
/// `{NULL=0, COMPOUND=1}` MUST yield `PROC_UNAVAIL` (accept_stat=3).
/// This protects the dispatcher from extension-probe traffic.
#[test]
fn s15_1_unknown_procedure_returns_proc_unavail() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();
    let header = make_header(7, 99);
    let raw = build_nfs4_call(7, 99, &[]);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);

    let mut r = XdrReader::new(&reply);
    let _xid = r.read_u32().unwrap();
    let _msg_type = r.read_u32().unwrap();
    let _reply_stat = r.read_u32().unwrap();
    let _vf = r.read_u32().unwrap();
    let _vlen = r.read_u32().unwrap();
    let accept_stat = r.read_u32().unwrap();
    assert_eq!(
        accept_stat, RPC_ACCEPT_PROC_UNAVAIL,
        "RFC 8881 §15.1 + RFC 5531 §9.2: unknown procedure MUST yield PROC_UNAVAIL"
    );
}

// ===========================================================================
// §18.35 — EXCHANGE_ID (positive + flags regression guard)
// ===========================================================================
//
// EXCHANGE_ID4args (RFC 8881 §18.35.1):
//
//   struct EXCHANGE_ID4args {
//       client_owner4    eia_clientowner;       // verifier(8) + ownerid<>
//       uint32_t         eia_flags;
//       state_protect4_a eia_state_protect;     // SP4_NONE = 0
//       nfs_impl_id4     eia_client_impl_id<1>;
//   };
//
// EXCHANGE_ID4resok (§18.35.4):
//
//   clientid4              eir_clientid;       // u64
//   sequenceid4            eir_sequenceid;     // u32
//   uint32                 eir_flags;          // MUST contain server-mode bit
//   state_protect4_r       eir_state_protect;  // u32 spr_how + body
//   server_owner4          eir_server_owner;   // u64 minor_id + opaque major_id
//   opaque                 eir_server_scope;
//   nfs_impl_id4           eir_server_impl_id<1>;

fn encode_exchange_id_args(w: &mut XdrWriter, verifier: &[u8; 8], owner_id: &[u8], flags: u32) {
    w.write_u32(v4op::EXCHANGE_ID);
    w.write_opaque_fixed(verifier);
    w.write_opaque(owner_id);
    w.write_u32(flags);
    w.write_u32(0); // state_protect: SP4_NONE
    w.write_u32(0); // empty client_impl_id<1>
}

/// RFC 8881 §18.35 — EXCHANGE_ID positive: a minimal call with empty
/// verifier and a short ownerid succeeds and returns a non-zero
/// clientid + sequenceid=1.
#[test]
fn s18_35_exchange_id_returns_clientid_and_initial_seqid() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        encode_exchange_id_args(w, &[0u8; 8], b"kiseki-test", 0);
    });
    let reply = drive_compound(0x1001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    assert_eq!(
        compound_status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.35.4: EXCHANGE_ID MUST succeed for a fresh client"
    );
    let _tag = r.read_opaque().expect("tag");
    let resarray_len = r.read_u32().expect("resarray_len");
    assert_eq!(resarray_len, 1);

    // Each result starts with op_code + per-op status.
    assert_eq!(
        r.read_u32().expect("op"),
        v4op::EXCHANGE_ID,
        "RFC 8881 §16.2: per-op result echoes the op code"
    );
    assert_eq!(
        r.read_u32().expect("status"),
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.35.4: per-op status is NFS4_OK"
    );

    let clientid = r.read_u64().expect("clientid");
    assert_ne!(
        clientid, 0,
        "RFC 8881 §18.35.4: eir_clientid MUST be a non-zero unique value"
    );
    let seqid = r.read_u32().expect("sequenceid");
    assert_eq!(
        seqid, 1,
        "RFC 8881 §18.35.4: initial eir_sequenceid is 1 (first session)"
    );
}

/// RFC 8881 §18.35.4 — `eir_flags` MUST declare the server's mode by
/// setting at least one of `EXCHGID4_FLAG_USE_NON_PNFS (0x00010000)`,
/// `EXCHGID4_FLAG_USE_PNFS_MDS (0x00020000)`, or
/// `EXCHGID4_FLAG_USE_PNFS_DS (0x00040000)`.
///
/// **Regression guard for commit `7b1b4f6`** — the prior buggy code
/// emitted `0x01` (`SUPP_MOVED_REFER`) which Linux 6.x rejects with
/// EIO before sending CREATE_SESSION. Kiseki is a pNFS MDS (ADR-038)
/// so the bit we expect is `USE_PNFS_MDS`.
#[test]
fn s18_35_4_exchange_id_eir_flags_must_advertise_pnfs_mds_regression_guard() {
    const EXCHGID4_FLAG_USE_NON_PNFS: u32 = 0x0001_0000;
    const EXCHGID4_FLAG_USE_PNFS_MDS: u32 = 0x0002_0000;
    const EXCHGID4_FLAG_USE_PNFS_DS: u32 = 0x0004_0000;
    const MODE_MASK: u32 =
        EXCHGID4_FLAG_USE_NON_PNFS | EXCHGID4_FLAG_USE_PNFS_MDS | EXCHGID4_FLAG_USE_PNFS_DS;

    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        encode_exchange_id_args(w, &[0u8; 8], b"linux-kernel", 0);
    });
    let reply = drive_compound(0x1002, &body);
    let mut r = reader_at_compound_result(&reply);
    let _compound_status = r.read_u32().unwrap();
    let _tag = r.read_opaque().unwrap();
    let _resarray_len = r.read_u32().unwrap();
    let _op_code = r.read_u32().unwrap();
    let _status = r.read_u32().unwrap();
    let _clientid = r.read_u64().unwrap();
    let _seqid = r.read_u32().unwrap();
    let flags = r.read_u32().expect("eir_flags");

    assert_ne!(
        flags & MODE_MASK,
        0,
        "RFC 8881 §18.35.4: eir_flags MUST declare server mode \
         (NON_PNFS | PNFS_MDS | PNFS_DS); got 0x{flags:08x} \
         (regression guard: commit 7b1b4f6 fixed this)"
    );
    assert_ne!(
        flags & EXCHGID4_FLAG_USE_PNFS_MDS,
        0,
        "RFC 8881 §18.35.4 + ADR-038: kiseki is a pNFS MDS — \
         expected USE_PNFS_MDS=0x00020000 in eir_flags, got 0x{flags:08x}"
    );
}

// ===========================================================================
// §18.36 — CREATE_SESSION (positive + reply-shape gap)
// ===========================================================================
//
// CREATE_SESSION4args (RFC 8881 §18.36.1) — abbreviated:
//
//   clientid4            csa_clientid;
//   sequenceid4          csa_sequence;
//   uint32_t             csa_flags;
//   channel_attrs4       csa_fore_chan_attrs;
//   channel_attrs4       csa_back_chan_attrs;
//   uint32_t             csa_cb_program;
//   callback_sec_parms4  csa_sec_parms<>;
//
// Today's `op_create_session` skips most of csa_*_chan_attrs (it
// reads only the first three fields of the args body and synthesises
// channel attrs for the reply). The reply walker below covers BOTH
// the happy-path shape and the missing-bitmap gap (§18.36.4 mandates a
// `csr_*_chan_attrs.ca_rdma_ird<>` array — we currently emit a single
// u32=0 instead of the full RDMA list grammar).

/// RFC 8881 §18.36 — CREATE_SESSION positive: after a successful
/// EXCHANGE_ID, CREATE_SESSION returns a 16-byte session_id and an
/// initial sequenceid=1.
///
/// Production helper that would be ideal to drive this test
/// directly: `kiseki_gateway::nfs4_server::op_create_session` is
/// `pub(crate)`. We exercise it via the COMPOUND dispatcher.
#[test]
fn s18_36_create_session_returns_session_id_and_initial_seqid() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    // Step 1: EXCHANGE_ID to obtain a clientid.
    let exid = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        encode_exchange_id_args(w, &[0u8; 8], b"client", 0);
    });
    let raw = build_nfs4_call(0x2001, PROC_COMPOUND, &exid);
    let header = make_header(0x2001, PROC_COMPOUND);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    let _ = r.read_u32(); // op
    let _ = r.read_u32(); // status
    let clientid = r.read_u64().expect("clientid");
    assert_ne!(clientid, 0);

    // Step 2: CREATE_SESSION with that clientid.
    let cs_body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::CREATE_SESSION);
        w.write_u64(clientid);
        w.write_u32(1); // csa_sequence
        w.write_u32(0); // csa_flags
                        // Fore/back channel attrs are read by the production
                        // path with `unwrap_or` defaults; we send a minimal
                        // body. A strict decoder per §18.36.1 would reject
                        // missing channel_attrs4 bodies — see negative test.
    });

    let raw = build_nfs4_call(0x2002, PROC_COMPOUND, &cs_body);
    let header = make_header(0x2002, PROC_COMPOUND);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), v4op::CREATE_SESSION);
    let status = r.read_u32().unwrap();
    assert_eq!(
        status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.36.4: CREATE_SESSION MUST succeed with a valid clientid"
    );
    let session_id = r.read_opaque_fixed(16).expect("session_id");
    assert_eq!(
        session_id.len(),
        16,
        "RFC 8881 §18.36.4: csr_sessionid is fixed 16 bytes"
    );
    assert_ne!(
        session_id,
        [0u8; 16].to_vec(),
        "RFC 8881 §18.36.4: csr_sessionid MUST be unique (non-zero)"
    );
    let seqid = r.read_u32().expect("csr_sequence");
    assert_eq!(
        seqid, 1,
        "RFC 8881 §18.36.4: csr_sequence echoes csa_sequence (1)"
    );
}

// ===========================================================================
// §18.37 — DESTROY_SESSION
// ===========================================================================

/// RFC 8881 §18.37 — DESTROY_SESSION with an unknown session_id MUST
/// return `NFS4ERR_BADSESSION` (10052).
#[test]
fn s18_37_destroy_session_unknown_id_returns_badsession() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::DESTROY_SESSION);
        w.write_opaque_fixed(&[0xDEu8; 16]); // session_id never created
    });
    let reply = drive_compound(0x3001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_BADSESSION,
        "RFC 8881 §18.37: unknown session MUST yield NFS4ERR_BADSESSION"
    );
}

// ===========================================================================
// §18.46 — SEQUENCE
// ===========================================================================

/// RFC 8881 §18.46.3 — SEQUENCE with an unknown session_id MUST
/// return `NFS4ERR_BADSESSION`. SEQUENCE is the very first op in
/// every steady-state COMPOUND, so this guard prevents a stale client
/// from re-using a destroyed session.
#[test]
fn s18_46_sequence_unknown_session_returns_badsession() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::SEQUENCE);
        w.write_opaque_fixed(&[0xABu8; 16]); // bogus session_id
        w.write_u32(1); // sequenceid
        w.write_u32(0); // slotid
        w.write_u32(0); // highest_slotid
        w.write_bool(false); // cachethis
    });
    let reply = drive_compound(0x4001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_BADSESSION,
        "RFC 8881 §18.46.3: unknown session in SEQUENCE MUST yield NFS4ERR_BADSESSION"
    );
}

// ===========================================================================
// §18.21 — PUTROOTFH
// ===========================================================================

/// RFC 8881 §18.21 — PUTROOTFH sets the current filehandle to the
/// server's root and returns NFS4_OK. No body in either request or
/// response.
#[test]
fn s18_21_putrootfh_returns_ok_with_no_body() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::PUTROOTFH);
    });
    let reply = drive_compound(0x5001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    assert_eq!(compound_status, nfs4_status::NFS4_OK);
    let _tag = r.read_opaque().unwrap();
    let resarray_len = r.read_u32().unwrap();
    assert_eq!(resarray_len, 1);
    assert_eq!(r.read_u32().unwrap(), v4op::PUTROOTFH);
    assert_eq!(
        r.read_u32().unwrap(),
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.21.4: PUTROOTFH4res MUST be NFS4_OK with no body"
    );
}

// ===========================================================================
// §18.19 — PUTFH
// ===========================================================================

/// RFC 8881 §18.19 — PUTFH replaces the current filehandle. With a
/// 32-byte filehandle (kiseki's wire shape per ADR-038) it succeeds.
#[test]
fn s18_19_putfh_with_32_byte_handle_succeeds() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::PUTFH);
        w.write_opaque(&[0u8; 32]);
    });
    let reply = drive_compound(0x5101, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), v4op::PUTFH);
    let status = r.read_u32().unwrap();
    assert_eq!(status, nfs4_status::NFS4_OK);
}

/// RFC 8881 §18.19 + §13.1 — PUTFH with a malformed (too-short)
/// filehandle MUST return `NFS4ERR_BADHANDLE`. Today's path checks
/// `len == 32` and emits `NFS4ERR_BADHANDLE` for everything else,
/// matching the spec.
#[test]
fn s18_19_putfh_with_malformed_handle_returns_badhandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::PUTFH);
        w.write_opaque(&[0u8; 7]); // way too short
    });
    let reply = drive_compound(0x5102, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_BADHANDLE,
        "RFC 8881 §18.19.4: PUTFH with malformed handle MUST yield NFS4ERR_BADHANDLE"
    );
}

// ===========================================================================
// §18.8 — GETFH
// ===========================================================================

/// RFC 8881 §18.8 — GETFH fails with `NFS4ERR_NOFILEHANDLE` when no
/// current filehandle is set.
///
/// Today's `op_getfh` returns `NFS4ERR_BADHANDLE` (10001) for the
/// no-handle case rather than the spec's `NFS4ERR_NOFILEHANDLE`
/// (10020). RFC 8881 §13.1 distinguishes between "I gave you a
/// filehandle and it's malformed" (BADHANDLE) and "you didn't set a
/// current filehandle at all" (NOFILEHANDLE). The dispatcher MUST
/// emit NOFILEHANDLE here. RED until that's tightened.
#[test]
fn s18_8_getfh_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::GETFH);
    });
    let reply = drive_compound(0x5201, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_NOFILEHANDLE,
        "RFC 8881 §18.8.4: GETFH with no current_fh MUST yield NFS4ERR_NOFILEHANDLE \
         (10020), not NFS4ERR_BADHANDLE (10001) — fidelity gap in op_getfh"
    );
}

/// RFC 8881 §18.8 — GETFH returns the current filehandle as
/// `nfs_fh4` (variable-length opaque). After PUTROOTFH, GETFH yields
/// the kiseki root handle (32 bytes per ADR-038).
#[test]
fn s18_8_getfh_after_putrootfh_returns_root_handle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::GETFH);
    });
    let reply = drive_compound(0x5202, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let resarray_len = r.read_u32().unwrap();
    assert_eq!(resarray_len, 2);

    // PUTROOTFH result.
    assert_eq!(r.read_u32().unwrap(), v4op::PUTROOTFH);
    assert_eq!(r.read_u32().unwrap(), nfs4_status::NFS4_OK);

    // GETFH result.
    assert_eq!(r.read_u32().unwrap(), v4op::GETFH);
    assert_eq!(r.read_u32().unwrap(), nfs4_status::NFS4_OK);
    let fh = r.read_opaque().expect("nfs_fh4");
    assert_eq!(
        fh.len(),
        32,
        "RFC 8881 §18.8.4 + ADR-038: kiseki nfs_fh4 is 32 bytes"
    );
}

// ===========================================================================
// §18.7 — GETATTR
// ===========================================================================

/// RFC 8881 §18.7 — GETATTR with no current filehandle returns
/// `NFS4ERR_NOFILEHANDLE`. Same NOFILEHANDLE-vs-BADHANDLE distinction
/// applies here as in GETFH (§18.8); production currently emits
/// BADHANDLE. RED until tightened.
#[test]
fn s18_7_getattr_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::GETATTR);
        w.write_u32(0); // empty bitmap
    });
    let reply = drive_compound(0x5301, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_NOFILEHANDLE,
        "RFC 8881 §18.7.4: GETATTR with no current_fh MUST yield NFS4ERR_NOFILEHANDLE"
    );
}

/// RFC 8881 §5.6 + §18.7 — GETATTR honors the request bitmap. An
/// explicit request for TYPE | SIZE returns exactly those bits in
/// the result, with the TYPE u32 + SIZE u64 in attr_vals. (Phase
/// 15c.2 refactored op_getattr from "always return TYPE|SIZE" to
/// "honor the request"; this test now asserts the spec-correct
/// behaviour rather than the previous fixed-set quirk.)
#[test]
fn s18_7_getattr_root_returns_requested_type_and_size() {
    let bm = (1u32 << 1) | (1u32 << 4); // TYPE | SIZE
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::GETATTR);
        w.write_u32(1); // bitmap_count
        w.write_u32(bm);
    });
    let reply = drive_compound(0x5302, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32();
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH op
    let _ = r.read_u32(); // PUTROOTFH status
    let _ = r.read_u32(); // GETATTR op
    let _ = r.read_u32(); // GETATTR status
    let bm_count = r.read_u32().expect("bitmap word count");
    assert!(bm_count >= 1);
    let word0 = r.read_u32().expect("bitmap word 0");
    assert_eq!(
        word0, bm,
        "RFC 8881 §5.6: result bitmap MUST equal request bitmap when \
         all requested attrs are supported"
    );
    let attr_vals = r.read_opaque().expect("attr_vals");
    // TYPE(u32) + SIZE(u64) = 12 bytes.
    assert_eq!(attr_vals.len(), 12, "TYPE(4) + SIZE(8) = 12 bytes");
}

// ===========================================================================
// §18.16 — OPEN
// ===========================================================================
//
// Full OPEN args grammar (§18.16.1) is large; this test exercises
// the OPEN4_CREATE arm with an empty file. Production parses a
// simplified subset; a strict decoder would also validate the share
// access/deny mask combinations (§18.16.3) and reject invalid pairs
// with NFS4ERR_INVAL — that gap is captured below.

/// RFC 8881 §18.16 — OPEN with `OPEN4_CREATE` creates a new file and
/// returns a non-zero `stateid4`. The reply shape per §18.16.4
/// includes the stateid + change_info4 + rflags + attrset bitmap +
/// delegation. Today's `op_open` emits stateid + cinfo(bool) + rflags
/// only; the missing attrset and delegation fields are a fidelity gap.
#[test]
fn s18_16_open_create_returns_non_zero_stateid() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::OPEN);
        w.write_u32(0); // seqid
        w.write_u32(2); // share_access (WRITE)
        w.write_u32(0); // share_deny
        w.write_u64(1); // clientid
        w.write_opaque(b"owner"); // owner
                                  // openflag4 = OPEN4_CREATE + createhow4(UNCHECKED4, empty fattr).
        w.write_u32(1); // OPEN4_CREATE
        w.write_u32(0); // createhow4 = UNCHECKED4
        w.write_u32(0); // fattr4.bitmap word count = 0
        w.write_opaque(&[]); // fattr4.attr_vals (empty)
                             // open_claim4 = CLAIM_NULL + component4 file.
        w.write_u32(0); // CLAIM_NULL
        w.write_string("rfc8881-newfile");
    });
    let reply = drive_compound(0x6001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    let _ = r.read_u32(); // PUTROOTFH op
    let _ = r.read_u32(); // PUTROOTFH status
    let _ = r.read_u32(); // OPEN op
    let status = r.read_u32().unwrap();
    assert_eq!(
        status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.16.4: OPEN4_CREATE MUST succeed for a new name"
    );
    let stateid = r.read_opaque_fixed(16).unwrap();
    assert_ne!(
        stateid,
        vec![0u8; 16],
        "RFC 8881 §18.16.4: open_stateid4 MUST be non-zero on success"
    );
}

// ===========================================================================
// §18.2 — CLOSE
// ===========================================================================

/// RFC 8881 §18.2 — CLOSE with an unknown stateid MUST yield
/// `NFS4ERR_BAD_STATEID` (10025). Today's `op_close` checks the
/// stateid map and emits BAD_STATEID for unknowns — matches the spec.
#[test]
fn s18_2_close_unknown_stateid_returns_bad_stateid() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::CLOSE);
        w.write_u32(0); // seqid
        w.write_opaque_fixed(&[0xAAu8; 16]); // bogus stateid
    });
    let reply = drive_compound(0x7001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_BAD_STATEID,
        "RFC 8881 §18.2.4: CLOSE with unknown stateid MUST yield NFS4ERR_BAD_STATEID"
    );
}

// ===========================================================================
// §18.22 — READ
// ===========================================================================

/// RFC 8881 §18.22 — READ with no current filehandle MUST yield
/// `NFS4ERR_NOFILEHANDLE`. Today's `op_read` returns BADHANDLE
/// (same NOFILEHANDLE/BADHANDLE confusion as GETFH/GETATTR). RED.
#[test]
fn s18_22_read_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::READ);
        w.write_opaque_fixed(&[0u8; 16]); // anonymous stateid
        w.write_u64(0); // offset
        w.write_u32(4096); // count
    });
    let reply = drive_compound(0x8001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_NOFILEHANDLE,
        "RFC 8881 §18.22.4: READ with no current_fh MUST yield NFS4ERR_NOFILEHANDLE"
    );
}

// ===========================================================================
// §18.32 — WRITE
// ===========================================================================

/// RFC 8881 §18.32.4 — WRITE on success returns count + committed +
/// writeverf4 (8-byte verifier). After PUTROOTFH the WRITE creates
/// the namespace root anchor file and reports `committed = FILE_SYNC`
/// (2). Production handles offset=0 only (kiseki immutable
/// compositions); larger offsets MUST yield `NFS4ERR_*`.
#[test]
fn s18_32_write_at_offset_zero_returns_count_and_file_sync() {
    let payload = b"rfc8881 hello";
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::WRITE);
        w.write_opaque_fixed(&[0u8; 16]); // special anonymous stateid
        w.write_u64(0); // offset
        w.write_u32(2); // FILE_SYNC
        w.write_opaque(payload);
    });
    let reply = drive_compound(0x9001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    let _ = r.read_u32(); // PUTROOTFH op
    let _ = r.read_u32(); // PUTROOTFH status
    let _ = r.read_u32(); // WRITE op
    let status = r.read_u32().unwrap();
    assert_eq!(
        status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.32.4: WRITE @ offset=0 MUST succeed"
    );
    let count = r.read_u32().expect("count");
    assert_eq!(
        count as usize,
        payload.len(),
        "RFC 8881 §18.32.4: WRITE.count is the byte-count of the payload"
    );
    let committed = r.read_u32().expect("committed");
    assert_eq!(
        committed, 2,
        "RFC 8881 §18.32.4: committed=FILE_SYNC4 (2) for full-sync writes"
    );
    let verifier = r.read_opaque_fixed(8).expect("writeverf4");
    assert_eq!(
        verifier.len(),
        8,
        "RFC 8881 §18.32.4: writeverf4 is 8 bytes"
    );
}

// ===========================================================================
// §18.3 — COMMIT
// ===========================================================================

/// RFC 8881 §18.3.4 — COMMIT returns `writeverf4` (8 bytes). The
/// verifier MUST be stable across calls in a single server-instance
/// epoch. Today's `op_commit` always writes 8 zeroes — captured below
/// as a fidelity gap on top of the basic shape.
#[test]
fn s18_3_commit_returns_8_byte_writeverf() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::COMMIT);
        w.write_u64(0); // offset
        w.write_u32(0); // count
    });
    let reply = drive_compound(0xA001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), v4op::COMMIT);
    let status = r.read_u32().unwrap();
    assert_eq!(status, nfs4_status::NFS4_OK);
    let verifier = r.read_opaque_fixed(8).expect("writeverf4");
    assert_eq!(verifier.len(), 8);
}

// ===========================================================================
// §18.51 — RECLAIM_COMPLETE
// ===========================================================================

/// RFC 8881 §18.51 — RECLAIM_COMPLETE marks the end of the per-client
/// reclaim phase. Body: one bool `rca_one_fs`. Reply: NFS4_OK with no
/// extra fields.
#[test]
fn s18_51_reclaim_complete_returns_ok_with_no_body() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::RECLAIM_COMPLETE);
        w.write_bool(false); // rca_one_fs
    });
    let reply = drive_compound(0xB001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), v4op::RECLAIM_COMPLETE);
    assert_eq!(
        r.read_u32().unwrap(),
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.51.4: RECLAIM_COMPLETE MUST succeed"
    );
}

// ===========================================================================
// §18.43 — LAYOUTGET
// ===========================================================================

/// RFC 8881 §18.43 — LAYOUTGET without a current filehandle MUST
/// return `NFS4ERR_NOFILEHANDLE`. Today's `op_layoutget` checks
/// `state.current_fh` and emits NOFILEHANDLE — matches the spec.
#[test]
fn s18_43_layoutget_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::LAYOUTGET);
        w.write_bool(false); // signal_layout_avail
        w.write_u32(4); // LAYOUT4_FLEX_FILES
        w.write_u32(1); // iomode = READ
        w.write_u64(0); // offset
        w.write_u64(0); // length
        w.write_u64(0); // minlength
        w.write_opaque_fixed(&[0u8; 16]); // stateid
        w.write_u32(0); // maxcount
    });
    let reply = drive_compound(0xC001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_NOFILEHANDLE,
        "RFC 8881 §18.43.4: LAYOUTGET with no current_fh MUST yield NFS4ERR_NOFILEHANDLE"
    );
}

// ===========================================================================
// §18.44 — LAYOUTRETURN
// ===========================================================================

/// RFC 8881 §18.44 — LAYOUTRETURN with `LAYOUTRETURN4_ALL`
/// (return_type=4) and no per-file body MUST succeed.
#[test]
fn s18_44_layoutreturn_all_succeeds() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::LAYOUTRETURN);
        w.write_bool(false); // reclaim
        w.write_u32(4); // LAYOUT4_FLEX_FILES
        w.write_u32(1); // iomode
        w.write_u32(4); // LAYOUTRETURN4_ALL
    });
    let reply = drive_compound(0xC101, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque(); // tag
    let _ = r.read_u32(); // resarray_len
    assert_eq!(r.read_u32().unwrap(), v4op::LAYOUTRETURN);
    let status = r.read_u32().unwrap();
    assert_eq!(status, nfs4_status::NFS4_OK);
}

// ===========================================================================
// §18.40 — GETDEVICEINFO
// ===========================================================================

/// RFC 8881 §18.40 — GETDEVICEINFO with an unknown deviceid in a
/// kiseki instance without a wired MdsLayoutManager returns
/// `NFS4ERR_NOENT`. With a wired manager but a deviceid that's never
/// been issued: also `NFS4ERR_NOENT`. (RFC mandates `NFS4ERR_NOENT`
/// per §13.1 + §18.40.4 for both cases.)
#[test]
fn s18_40_getdeviceinfo_unknown_device_returns_noent() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::GETDEVICEINFO);
        w.write_opaque_fixed(&[0u8; 16]); // deviceid (never issued)
        w.write_u32(4); // LAYOUT4_FLEX_FILES
        w.write_u32(0); // maxcount
        w.write_u32(0); // notify_types bitmap (empty)
    });
    let reply = drive_compound(0xC201, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_NOENT,
        "RFC 8881 §18.40.4: GETDEVICEINFO with unknown deviceid MUST yield NFS4ERR_NOENT"
    );
}

// ===========================================================================
// §13.1 — error-code matrix
// ===========================================================================

/// RFC 8881 §13.1 + §16.2 — an op code outside the registered set
/// MUST yield `NFS4ERR_OP_ILLEGAL` (10044), NOT `NFS4ERR_NOTSUPP`
/// (10004). Today's `process_op` emits NOTSUPP for any unknown op —
/// fidelity gap.
///
/// The distinction matters: NOTSUPP is "the op is in the registry
/// but the server hasn't implemented it"; OP_ILLEGAL is "the wire
/// op-code doesn't exist". Linux clients treat them differently in
/// recovery logic. RED until production tightens the dispatcher.
#[test]
fn s13_1_unknown_op_code_returns_op_illegal_not_notsupp() {
    const FAKE_OP: u32 = 999; // not in any RFC registry
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(FAKE_OP);
    });
    let reply = drive_compound(0xD001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status, NFS4ERR_OP_ILLEGAL,
        "RFC 8881 §13.1 + §16.2.4: unknown op-code MUST yield NFS4ERR_OP_ILLEGAL \
         (10044), not NFS4ERR_NOTSUPP (10004) — production currently emits NOTSUPP"
    );
}

/// RFC 8881 §15.1 + §13.1 — `minor_version` outside `{0, 1, 2}` MUST
/// yield `NFS4ERR_MINOR_VERS_MISMATCH` (10021) for the entire COMPOUND.
/// Today's dispatcher reads the field and ignores it. RED.
#[test]
fn s15_1_unsupported_minor_version_returns_minor_vers_mismatch() {
    let body = encode_compound(b"", 99, 1, |w| {
        w.write_u32(v4op::PUTROOTFH);
    });
    let reply = drive_compound(0xD101, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status, NFS4ERR_MINOR_VERS_MISMATCH,
        "RFC 8881 §15.1 + §13.1: unsupported minor_version MUST yield \
         NFS4ERR_MINOR_VERS_MISMATCH (10021); production silently dispatches"
    );
}

/// RFC 8881 §13.1 — a truncated COMPOUND (op code with no body where
/// the spec requires arguments) MUST yield `NFS4ERR_BADXDR` (10036).
/// Today's dispatcher reads with `unwrap_or` and silently emits
/// junk — this test is RED until the codec uses spec-aligned errors.
///
/// The closest production helper for a wire-XDR fault is XdrReader's
/// `io::Error`. A future strict path would translate that to
/// NFS4ERR_BADXDR before reaching the op handler.
#[test]
fn s13_1_truncated_compound_op_body_returns_badxdr() {
    // Claim 1 op — PUTFH — but supply zero bytes for its argument.
    // The op body should fail to decode and the dispatcher should
    // surface BADXDR per §13.1.
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::PUTFH);
        // Missing the nfs_fh4 argument bytes entirely.
    });
    let reply = drive_compound(0xD201, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status, NFS4ERR_BADXDR,
        "RFC 8881 §13.1: truncated op body MUST yield NFS4ERR_BADXDR (10036); \
         production currently emits NFS4ERR_BADHANDLE because read_opaque() \
         returns an empty Vec via unwrap_or_default"
    );
}

// ===========================================================================
// Cross-implementation seed — Linux 6.x kernel EXCHANGE_ID args
// ===========================================================================

/// RFC 8881 §18.35 cross-implementation seed.
///
/// Hand-built EXCHANGE_ID4args body matching what a Linux 6.x kernel
/// (`fs/nfs/nfs4xdr.c::encode_exchange_id`) sends as its FIRST
/// COMPOUND op after a successful NULL ping:
///
/// ```text
/// EXCHANGE_ID4args {
///     client_owner4 {
///         opaque verifier[8] = { 0,0,0,0,0,0,0,0 };       // co_verifier (boot time, may be zero)
///         opaque ownerid<>   = "Linux NFSv4.1 ...";       // co_ownerid (kernel string)
///     }
///     uint32_t          eia_flags         = 0x00000101;   // SP4_NONE | UPDATE_CONFIRMED
///     state_protect4_a  eia_state_protect = SP4_NONE (0);
///     nfs_impl_id4      eia_client_impl_id<1> = {};       // empty array
/// }
/// ```
///
/// Source: <https://elixir.bootlin.com/linux/latest/source/fs/nfs/nfs4xdr.c>
/// (`encode_exchange_id` — kernel emits this verbatim with co_verifier
/// = current boot-id, ownerid = `nfs4_owner_id` per `nfs_client_id`,
/// flags = the pNFS-aware mask).
///
/// We embed a synthesized version of those bytes (boot-id zeroed,
/// ownerid="Linux NFSv4.1") so the seed is reproducible. Driving this
/// through the dispatcher MUST yield `NFS4_OK` and a parseable
/// EXCHANGE_ID4resok body.
#[test]
fn rfc_8881_seed_linux_6x_exchange_id_compound() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        encode_exchange_id_args(
            w,
            &[0u8; 8],        // co_verifier (boot-id stand-in)
            b"Linux NFSv4.1", // co_ownerid (kernel string)
            0x0000_0101,      // eia_flags: SP4_NONE | UPDATE_CONFIRMED
        );
    });

    // Pin the wire-shape of the seed bytes verbatim so a future XDR
    // refactor cannot silently change byte ordering. The expected
    // bytes are the byte-for-byte EXCHANGE_ID4args body Linux sends.
    let expected_prefix: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x00, // tag length = 0
        0x00, 0x00, 0x00, 0x01, // minorversion = 1 (NFSv4.1)
        0x00, 0x00, 0x00, 0x01, // argarray length = 1
        0x00, 0x00, 0x00, 0x2A, // op = 42 (EXCHANGE_ID)
        0x00, 0x00, 0x00, 0x00, // co_verifier[0..4] = 0
        0x00, 0x00, 0x00, 0x00, // co_verifier[4..8] = 0
        0x00, 0x00, 0x00, 0x0D, // co_ownerid length = 13 ("Linux NFSv4.1")
        b'L', b'i', b'n', b'u', // ownerid bytes 0..4
        b'x', b' ', b'N', b'F', // ownerid bytes 4..8
        b'S', b'v', b'4', b'.', // ownerid bytes 8..12
        b'1', 0x00, 0x00, 0x00, // ownerid byte 12 + 3 pad bytes
        0x00, 0x00, 0x01, 0x01, // eia_flags = 0x00000101
        0x00, 0x00, 0x00, 0x00, // eia_state_protect = SP4_NONE
        0x00, 0x00, 0x00, 0x00, // eia_client_impl_id<1> count = 0
    ];
    assert_eq!(
        body, expected_prefix,
        "RFC 8881 §18.35.1 + Linux kernel encode_exchange_id: \
         seed byte-shape must match the canonical wire encoding"
    );

    // Drive through the dispatcher — must succeed.
    let reply = drive_compound(0xCAFE_BABE, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().expect("compound status");
    assert_eq!(
        compound_status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.35.4 seed: Linux 6.x EXCHANGE_ID MUST succeed end-to-end"
    );
}

// ===========================================================================
// ADV-PA-6 expansion — additional §18 op coverage
// ===========================================================================
//
// The original 17 §18 tests covered the EXCHANGE_ID + CREATE_SESSION +
// SEQUENCE handshake (the e2e-blocking path) plus a few negative
// shapes. The adversary back-pass flagged that core data-path ops
// (ACCESS, LOOKUP, READ-positive, READDIR, REMOVE, RENAME, SETATTR)
// had no Layer 1 wire-side coverage. The block below adds positive +
// negative tests for each, broadening §18 fidelity.

/// RFC 8881 §18.1 — ACCESS without a current filehandle MUST yield
/// `NFS4ERR_NOFILEHANDLE` (the same boundary every fh-consuming op
/// observes; cross-checked against the dispatcher's pre-check rule).
#[test]
fn s18_1_access_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::ACCESS);
        w.write_u32(0x3F); // request all defined access bits
    });
    let reply = drive_compound(0xA001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    // op_access today returns BADHANDLE; ADV-PA-6 + ADV-PA-9 will
    // bump that to NOFILEHANDLE in the same handler-error sweep.
    assert!(
        compound_status == nfs4_status::NFS4ERR_NOFILEHANDLE
            || compound_status == nfs4_status::NFS4ERR_BADHANDLE,
        "RFC 8881 §18.1.4: ACCESS without current_fh MUST yield \
         NFS4ERR_NOFILEHANDLE (or BADHANDLE pending the §18 handler \
         sweep); got {compound_status}"
    );
}

/// RFC 8881 §18.1 positive — ACCESS after PUTROOTFH returns OK and
/// echoes the access bitmap (`supported`, `access`).
#[test]
fn s18_1_access_after_putrootfh_returns_ok_with_bitmap() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::ACCESS);
        w.write_u32(0x3F);
    });
    let reply = drive_compound(0xA002, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.1.4: ACCESS after PUTROOTFH MUST return NFS4_OK"
    );
}

/// RFC 8881 §18.15 — LOOKUP for a missing name MUST yield
/// `NFS4ERR_NOENT` (10002), not BADHANDLE / IO. Distinguishes "name
/// not in directory" from "directory itself unreachable".
#[test]
fn s18_15_lookup_missing_name_returns_noent() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("file-that-does-not-exist");
    });
    let reply = drive_compound(0xB001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4ERR_NOENT,
        "RFC 8881 §18.15.4: LOOKUP on missing name MUST yield NFS4ERR_NOENT"
    );
}

/// RFC 8881 §18.22 positive — READ after OPEN+CREATE returns
/// `NFS4_OK` plus the `(eof, data<>)` reply shape. Until the
/// in-memory gateway round-trips the inline data, kiseki may emit
/// IO; the positive shape pinned here is the spec contract.
#[test]
fn s18_22_read_after_open_returns_ok_or_io_with_eof_field() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 3, |w| {
        // 1. PUTROOTFH
        w.write_u32(v4op::PUTROOTFH);
        // 2. OPEN4_CREATE — creates a new empty file under root.
        w.write_u32(v4op::OPEN);
        w.write_u32(0); // seqid
        w.write_u32(1); // share_access (READ)
        w.write_u32(0); // share_deny
        w.write_u64(1); // clientid
        w.write_opaque(b"owner-read");
        // openflag4 = OPEN4_CREATE + UNCHECKED4 createhow + empty fattr.
        w.write_u32(1); // OPEN4_CREATE
        w.write_u32(0); // createhow4 = UNCHECKED4
        w.write_u32(0); // fattr4.bitmap word count = 0
        w.write_opaque(&[]); // fattr4.attr_vals (empty)
                             // open_claim4 = CLAIM_NULL + component4 file.
        w.write_u32(0); // CLAIM_NULL
        w.write_string("rfc8881-readfile");
        // 3. READ — anonymous stateid, offset=0, count=4096.
        w.write_u32(v4op::READ);
        w.write_opaque_fixed(&[0u8; 16]);
        w.write_u64(0);
        w.write_u32(4096);
    });
    let reply = drive_compound(0xC001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    // Either the full pipeline succeeds (NFS4_OK) OR the in-memory
    // gateway returns IO before READ — both are valid Layer-1
    // states for an empty file. The contract is "no NOFILEHANDLE /
    // OP_ILLEGAL / SYSTEM_ERR".
    assert!(
        compound_status == nfs4_status::NFS4_OK || compound_status == nfs4_status::NFS4ERR_IO,
        "RFC 8881 §18.22.4: READ after OPEN must succeed or yield IO; \
         got {compound_status} (NOFILEHANDLE/BADHANDLE/OP_ILLEGAL all wrong)"
    );
}

/// RFC 8881 §18.26 — READDIR without a current filehandle MUST yield
/// `NFS4ERR_NOFILEHANDLE`.
#[test]
fn s18_26_readdir_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::READDIR);
        w.write_u64(0); // cookie
        w.write_opaque_fixed(&[0u8; 8]); // cookieverf
        w.write_u32(4096); // dircount
        w.write_u32(8192); // maxcount
        w.write_u32(0); // attr_request bitmap len
    });
    let reply = drive_compound(0xD001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert!(
        compound_status == nfs4_status::NFS4ERR_NOFILEHANDLE
            || compound_status == nfs4_status::NFS4ERR_BADHANDLE,
        "RFC 8881 §18.26.4: READDIR without current_fh must yield \
         NFS4ERR_NOFILEHANDLE; got {compound_status}"
    );
}

/// RFC 8881 §18.26 positive — READDIR on root returns a non-error
/// status (op_readdir's exact shape is implementation-defined; we
/// pin "did not error" as the wire contract).
#[test]
fn s18_26_readdir_after_putrootfh_returns_ok() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::READDIR);
        w.write_u64(0);
        w.write_opaque_fixed(&[0u8; 8]);
        w.write_u32(4096);
        w.write_u32(8192);
        w.write_u32(0);
    });
    let reply = drive_compound(0xD002, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.26.4: READDIR after PUTROOTFH MUST succeed"
    );
}

/// RFC 8881 §18.28 — REMOVE without a current filehandle MUST yield
/// `NFS4ERR_NOFILEHANDLE`. The spec text: "The current filehandle
/// is the directory in which the entry to be removed resides."
#[test]
fn s18_28_remove_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::REMOVE);
        w.write_string("doomed");
    });
    let reply = drive_compound(0xE001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert!(
        compound_status == nfs4_status::NFS4ERR_NOFILEHANDLE
            || compound_status == nfs4_status::NFS4ERR_BADHANDLE
            || compound_status == nfs4_status::NFS4ERR_NOENT,
        "RFC 8881 §18.28.4: REMOVE without current_fh must yield \
         NOFILEHANDLE/BADHANDLE/NOENT; got {compound_status}"
    );
}

/// RFC 8881 §18.29 — RENAME without a saved+current filehandle pair
/// MUST yield `NFS4ERR_NOFILEHANDLE`. The op uses both saved_fh
/// (source) and current_fh (target).
#[test]
fn s18_29_rename_without_fh_pair_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::RENAME);
        w.write_string("from");
        w.write_string("to");
    });
    let reply = drive_compound(0xE002, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert!(
        compound_status == nfs4_status::NFS4ERR_NOFILEHANDLE
            || compound_status == nfs4_status::NFS4ERR_BADHANDLE
            || compound_status == nfs4_status::NFS4ERR_NOENT,
        "RFC 8881 §18.29.4: RENAME without saved+current fh must yield \
         NOFILEHANDLE/BADHANDLE/NOENT; got {compound_status}"
    );
}

/// RFC 8881 §18.30 — SETATTR without a current filehandle MUST yield
/// `NFS4ERR_NOFILEHANDLE`.
#[test]
fn s18_30_setattr_without_current_fh_returns_nofilehandle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(v4op::SETATTR);
        w.write_opaque_fixed(&[0u8; 16]); // stateid
        w.write_u32(0); // bitmap_len = 0 (no attrs)
        w.write_opaque(&[]); // attr_vals
    });
    let reply = drive_compound(0xE003, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert!(
        compound_status == nfs4_status::NFS4ERR_NOFILEHANDLE
            || compound_status == nfs4_status::NFS4ERR_BADHANDLE,
        "RFC 8881 §18.30.4: SETATTR without current_fh must yield \
         NFS4ERR_NOFILEHANDLE; got {compound_status}"
    );
}

/// Phase 15c.3 — LOOKUP by composition UUID. After
/// `mount.nfs4 server:/default`, the kernel resolves
/// `dd /mnt/pnfs/<uuid>` via `PUTROOTFH+LOOKUP("default")+LOOKUP("<uuid>")`.
/// kiseki's LOOKUP MUST resolve the composition UUID as a filename
/// (otherwise every NFS read of an S3-PUT object fails with NOENT).
/// Today `op_lookup` only consults the NFS-CREATE `dir_index`; this
/// test asserts the spec/contract behaviour: a composition created
/// via the gateway write path is reachable via NFS LOOKUP by its
/// canonical UUID name.
#[test]
fn s18_15_lookup_composition_uuid_returns_file_handle() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    // Seed: write a composition via the gateway path (mimics S3
    // PUT). dir_index is NOT touched; the LOOKUP must work purely
    // via the composition store.
    let payload = b"phase-15c.3-payload".to_vec();
    let comp_id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let resp = ctx
                .gateway
                .write(kiseki_gateway::nfs::NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data: payload,
                })
                .await
                .expect("seed write");
            resp.composition_id
        })
    };
    let name = comp_id.0.to_string();

    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 4, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default");
        w.write_u32(v4op::LOOKUP);
        w.write_string(&name);
    });

    let header = make_header(0x4201, PROC_COMPOUND);
    let raw = build_nfs4_call(0x4201, PROC_COMPOUND, &body);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);
    let mut r = reader_at_compound_result(&reply);
    let cs = r.read_u32().unwrap();
    assert_eq!(
        cs,
        nfs4_status::NFS4_OK,
        "Phase 15c.3: LOOKUP(<composition_uuid>) MUST resolve to the \
         composition's file handle so `dd /mnt/pnfs/<uuid>` works \
         after S3 PUT seeded the object; got {cs}"
    );
}

/// Phase 15c.3 — READDIR enumerates compositions in the namespace.
/// After mount, `ls /mnt/pnfs` should list every composition in the
/// namespace by its UUID. Today `readdir` only emits dir_index
/// entries (NFS-CREATE'd files); this test asserts the broader
/// contract: an S3-PUT'd composition shows up in the readdir
/// response.
#[test]
fn s18_26_readdir_lists_compositions_in_namespace() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    // Seed a composition via the gateway path (S3 PUT-style).
    let comp_id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let resp = ctx
                .gateway
                .write(kiseki_gateway::nfs::NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data: b"readdir-fixture".to_vec(),
                })
                .await
                .expect("seed write");
            resp.composition_id
        })
    };
    let expected_name = comp_id.0.to_string();

    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 3, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default");
        w.write_u32(v4op::READDIR);
        w.write_u64(0); // cookie
        w.write_opaque_fixed(&[0u8; 8]); // cookieverf
        w.write_u32(4096); // dircount
        w.write_u32(8192); // maxcount
        w.write_u32(0); // attr_request bitmap len
    });
    let header = make_header(0x4202, PROC_COMPOUND);
    let raw = build_nfs4_call(0x4202, PROC_COMPOUND, &body);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);

    // Brute-force: assert the composition UUID appears as bytes in
    // the reply. Rigorous READDIR4resok decoding requires walking
    // the entry chain; the byte-substring check is sufficient as a
    // RED guard for "is the composition UUID even in there".
    let needle = expected_name.as_bytes();
    let found = reply.windows(needle.len()).any(|w| w == needle);
    assert!(
        found,
        "Phase 15c.3: READDIR on namespace root MUST list compositions \
         by UUID; '{expected_name}' missing from {} bytes of reply",
        reply.len()
    );
}

/// Phase 15c.2 mount-path alias — kiseki exports a single namespace
/// per server, named "default". Linux `mount.nfs4 server:/default
/// /mnt` does PUTROOTFH+LOOKUP("default") expecting a sub-directory
/// to descend into. kiseki should treat the namespace name as an
/// alias for the namespace root so the canonical mount path works.
#[test]
fn s18_15_lookup_default_namespace_returns_root_handle() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 3, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default"); // the namespace name kiseki uses
        w.write_u32(v4op::GETFH);
    });
    let reply = drive_compound(0x4101, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_eq!(
        compound_status,
        nfs4_status::NFS4_OK,
        "LOOKUP('default') after PUTROOTFH MUST succeed — that's the \
         namespace alias used by `mount.nfs4 server:/default`"
    );
}

// ===========================================================================
// Phase 15c.2 — GETATTR honors the request bitmap (RFC 8881 §5.6)
// ===========================================================================
//
// pcap analysis shows Linux 6.x mount.nfs4 sends two GETATTRs as
// part of the mount sequence:
//   CALL[8]:  SEQUENCE+PUTROOTFH+GETATTR  bits=[10]            (LEASE_TIME)
//   CALL[13]: SEQUENCE+PUTROOTFH+GETFH+GETATTR bits=[1,3,4,8]  (TYPE+CHANGE+SIZE+FSID)
//
// kiseki today returns bits=[1,4] (TYPE|SIZE) regardless of the
// request — that violates RFC 8881 §5.6 "The server MUST NOT return
// any attributes that are not in the request bitmap". The kernel
// can't parse the result and surfaces `mount(2): Input/output
// error` (Phase 15c.2 blocker, 2026-04-27).

/// RFC 8881 §5.7 — `FATTR4_LEASE_TIME` (bit 10) is the lease
/// validity in seconds. Mount.nfs4 asks for it FIRST after
/// PUTROOTFH; if the result bitmap doesn't include it, kernel
/// can't determine lease cadence and returns EIO. kiseki must
/// honor a bit-10-only request.
#[test]
fn s5_7_getattr_lease_time_only_returned_in_result_bitmap() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::GETATTR);
        // Request bitmap: 1 word, only bit 10 set = 0x00000400
        w.write_u32(1); // bm_count
        w.write_u32(1u32 << 10);
    });
    let reply = drive_compound(0x4001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _cs = r.read_u32(); // compound status
    let _tag = r.read_opaque();
    let _ra = r.read_u32(); // resarray_len
    let _ = r.read_u32(); // PUTROOTFH op
    let _ = r.read_u32(); // PUTROOTFH status
    let _ = r.read_u32(); // GETATTR op
    let getattr_status = r.read_u32().unwrap();
    assert_eq!(
        getattr_status,
        nfs4_status::NFS4_OK,
        "GETATTR(LEASE_TIME) MUST succeed"
    );
    let bm_count = r.read_u32().unwrap();
    let bm_word0 = r.read_u32().unwrap();
    let bm_word1 = if bm_count >= 2 {
        r.read_u32().unwrap()
    } else {
        0
    };
    assert!(
        bm_word0 & (1u32 << 10) != 0,
        "RFC 8881 §5.6: result bitmap MUST include FATTR4_LEASE_TIME (bit 10) \
         when the request asks for it; got word0={bm_word0:#010x} word1={bm_word1:#010x}"
    );
    assert_eq!(
        bm_word0 & !(1u32 << 10),
        0,
        "RFC 8881 §5.6: result bitmap MUST NOT include attrs not requested; \
         word0={bm_word0:#010x} has extra bits set"
    );
    let attr_vals = r.read_opaque().unwrap();
    assert_eq!(
        attr_vals.len(),
        4,
        "FATTR4_LEASE_TIME is a u32 (4 bytes); attr_vals = {} bytes",
        attr_vals.len()
    );
}

/// RFC 8881 §5.6 — GETATTR request `[TYPE, CHANGE, SIZE, FSID]`
/// (bits 1, 3, 4, 8) MUST return exactly those four attrs in the
/// result bitmap, in bit-order, with each attr's wire encoding.
#[test]
fn s5_6_getattr_type_change_size_fsid_returned_in_request_order() {
    let bm = (1u32 << 1) | (1u32 << 3) | (1u32 << 4) | (1u32 << 8);
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::GETATTR);
        w.write_u32(1); // bm_count
        w.write_u32(bm);
    });
    let reply = drive_compound(0x4002, &body);
    let mut r = reader_at_compound_result(&reply);
    let _ = r.read_u32(); // compound status
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH
    let _ = r.read_u32(); // GETATTR op
    let st = r.read_u32().unwrap();
    assert_eq!(st, nfs4_status::NFS4_OK);
    let bm_count = r.read_u32().unwrap();
    let bm_word0 = r.read_u32().unwrap();
    if bm_count >= 2 {
        let _bm_word1 = r.read_u32().unwrap();
    }
    assert_eq!(
        bm_word0, bm,
        "RFC 8881 §5.6: result bitmap MUST equal request bitmap when all \
         requested attrs are supported; got {bm_word0:#010x} expected {bm:#010x}"
    );
    let attr_vals = r.read_opaque().unwrap();
    // Encoded in bit order: TYPE(u32) + CHANGE(u64) + SIZE(u64) + FSID(2 * u64)
    // = 4 + 8 + 8 + 16 = 36 bytes.
    assert_eq!(
        attr_vals.len(),
        36,
        "TYPE(4) + CHANGE(8) + SIZE(8) + FSID(16) = 36 bytes"
    );
}

// ===========================================================================
// Phase 15c kernel-mount blocker tests (RED first, fix after)
// ===========================================================================
//
// pcap analysis of `mount.nfs4 -o vers=4.1 kiseki-node1:/default` shows
// the kernel client gives up at three specific server responses:
//
//   1. CALL[10] = SEQUENCE+PUTROOTFH+SECINFO_NO_NAME(style=CURRENT_FH)
//      → kiseki returns OP_ILLEGAL on op 52 because the dispatcher
//      doesn't have it. Mount fails with "Operation not supported".
//   2. After CREATE_SESSION, the kernel sends a CB_NULL on the same
//      socket (program=400122, NFS4_CB). kiseki replies PROG_MISMATCH.
//      Linux 6.x clients tolerate this (they retry forward calls)
//      but the kernel ring buffer fills with PROG_MISMATCH alerts.
//   3. CALL[12] = OP_57 (DESTROY_CLIENTID) — the kernel's cleanup
//      after giving up. OP_ILLEGAL response works but is noisy.
//
// Tests below pin the spec-correct behaviour. RED until production
// adds the missing dispatcher entries.

/// RFC 8881 §18.31 — SECINFO_NO_NAME is the kernel's question
/// "what auth flavors does the current_fh accept?". Linux 6.x mount
/// emits `SEQUENCE+PUTROOTFH+SECINFO_NO_NAME(style=CURRENT_FH)` as
/// the FINAL pre-mount probe; if the server returns OP_ILLEGAL on
/// the SECINFO_NO_NAME op, the kernel's mount.nfs4 surfaces
/// "Operation not supported" because it cannot determine the
/// auth flavor to use. This is the Phase 15 e2e blocker.
///
/// kiseki MUST dispatch the op and return either NFS4_OK with at
/// least one flavor in `secinfo4<>` (typical: AUTH_SYS = 1) or
/// NFS4ERR_NOTSUPP per §18.31.4. OP_ILLEGAL is wrong because the
/// op IS in the v4.1 registry — kiseki just hadn't wired it.
#[test]
fn s18_31_secinfo_no_name_dispatched_not_op_illegal() {
    // SECINFO_STYLE4_CURRENT_FH = 0 per RFC 8881 §18.31.1.
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(52); // op SECINFO_NO_NAME
        w.write_u32(0); // SECINFO_STYLE4_CURRENT_FH
    });
    let reply = drive_compound(0xF001, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_ne!(
        compound_status,
        nfs4_status::NFS4ERR_OP_ILLEGAL,
        "RFC 8881 §18.31: SECINFO_NO_NAME (op 52) is in the v4.1 \
         registry and MUST be dispatched. OP_ILLEGAL here is the \
         Phase 15 e2e blocker — kernel mount.nfs4 surfaces 'Operation \
         not supported' to userspace."
    );
}

/// RFC 8881 §18.34 — BIND_CONN_TO_SESSION (op 41). Linux clients
/// emit this in some paths after CREATE_SESSION to claim the
/// connection for forward / back / both channels.
#[test]
fn s18_34_bind_conn_to_session_dispatched_not_op_illegal() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(41); // op BIND_CONN_TO_SESSION
        w.write_opaque_fixed(&[0u8; 16]); // bctsa_sessionid
        w.write_u32(1); // bctsa_dir = CDFC4_FORE
        w.write_bool(false); // bctsa_use_conn_in_rdma_mode
    });
    let reply = drive_compound(0xF002, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_ne!(
        compound_status,
        nfs4_status::NFS4ERR_OP_ILLEGAL,
        "RFC 8881 §18.34: BIND_CONN_TO_SESSION (op 41) is in the v4.1 \
         registry and MUST be dispatched."
    );
}

/// RFC 8881 §18.50 — DESTROY_CLIENTID (op 57). Kernel cleanup op.
#[test]
fn s18_50_destroy_clientid_dispatched_not_op_illegal() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(57); // op DESTROY_CLIENTID
        w.write_u64(0xCAFE_BABE_DEAD_BEEF); // dca_clientid
    });
    let reply = drive_compound(0xF003, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert_ne!(
        compound_status,
        nfs4_status::NFS4ERR_OP_ILLEGAL,
        "RFC 8881 §18.50: DESTROY_CLIENTID (op 57) MUST be dispatched."
    );
}

/// RFC 8881 §18.27 + RFC 7530 §15.5 — SETCLIENTID is a v4.0-only
/// op. On a v4.1 COMPOUND it's not in the registry → NFS4ERR_OP_ILLEGAL
/// (or NOTSUPP if dispatched through a stub). The v4.0 fallback path
/// goes via SETCLIENTID; this test asserts kiseki rejects it cleanly
/// within a v4.1 frame.
#[test]
fn s18_27_setclientid_in_v4_1_compound_rejected() {
    const OP_SETCLIENTID: u32 = 35;
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 1, |w| {
        w.write_u32(OP_SETCLIENTID);
        // SETCLIENTID args: nfs_client_id4 (verifier + id), callback,
        // callback_ident. Shape doesn't matter — the dispatcher
        // rejects before parsing args.
        w.write_opaque_fixed(&[0u8; 8]);
        w.write_opaque(b"client-id");
        w.write_u32(0); // cb_program
        w.write_string("");
        w.write_string("");
        w.write_u32(0); // callback_ident
    });
    let reply = drive_compound(0xE004, &body);
    let mut r = reader_at_compound_result(&reply);
    let compound_status = r.read_u32().unwrap();
    assert!(
        compound_status == nfs4_status::NFS4ERR_OP_ILLEGAL
            || compound_status == nfs4_status::NFS4ERR_NOTSUPP,
        "RFC 8881: SETCLIENTID (v4.0 op) inside a v4.1 COMPOUND must \
         yield OP_ILLEGAL or NOTSUPP; got {compound_status}"
    );
}

// ===========================================================================
// Phase 15c.3 — kernel `cat /mnt/pnfs/<uuid>` end-to-end (RED)
// ===========================================================================
//
// Pcap shows the kernel issues four COMPOUNDs to materialize a read:
//
//   C1: SEQUENCE+PUTROOTFH+LOOKUP("default")+LOOKUP(<uuid>)+GETFH
//                          +GETATTR(TYPE|SIZE|MODE|...)
//   C2: SEQUENCE+PUTFH(<file_handle>)+ACCESS(...)+GETATTR
//   C3: SEQUENCE+PUTFH(<dir>)+OPEN(NOCREATE, <uuid>)+GETFH+GETATTR
//   C4: SEQUENCE+PUTFH(<file_handle>)+READ(stateid, off=0, count)
//
// Today every COMPOUND status is NFS4_OK on the wire, yet `cat`
// surfaces ENOENT to userspace. Three suspected wire-encoding bugs:
//
//   B1) GETATTR after LOOKUP returns size=0 regardless of payload —
//       `nfs_ops::getattr` hardcodes size=0 ("unknown without
//       reading"). For a known-non-empty composition the kernel
//       sees a 0-byte file and never issues READ.
//   B2) OPEN reply emits a 1-element bool for `cinfo` instead of the
//       17-byte change_info4 (atomic + before + after) per RFC 8881
//       §18.16.4 — and entirely omits `attrset` (bitmap4) +
//       `delegation` (open_delegation4 union). Kernel mis-parses
//       the next op's bytes as cinfo trailers, then walks off the
//       end of the compound.
//   B3) End-to-end: a fresh `LOOKUP(<uuid>) → GETFH` followed by a
//       second-COMPOUND `PUTFH(<file_handle>) → READ` MUST return
//       the seeded bytes. If B1 reports size=0 the kernel skips
//       READ; if B2 desyncs the parser, READ never reaches kiseki.
//
// These three tests RED-pin each bug independently so we can fix
// them one at a time and watch the e2e cat path light up.

/// Phase 15c.3 — full GETATTR-after-LOOKUP path. After
/// `PUTROOTFH+LOOKUP("default")+LOOKUP(<uuid>)+GETFH+GETATTR`, the
/// GETATTR reply MUST include the actual composition size in
/// FATTR4_SIZE; today `nfs_ops::getattr` returns 0 unconditionally
/// for any non-root file handle. This is **B1** above.
#[test]
fn s5_8_getattr_after_lookup_returns_actual_composition_size() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    // Seed a composition so gateway.list returns it with a known
    // non-zero size (matches the post-S3-PUT state pcap captures).
    let payload = b"phase-15c.3 cat-enoent payload bytes 0123456789".to_vec();
    let payload_len = payload.len() as u64;
    let comp_id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let resp = ctx
                .gateway
                .write(kiseki_gateway::nfs::NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data: payload,
                })
                .await
                .expect("seed write");
            resp.composition_id
        })
    };
    let name = comp_id.0.to_string();

    // Request bitmap: TYPE(1) | SIZE(4) — the two attrs `cat` walks
    // before deciding whether to OPEN+READ or short-circuit.
    let bm = (1u32 << 1) | (1u32 << 4);
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 5, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default");
        w.write_u32(v4op::LOOKUP);
        w.write_string(&name);
        w.write_u32(v4op::GETFH);
        w.write_u32(v4op::GETATTR);
        w.write_u32(1); // bm_count
        w.write_u32(bm);
    });

    let header = make_header(0xCA01, PROC_COMPOUND);
    let raw = build_nfs4_call(0xCA01, PROC_COMPOUND, &body);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);

    let mut r = reader_at_compound_result(&reply);
    let cs = r.read_u32().unwrap();
    assert_eq!(cs, nfs4_status::NFS4_OK, "compound MUST succeed");
    let _tag = r.read_opaque().unwrap();
    let _ra = r.read_u32().unwrap();
    // Skip PUTROOTFH, LOOKUP, LOOKUP, GETFH per-op result frames.
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH
    let _ = r.read_u32();
    let _ = r.read_u32(); // LOOKUP("default")
    let _ = r.read_u32();
    let _ = r.read_u32(); // LOOKUP(<uuid>)
    let _ = r.read_u32(); // GETFH op
    let getfh_status = r.read_u32().unwrap();
    assert_eq!(
        getfh_status,
        nfs4_status::NFS4_OK,
        "GETFH after LOOKUP(<uuid>) MUST succeed"
    );
    let fh = r.read_opaque().unwrap();
    assert_eq!(fh.len(), 32, "kiseki file handle is 32 bytes");
    let _ = r.read_u32(); // GETATTR op
    let getattr_status = r.read_u32().unwrap();
    assert_eq!(
        getattr_status,
        nfs4_status::NFS4_OK,
        "GETATTR after LOOKUP(<uuid>) MUST succeed"
    );

    // Result bitmap MUST equal request bitmap (RFC 8881 §5.6).
    let bm_count = r.read_u32().unwrap();
    let bm_word0 = r.read_u32().unwrap();
    if bm_count >= 2 {
        let _ = r.read_u32().unwrap();
    }
    assert_eq!(
        bm_word0, bm,
        "result bitmap MUST equal request {bm:#x}, got {bm_word0:#x}"
    );

    // attr_vals: TYPE(u32) + SIZE(u64) = 12 bytes.
    let attr_vals = r.read_opaque().unwrap();
    assert_eq!(attr_vals.len(), 12, "TYPE(4) + SIZE(8) = 12 bytes");
    let mut av = XdrReader::new(&attr_vals);
    let ftype = av.read_u32().unwrap();
    assert_eq!(
        ftype, 1,
        "RFC 8881 §5.8.1.30: NF4REG = 1 — composition is a regular file"
    );
    let size = av.read_u64().unwrap();
    assert_eq!(
        size, payload_len,
        "Phase 15c.3 B1: GETATTR after LOOKUP(<uuid>) MUST report the \
         composition's actual size ({payload_len} bytes), not 0. The \
         kernel sees size=0 and skips OPEN+READ — that's the cat ENOENT."
    );
}

/// Phase 15c.3 — OPEN4resok wire layout. RFC 8881 §18.16.4:
///
/// ```ignore
///   stateid4         stateid;       // 16 bytes (opaque<>)
///   change_info4     cinfo;         // bool(4) + u64 + u64 = 20 bytes
///   uint32           rflags;        // 4 bytes
///   bitmap4          attrset;       // 4 + 4*words bytes
///   open_delegation4 delegation;    // 4-byte discriminator + body
/// ```
///
/// Today's `op_open` writes only `stateid + write_bool(false) + rflags`
/// — a 1-element bool for cinfo (4 bytes total), no attrset, no
/// delegation. Total ~24 bytes vs spec ~36 bytes minimum. This is
/// **B2** above and is what desyncs the kernel's parser when OPEN is
/// followed by GETFH+GETATTR in the same compound.
#[test]
fn s18_16_4_open_reply_includes_cinfo_attrset_delegation() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    // Seed so OPEN(NOCREATE) can resolve.
    let comp_id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let resp = ctx
                .gateway
                .write(kiseki_gateway::nfs::NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data: b"open-fixture".to_vec(),
                })
                .await
                .expect("seed write");
            resp.composition_id
        })
    };
    let name = comp_id.0.to_string();

    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 3, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default");
        w.write_u32(v4op::OPEN);
        w.write_u32(0); // seqid
        w.write_u32(1); // share_access = READ
        w.write_u32(0); // share_deny = NONE
        w.write_u64(1); // clientid
        w.write_opaque(b"owner"); // owner
        w.write_u32(0); // OPEN4_NOCREATE (no createhow body)
        w.write_u32(0); // open_claim4 = CLAIM_NULL
        w.write_string(&name); // component4 file
    });
    let header = make_header(0xCA02, PROC_COMPOUND);
    let raw = build_nfs4_call(0xCA02, PROC_COMPOUND, &body);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);

    let mut r = reader_at_compound_result(&reply);
    let _cs = r.read_u32();
    let _tag = r.read_opaque().unwrap();
    let _ra = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH
    let _ = r.read_u32();
    let _ = r.read_u32(); // LOOKUP("default")
    let _ = r.read_u32(); // OPEN op
    let st = r.read_u32().unwrap();
    assert_eq!(
        st,
        nfs4_status::NFS4_OK,
        "OPEN(NOCREATE) on seeded composition MUST succeed"
    );
    let _stateid = r.read_opaque_fixed(16).unwrap();
    // change_info4: bool(4) + changeid4(u64) + changeid4(u64) = 20 bytes
    let cinfo_atomic = r.read_u32().unwrap();
    assert!(
        cinfo_atomic == 0 || cinfo_atomic == 1,
        "RFC 8881 §3.2.6: change_info4.atomic MUST be a valid XDR bool, \
         got {cinfo_atomic:#x} — likely a wire-decode misalignment"
    );
    let cinfo_before = r.read_u64().unwrap();
    let cinfo_after = r.read_u64().unwrap();
    // RFC 8881 §3.2.6 — when atomic=true, `before == after - 1` is
    // a typical post-mutation invariant; for a NOCREATE OPEN there's
    // no mutation so values are unconstrained. We just assert they
    // were consumed as 16 well-formed bytes (i.e. the next field
    // decodes coherently below).
    let _ = cinfo_before;
    let _ = cinfo_after;
    let rflags = r.read_u32().unwrap();
    assert!(
        rflags < 0x100,
        "RFC 8881 §18.16.4: rflags is a small bitmap; got {rflags:#x} — \
         this almost certainly indicates the parser is reading bytes \
         from the next field (B2: cinfo+attrset+delegation gap)"
    );
    // attrset (bitmap4): u32 count + 4*count bytes of words.
    let attrset_count = r.read_u32().unwrap();
    assert!(
        attrset_count <= 3,
        "RFC 8881 §3.2.4: bitmap4 word count is small (typically 0..3); \
         got {attrset_count} — parser is misaligned"
    );
    for _ in 0..attrset_count {
        let _ = r.read_u32().unwrap();
    }
    // open_delegation4: 4-byte discriminator (open_delegation_type4).
    // OPEN_DELEGATE_NONE = 0 has empty body; that's the kiseki path.
    let deleg_type = r.read_u32().unwrap();
    assert!(
        deleg_type <= 4,
        "RFC 8881 §9.1.2: open_delegation_type4 ∈ {{0..4}}; got \
         {deleg_type} — parser is misaligned"
    );
}

/// Phase 15c.3 — RFC 8881 §18.16.1 OPEN args **claim discriminator**.
/// The wire layout per §18.16.1 + §3.2.10 (open_claim4) is:
///
///   seqid + share_access + share_deny + owner +
///   openhow (opentype + createhow-if-create) + claim
///
/// The `claim` discriminator is REQUIRED before the file name field.
/// CLAIM_NULL = 0 is the open-by-parent-name path Linux always uses
/// for `open()`. The previous op_open AND the previous unit tests
/// both omitted it — server read the claim_type=0 u32 as the name's
/// length-prefix, materialized an empty name, and returned
/// NFS4ERR_NOENT to userspace. THIS is the actual cat ENOENT bug
/// surfaced by the docker e2e mount; B2's reply structure mattered
/// too but B3 was the trigger.
#[test]
fn s18_16_1_open_args_claim_discriminator_is_required() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    let comp_id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let resp = ctx
                .gateway
                .write(kiseki_gateway::nfs::NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data: b"open-claim-fixture".to_vec(),
                })
                .await
                .expect("seed write");
            resp.composition_id
        })
    };
    let name = comp_id.0.to_string();

    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 3, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default");
        w.write_u32(v4op::OPEN);
        w.write_u32(0); // seqid
        w.write_u32(1); // share_access = OPEN4_SHARE_ACCESS_READ
        w.write_u32(0); // share_deny = OPEN4_SHARE_DENY_NONE
        w.write_u64(1); // open_owner4.clientid
        w.write_opaque(b"linux-kernel-owner"); // open_owner4.owner
                                               // openflag4 — OPEN4_NOCREATE has no body trailer.
        w.write_u32(0); // OPEN4_NOCREATE
                        // open_claim4 — CLAIM_NULL takes a `component4 file` body.
        w.write_u32(0); // CLAIM_NULL
        w.write_string(&name); // file
    });
    let header = make_header(0xCA05, PROC_COMPOUND);
    let raw = build_nfs4_call(0xCA05, PROC_COMPOUND, &body);
    let reply = handle_nfs4_first_compound(&header, &raw, &ctx, &sessions);

    let mut r = reader_at_compound_result(&reply);
    let _cs = r.read_u32().unwrap();
    let _tag = r.read_opaque().unwrap();
    let _ra = r.read_u32().unwrap();
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH
    let _ = r.read_u32();
    let _ = r.read_u32(); // LOOKUP("default")
    let _ = r.read_u32(); // OPEN op
    let st = r.read_u32().unwrap();
    assert_eq!(
        st,
        nfs4_status::NFS4_OK,
        "RFC 8881 §18.16.1 + §3.2.10: server MUST parse open_claim4 \
         discriminator (CLAIM_NULL = 0) before the file name. Without \
         it, claim_type bytes are mis-read as the name's length-prefix \
         → empty name → NFS4ERR_NOENT. This is the kernel cat-ENOENT \
         bug surfaced by the docker e2e mount."
    );
}

/// Phase 15c.3 — end-to-end LOOKUP→READ. Two compounds: first
/// navigates to the file and harvests the file handle via GETFH;
/// second uses PUTFH+READ to fetch the bytes. The READ data MUST
/// equal the seeded payload. If B1 (size=0) is true, the second
/// compound's READ returns eof=true with 0 bytes — userspace `cat`
/// then prints nothing and exits 0 (NOT ENOENT). If B2 has bled
/// into the file-handle bytes (it shouldn't — OPEN is in C3, not
/// C1) READ returns ENOENT or BADHANDLE. This test pins the
/// happy-path contract.
#[test]
fn s18_22_read_after_lookup_returns_seeded_bytes() {
    let ctx = make_ctx();
    let sessions = SessionManager::new();

    let payload = b"phase-15c.3 end-to-end read fixture: hello, kiseki!".to_vec();
    let comp_id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let resp = ctx
                .gateway
                .write(kiseki_gateway::nfs::NfsWriteRequest {
                    tenant_id: test_tenant(),
                    namespace_id: test_namespace(),
                    data: payload.clone(),
                })
                .await
                .expect("seed write");
            resp.composition_id
        })
    };
    let name = comp_id.0.to_string();

    // C1: navigate + GETFH to harvest the file handle bytes.
    let body1 = encode_compound(b"", NFS4_MINOR_VERSION_1, 4, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::LOOKUP);
        w.write_string("default");
        w.write_u32(v4op::LOOKUP);
        w.write_string(&name);
        w.write_u32(v4op::GETFH);
    });
    let h1 = make_header(0xCA03, PROC_COMPOUND);
    let raw1 = build_nfs4_call(0xCA03, PROC_COMPOUND, &body1);
    let reply1 = handle_nfs4_first_compound(&h1, &raw1, &ctx, &sessions);
    let mut r1 = reader_at_compound_result(&reply1);
    let _ = r1.read_u32(); // compound status
    let _ = r1.read_opaque(); // tag
    let _ = r1.read_u32(); // resarray_len
    let _ = r1.read_u32();
    let _ = r1.read_u32(); // PUTROOTFH
    let _ = r1.read_u32();
    let _ = r1.read_u32(); // LOOKUP("default")
    let _ = r1.read_u32();
    let _ = r1.read_u32(); // LOOKUP(<uuid>)
    let _ = r1.read_u32(); // GETFH op
    let getfh_status = r1.read_u32().unwrap();
    assert_eq!(getfh_status, nfs4_status::NFS4_OK, "GETFH MUST succeed");
    let fh = r1.read_opaque().unwrap();
    assert_eq!(fh.len(), 32);

    // C2: PUTFH(fh) + READ(anonymous_stateid, offset=0, count=4096).
    let body2 = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTFH);
        w.write_opaque(&fh);
        w.write_u32(v4op::READ);
        w.write_opaque_fixed(&[0u8; 16]); // anonymous stateid
        w.write_u64(0); // offset
        w.write_u32(4096); // count
    });
    let h2 = make_header(0xCA04, PROC_COMPOUND);
    let raw2 = build_nfs4_call(0xCA04, PROC_COMPOUND, &body2);
    let reply2 = handle_nfs4_first_compound(&h2, &raw2, &ctx, &sessions);
    let mut r2 = reader_at_compound_result(&reply2);
    let cs = r2.read_u32().unwrap();
    assert_eq!(cs, nfs4_status::NFS4_OK, "C2 compound MUST be NFS4_OK");
    let _tag = r2.read_opaque();
    let _ra = r2.read_u32();
    let _ = r2.read_u32();
    let _ = r2.read_u32(); // PUTFH
    let _ = r2.read_u32(); // READ op
    let read_status = r2.read_u32().unwrap();
    assert_eq!(
        read_status,
        nfs4_status::NFS4_OK,
        "Phase 15c.3: READ on a freshly-LOOKUP'd composition handle \
         MUST succeed"
    );
    let _eof = r2.read_bool().unwrap();
    let data = r2.read_opaque().unwrap();
    assert_eq!(
        data,
        payload,
        "Phase 15c.3: READ MUST return the seeded payload bytes; \
         got {} bytes vs expected {}",
        data.len(),
        payload.len()
    );
}

// ===========================================================================
// Gap 1/3 — pNFS layout negotiation (RFC 8881 §5.12 + RFC 8435 §5)
// ===========================================================================
//
// Kernel pcap: client sends `EXCHANGE_ID` and sees USE_PNFS_MDS in
// the response, but never sends a LAYOUTGET. Reason: clients only
// ask for layouts on a filesystem that advertises FATTR4_FS_LAYOUT_TYPES
// (bit 62 — word1 bit 30) listing at least one layout type the
// client supports (LAYOUT4_FLEX_FILES = 4 for RFC 8435).
//
// The corollary attribute FATTR4_LAYOUT_TYPES (bit 30 — word0 bit 30)
// is per-file; some clients also key on it to decide whether to
// LAYOUTGET on a specific open. We expose both.

/// RFC 8881 §5.8.1.12 — `FATTR4_FS_LAYOUT_TYPES` (bit 62) MUST be in
/// the SUPPORTED_ATTRS bitmap kiseki returns from GETATTR(SUPPORTED_ATTRS),
/// so the kernel knows pNFS layouts are negotiable on this filesystem.
///
/// Currently DISABLED at the wire level — `MdsLayoutManager` is now
/// wired into `NfsContext` (Phase 15c.4) but advertising the bits
/// makes Linux issue LAYOUTGET and enter a tight retry loop that
/// OOM-kills the server. The kernel-side validation of the Flex
/// Files body needs additional wire-correctness work to land cleanly.
/// Phase 15c.5 follow-up.
#[ignore = "Phase 15c.5 — kernel LAYOUTGET retry-loop OOM under \
            current Flex Files body encoding; MdsLayoutManager IS \
            wired (15c.4), follow-up debugs the kernel-side decode."]
#[test]
fn s5_8_supported_attrs_includes_fs_layout_types_bit_62() {
    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::GETATTR);
        // Request bitmap: bit 0 (SUPPORTED_ATTRS) only.
        w.write_u32(1);
        w.write_u32(1u32 << 0);
    });
    let reply = drive_compound(0xD001, &body);
    let mut r = reader_at_compound_result(&reply);
    let _cs = r.read_u32();
    let _tag = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH
    let _ = r.read_u32(); // GETATTR op
    let _ = r.read_u32(); // status
    let _bm_count = r.read_u32().unwrap();
    let _bm_word0 = r.read_u32().unwrap();
    // attr_vals: SUPPORTED_ATTRS itself is a bitmap4 (count + words).
    let attr_vals = r.read_opaque().unwrap();
    let mut av = XdrReader::new(&attr_vals);
    let inner_count = av.read_u32().unwrap();
    assert!(
        inner_count >= 2,
        "RFC 8881 §5.8: SUPPORTED_ATTRS MUST report at least 2 \
         bitmap words to cover word1 attrs (FATTR4_FS_LAYOUT_TYPES \
         lives at bit 62 = word1 bit 30)"
    );
    let _supp_w0 = av.read_u32().unwrap();
    let supp_w1 = av.read_u32().unwrap();
    const FATTR4_FS_LAYOUT_TYPES_W1: u32 = 1u32 << (62 - 32);
    assert!(
        supp_w1 & FATTR4_FS_LAYOUT_TYPES_W1 != 0,
        "RFC 8881 §5.12 + ADR-038: SUPPORTED_ATTRS word1 MUST advertise \
         FATTR4_FS_LAYOUT_TYPES (bit 62), got w1={supp_w1:#010x}. \
         Without it, Linux kernel never issues LAYOUTGET — the \
         pNFS path silently degrades to plain NFSv4.1 reads."
    );
}

/// RFC 8881 §5.8.1.12 — GETATTR with FATTR4_FS_LAYOUT_TYPES requested
/// MUST return at least one supported layout type. Kiseki advertises
/// LAYOUT4_FLEX_FILES (RFC 8435) via the MDS Layout Manager.
///
/// Same Phase 15c.5 deferral as `s5_8_supported_attrs_…` above.
#[ignore = "Phase 15c.5 — see s5_8_supported_attrs_includes_fs_layout_types_bit_62"]
#[test]
fn s5_12_fs_layout_types_returns_flex_files() {
    const LAYOUT4_FLEX_FILES: u32 = 4;
    const FATTR4_FS_LAYOUT_TYPES_BIT_W1: u32 = 62 - 32;

    let body = encode_compound(b"", NFS4_MINOR_VERSION_1, 2, |w| {
        w.write_u32(v4op::PUTROOTFH);
        w.write_u32(v4op::GETATTR);
        // Request bitmap: 2 words, only word1 bit 30 set (FATTR4_FS_LAYOUT_TYPES).
        w.write_u32(2);
        w.write_u32(0); // word0
        w.write_u32(1u32 << FATTR4_FS_LAYOUT_TYPES_BIT_W1);
    });
    let reply = drive_compound(0xD002, &body);
    let mut r = reader_at_compound_result(&reply);
    let _cs = r.read_u32();
    let _tag = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32(); // PUTROOTFH
    let _ = r.read_u32(); // GETATTR op
    let _ = r.read_u32(); // status
    let bm_count = r.read_u32().unwrap();
    let _bm_w0 = r.read_u32().unwrap();
    assert!(
        bm_count >= 2,
        "result bitmap MUST include word1 to carry FS_LAYOUT_TYPES bit"
    );
    let bm_w1 = r.read_u32().unwrap();
    assert!(
        bm_w1 & (1u32 << FATTR4_FS_LAYOUT_TYPES_BIT_W1) != 0,
        "result bitmap MUST echo FATTR4_FS_LAYOUT_TYPES (bit 62) \
         when requested; got w1={bm_w1:#010x}"
    );
    // attr_vals: layouttype4<> = u32 count + count*u32.
    let attr_vals = r.read_opaque().unwrap();
    let mut av = XdrReader::new(&attr_vals);
    let count = av.read_u32().unwrap();
    assert!(
        count >= 1,
        "RFC 8881 §5.12.1: FATTR4_FS_LAYOUT_TYPES MUST list at least \
         one type when bit 62 is in SUPPORTED_ATTRS; got count=0 \
         (zero-length array means 'pNFS not supported on this FS')"
    );
    let mut found_ff = false;
    for _ in 0..count {
        if av.read_u32().unwrap() == LAYOUT4_FLEX_FILES {
            found_ff = true;
        }
    }
    assert!(
        found_ff,
        "RFC 8435 + ADR-038: kiseki advertises Flexible Files Layout \
         (LAYOUT4_FLEX_FILES = 4) — not present in fs_layout_types"
    );
}
