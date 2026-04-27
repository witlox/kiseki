//! Layer 1 reference tests for **RFC 1813 — NFS Version 3 Protocol
//! Specification** (June 1995).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::nfs3_server` is the procedure-based
//! dispatcher. Public surface is `handle_nfs3_first_message(header,
//! raw_msg, ctx) -> Vec<u8>`. The header carries `xid`, `program`,
//! `version`, `procedure`; the raw message is a full ONC-RPC call
//! frame (header + procedure-args). These tests assemble those frames
//! with `XdrWriter` per RFC 1813 §3.0 procedure argument grammars and
//! decode replies against the response shapes the spec defines.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 1813".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc1813> (no errata
//! affecting wire format as of 2026-04-27).
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
use kiseki_gateway::nfs3_server::handle_nfs3_first_message;
use kiseki_gateway::nfs_ops::NfsContext;
use kiseki_gateway::nfs_xdr::{RpcCallHeader, XdrReader, XdrWriter};

// ===========================================================================
// Sentinel constants — pin the wire registry per RFC 1813 §3.0
// ===========================================================================

/// RFC 1813 §1.2 — NFSv3 program/version sentinels. These ride the
/// ONC RPC call header (RFC 5531 §9). A future refactor must NOT
/// renumber them.
const NFS3_PROGRAM: u32 = 100003;
const NFS3_VERSION: u32 = 3;

/// RFC 1813 §3.0 — every NFSv3 procedure number, even the ones we
/// don't implement. Pinning the full registry ensures the dispatcher
/// can be audited against the spec by `cat`-grepping this file.
mod proc {
    pub const NULL: u32 = 0; // RFC 1813 §3.3.0
    pub const GETATTR: u32 = 1; // RFC 1813 §3.3.1
    pub const SETATTR: u32 = 2; // RFC 1813 §3.3.2
    pub const LOOKUP: u32 = 3; // RFC 1813 §3.3.3
    pub const ACCESS: u32 = 4; // RFC 1813 §3.3.4
    pub const READLINK: u32 = 5; // RFC 1813 §3.3.5
    pub const READ: u32 = 6; // RFC 1813 §3.3.6
    pub const WRITE: u32 = 7; // RFC 1813 §3.3.7
    pub const CREATE: u32 = 8; // RFC 1813 §3.3.8
    pub const MKDIR: u32 = 9; // RFC 1813 §3.3.9
    pub const SYMLINK: u32 = 10; // RFC 1813 §3.3.10
    pub const MKNOD: u32 = 11; // RFC 1813 §3.3.11
    pub const REMOVE: u32 = 12; // RFC 1813 §3.3.12
    pub const RMDIR: u32 = 13; // RFC 1813 §3.3.13
    pub const RENAME: u32 = 14; // RFC 1813 §3.3.14
    pub const LINK: u32 = 15; // RFC 1813 §3.3.15
    pub const READDIR: u32 = 16; // RFC 1813 §3.3.16
    pub const READDIRPLUS: u32 = 17; // RFC 1813 §3.3.17
    pub const FSSTAT: u32 = 18; // RFC 1813 §3.3.18
    pub const FSINFO: u32 = 19; // RFC 1813 §3.3.19
    pub const PATHCONF: u32 = 20; // RFC 1813 §3.3.20
    pub const COMMIT: u32 = 21; // RFC 1813 §3.3.21
}

/// RFC 1813 §2.6 — NFS3ERR_* status codes. Listed for the negatives
/// we exercise from the wire side.
#[allow(dead_code)]
mod status {
    pub const NFS3_OK: u32 = 0;
    pub const NFS3ERR_NOENT: u32 = 2;
    pub const NFS3ERR_IO: u32 = 5;
    pub const NFS3ERR_NOTSUPP: u32 = 10004;
    pub const NFS3ERR_BADHANDLE: u32 = 10001;
}

/// RFC 1813 §3.0 — pin every procedure number declared in the spec,
/// even those kiseki does not implement. The dispatcher MUST recognize
/// each number; un-implemented procs route to PROC_UNAVAIL but the
/// number is reserved.
#[test]
fn s3_0_procedure_registry_pinned() {
    assert_eq!(NFS3_PROGRAM, 100003, "RFC 1813 §1.2: program = 100003");
    assert_eq!(NFS3_VERSION, 3, "RFC 1813 §1.2: version = 3");

    // Spec defines 22 procedures (NULL=0 .. COMMIT=21). The list
    // here is the canonical wire-side registry.
    let registry: &[(u32, &str)] = &[
        (proc::NULL, "NULL"),
        (proc::GETATTR, "GETATTR"),
        (proc::SETATTR, "SETATTR"),
        (proc::LOOKUP, "LOOKUP"),
        (proc::ACCESS, "ACCESS"),
        (proc::READLINK, "READLINK"),
        (proc::READ, "READ"),
        (proc::WRITE, "WRITE"),
        (proc::CREATE, "CREATE"),
        (proc::MKDIR, "MKDIR"),
        (proc::SYMLINK, "SYMLINK"),
        (proc::MKNOD, "MKNOD"),
        (proc::REMOVE, "REMOVE"),
        (proc::RMDIR, "RMDIR"),
        (proc::RENAME, "RENAME"),
        (proc::LINK, "LINK"),
        (proc::READDIR, "READDIR"),
        (proc::READDIRPLUS, "READDIRPLUS"),
        (proc::FSSTAT, "FSSTAT"),
        (proc::FSINFO, "FSINFO"),
        (proc::PATHCONF, "PATHCONF"),
        (proc::COMMIT, "COMMIT"),
    ];
    assert_eq!(registry.len(), 22, "RFC 1813 §3.0: 22 procedures total");
    // Numbers must be a contiguous 0..=21 range — no gaps, no
    // re-orderings allowed by the spec.
    for (i, (n, name)) in registry.iter().enumerate() {
        assert_eq!(
            *n, i as u32,
            "RFC 1813 §3.3.{i} ({name}) must have procedure number {i}"
        );
    }
}

// ===========================================================================
// Helpers — build calls / fixtures shared across procedure tests
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
    let gw = InMemoryGateway::new(compositions, Box::new(chunks), master_key);
    let nfs_gw = NfsGateway::new(gw);
    NfsContext::new(nfs_gw, test_tenant(), test_namespace())
}

/// Build the full ONC-RPC v2 call frame for a given NFSv3 procedure.
/// Per RFC 5531 §9 the header layout is:
/// `xid, msg_type=CALL(0), rpc_version=2, program, version, procedure,
///  cred(opaque_auth), verf(opaque_auth)`. Procedure args (`proc_body`)
/// follow.
fn build_nfs3_call(xid: u32, procedure: u32, proc_body: &[u8]) -> Vec<u8> {
    let mut w = XdrWriter::new();
    w.write_u32(xid);
    w.write_u32(0); // CALL
    w.write_u32(2); // RPC version
    w.write_u32(NFS3_PROGRAM);
    w.write_u32(NFS3_VERSION);
    w.write_u32(procedure);
    // AUTH_NONE creds + verifier per RFC 1057 §9.1.
    w.write_u32(0);
    w.write_opaque(&[]);
    w.write_u32(0);
    w.write_opaque(&[]);
    let mut buf = w.into_bytes();
    buf.extend_from_slice(proc_body);
    buf
}

fn make_header(xid: u32, procedure: u32) -> RpcCallHeader {
    RpcCallHeader {
        xid,
        program: NFS3_PROGRAM,
        version: NFS3_VERSION,
        procedure,
    }
}

/// Skip the ONC-RPC accepted-reply preamble and return a reader
/// positioned at the NFS3 procedure-result body. Layout per RFC 5531:
/// `xid(4) + REPLY(4) + MSG_ACCEPTED(4) + verf_flavor(4) +
///  verf_body_len(4) + accept_stat(4)` = 24 bytes.
fn reader_at_proc_result(reply: &[u8]) -> XdrReader<'_> {
    let mut r = XdrReader::new(reply);
    let xid = r.read_u32().expect("xid");
    let _ = xid;
    let _msg_type = r.read_u32().expect("msg_type=REPLY");
    let _reply_stat = r.read_u32().expect("reply_stat=MSG_ACCEPTED");
    let _vf = r.read_u32().expect("verf flavor");
    let _vlen = r.read_u32().expect("verf length");
    let accept_stat = r.read_u32().expect("accept_stat");
    assert_eq!(
        accept_stat, 0,
        "RFC 5531: dispatch should produce MSG_ACCEPTED+SUCCESS"
    );
    r
}

// ===========================================================================
// §3.3.0 — NULL
// ===========================================================================

/// RFC 1813 §3.3.0 — NULL: takes no args, returns no body. The reply
/// MUST be a bare `MSG_ACCEPTED + SUCCESS` with no procedure result
/// bytes following.
#[test]
fn s3_3_0_null_returns_empty_body() {
    let ctx = make_ctx();
    let xid = 0x0000_0001;
    let header = make_header(xid, proc::NULL);
    let raw = build_nfs3_call(xid, proc::NULL, &[]);

    let reply = handle_nfs3_first_message(&header, &raw, &ctx);
    let mut r = XdrReader::new(&reply);
    assert_eq!(r.read_u32().unwrap(), xid, "xid echoed");
    let _msg_type = r.read_u32().unwrap();
    let _reply_stat = r.read_u32().unwrap();
    let _vf = r.read_u32().unwrap();
    let _vlen = r.read_u32().unwrap();
    let accept_stat = r.read_u32().unwrap();
    assert_eq!(accept_stat, 0, "RFC 1813 §3.3.0: NULL must yield ACCEPT_OK");
    assert_eq!(
        r.remaining(),
        0,
        "RFC 1813 §3.3.0: NULL reply MUST have empty result body"
    );
}

// ===========================================================================
// §3.3.1 — GETATTR
// ===========================================================================

/// RFC 1813 §3.3.1 — GETATTR: input `nfs_fh3` (32-byte handle),
/// output on success is `NFS3_OK + fattr3`. `fattr3` is a fixed-shape
/// 21-u32-equivalent struct (with three nfstime3 trios at the tail).
#[test]
fn s3_3_1_getattr_root_handle_returns_ok_and_directory_fattr3() {
    let ctx = make_ctx();
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
    let mut body = XdrWriter::new();
    body.write_opaque(&root_fh);
    let raw = build_nfs3_call(7, proc::GETATTR, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(7, proc::GETATTR), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3_OK,
        "RFC 1813 §3.3.1: root GETATTR must succeed"
    );
    let ftype = r.read_u32().expect("ftype");
    // RFC 1813 §2.5 — `ftype3` enum: NF3DIR = 2.
    assert_eq!(ftype, 2, "RFC 1813 §2.5: root file type MUST be NF3DIR (2)");
}

/// RFC 1813 §3.3.1 — negative path. Per §2.6 a malformed handle
/// (length != 32 octets in our concrete fh3 shape) MUST be rejected
/// with `NFS3ERR_BADHANDLE` (10001).
#[test]
fn s3_3_1_getattr_short_handle_returns_badhandle() {
    let ctx = make_ctx();
    let mut body = XdrWriter::new();
    body.write_opaque(&[0xDEu8, 0xAD]); // 2 bytes, not 32
    let raw = build_nfs3_call(11, proc::GETATTR, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(11, proc::GETATTR), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3ERR_BADHANDLE,
        "RFC 1813 §3.3.1 + §2.6: malformed nfs_fh3 MUST yield NFS3ERR_BADHANDLE"
    );
}

// ===========================================================================
// §3.3.3 — LOOKUP
// ===========================================================================

/// RFC 1813 §3.3.3 — LOOKUP(dir_fh, name): on miss returns
/// `NFS3ERR_NOENT` (2) followed by the `post_op_attr` for the
/// directory.
#[test]
fn s3_3_3_lookup_missing_name_returns_noent() {
    let ctx = make_ctx();
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
    let mut body = XdrWriter::new();
    body.write_opaque(&root_fh);
    body.write_string("does-not-exist.txt");
    let raw = build_nfs3_call(20, proc::LOOKUP, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(20, proc::LOOKUP), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3ERR_NOENT,
        "RFC 1813 §3.3.3: lookup of non-existent name MUST yield NFS3ERR_NOENT"
    );
}

// ===========================================================================
// §3.3.6 — READ
// ===========================================================================

/// RFC 1813 §3.3.6 — READ on a never-registered 32-byte handle MUST
/// surface as either NFS3ERR_BADHANDLE (handle unknown) or
/// NFS3ERR_IO (handle decode succeeded but the lookup failed). A
/// strict server prefers NFS3ERR_BADHANDLE per §2.6's classification.
#[test]
fn s3_3_6_read_unknown_handle_yields_io_or_badhandle() {
    let ctx = make_ctx();
    let mut body = XdrWriter::new();
    body.write_opaque(&[0x55u8; 32]); // never-registered 32-byte handle
    body.write_u64(0); // offset
    body.write_u32(4096); // count
    let raw = build_nfs3_call(33, proc::READ, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(33, proc::READ), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert!(
        nfs_status == status::NFS3ERR_IO || nfs_status == status::NFS3ERR_BADHANDLE,
        "RFC 1813 §3.3.6: READ on unknown handle MUST yield IO or BADHANDLE; got {nfs_status}"
    );
    // Strict reading of §2.6: the spec's preferred error here is
    // BADHANDLE (the handle is not recognized). Today's server returns
    // NFS3ERR_IO — flag as a fidelity gap.
    assert_eq!(
        nfs_status,
        status::NFS3ERR_BADHANDLE,
        "RFC 1813 §2.6: unknown handle is BADHANDLE, not IO"
    );
}

// ===========================================================================
// §3.3.7 — WRITE
// ===========================================================================

/// RFC 1813 §3.3.7 — WRITE(file_fh, offset, count, stable, data):
/// successful write returns `NFS3_OK` then `wcc_data` (pre+post) then
/// `count4`, `stable_how`, `writeverf3(8)`. Kiseki only supports
/// offset=0 (immutable composition); a full WRITE happy path:
/// CREATE → LOOKUP → WRITE @ offset 0.
#[test]
fn s3_3_7_write_at_offset_zero_returns_ok_and_count() {
    let ctx = make_ctx();
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);

    // CREATE first.
    let mut body = XdrWriter::new();
    body.write_opaque(&root_fh);
    body.write_string("layer1-write.txt");
    let raw = build_nfs3_call(50, proc::CREATE, &body.into_bytes());
    let _ = handle_nfs3_first_message(&make_header(50, proc::CREATE), &raw, &ctx);

    // LOOKUP returns the file handle.
    let (file_fh, _) = ctx
        .lookup_by_name("layer1-write.txt")
        .expect("CREATE must have registered the handle");

    // WRITE with stable_how = FILE_SYNC (2) per RFC 1813 §3.3.7.
    let payload = b"layer-1 wire bytes";
    let mut body = XdrWriter::new();
    body.write_opaque(&file_fh);
    body.write_u64(0); // offset
    body.write_u32(payload.len() as u32);
    body.write_u32(2); // FILE_SYNC
    body.write_opaque(payload);
    let raw = build_nfs3_call(51, proc::WRITE, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(51, proc::WRITE), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(nfs_status, status::NFS3_OK, "RFC 1813 §3.3.7: WRITE OK");
    let _pre = r.read_bool().unwrap();
    let _post = r.read_bool().unwrap();
    let count = r.read_u32().unwrap();
    assert_eq!(
        count,
        payload.len() as u32,
        "RFC 1813 §3.3.7: count4 MUST equal bytes written"
    );
    let committed = r.read_u32().unwrap();
    assert_eq!(
        committed, 2,
        "RFC 1813 §3.3.7: stable_how echo MUST be FILE_SYNC (2)"
    );
}

// ===========================================================================
// §3.3.8 — CREATE
// ===========================================================================

/// RFC 1813 §3.3.8 — CREATE(dir_fh, name, createhow): on success
/// returns `NFS3_OK + post_op_fh3{handle_follows=TRUE, fh3} +
/// post_op_attr + wcc_data`.
#[test]
fn s3_3_8_create_returns_ok_with_handle() {
    let ctx = make_ctx();
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
    let mut body = XdrWriter::new();
    body.write_opaque(&root_fh);
    body.write_string("layer1-create.txt");
    let raw = build_nfs3_call(60, proc::CREATE, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(60, proc::CREATE), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3_OK,
        "RFC 1813 §3.3.8: CREATE happy path"
    );
    let handle_follows = r.read_bool().unwrap();
    assert!(
        handle_follows,
        "RFC 1813 §3.3.8: post_op_fh3.handle_follows MUST be TRUE"
    );
    let fh = r.read_opaque().expect("fh3 follows");
    assert_eq!(
        fh.len(),
        32,
        "RFC 1813 §3.3.8 + §2.5: NFSv3 file handle is 32 octets"
    );
}

// ===========================================================================
// §3.3.12 — REMOVE
// ===========================================================================

/// RFC 1813 §3.3.12 — REMOVE on a missing name MUST return
/// `NFS3ERR_NOENT`. (One of the wire-side NFS3ERR_* negatives the
/// catalog calls out.)
#[test]
fn s3_3_12_remove_missing_name_returns_noent() {
    let ctx = make_ctx();
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
    let mut body = XdrWriter::new();
    body.write_opaque(&root_fh);
    body.write_string("ghost.txt");
    let raw = build_nfs3_call(70, proc::REMOVE, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(70, proc::REMOVE), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3ERR_NOENT,
        "RFC 1813 §3.3.12: REMOVE of nonexistent name MUST yield NFS3ERR_NOENT"
    );
}

// ===========================================================================
// §3.3.16 — READDIR
// ===========================================================================

/// RFC 1813 §3.3.16 — READDIR(dir_fh, cookie, cookieverf, count):
/// success returns `NFS3_OK + post_op_attr + cookieverf3(8) + entries
/// + eof`. We only cover the minimal contract here: status==OK and
/// the 8-byte cookieverf is present.
#[test]
fn s3_3_16_readdir_root_returns_ok_with_cookieverf() {
    let ctx = make_ctx();
    let root_fh = ctx.handles.root_handle(ctx.namespace_id, ctx.tenant_id);
    let mut body = XdrWriter::new();
    body.write_opaque(&root_fh);
    let raw = build_nfs3_call(80, proc::READDIR, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(80, proc::READDIR), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3_OK,
        "RFC 1813 §3.3.16: READDIR root happy path"
    );
    let _post_op_attr = r.read_bool().unwrap();
    let cookieverf = r.read_opaque_fixed(8).expect("cookieverf3 is 8 bytes");
    assert_eq!(
        cookieverf.len(),
        8,
        "RFC 1813 §2.5: cookieverf3 is fixed 8 octets"
    );
}

// ===========================================================================
// Wire-level error: NFS3ERR_IO via offset-violating WRITE
// ===========================================================================

/// RFC 1813 §3.3.7 — kiseki's compositions are immutable. Any WRITE
/// with `offset != 0` on a file handle MUST surface as `NFS3ERR_IO`
/// (RFC 1813 §2.6). Negative test for the IO error code.
#[test]
fn s3_3_7_write_at_nonzero_offset_returns_nfs3err_io() {
    let ctx = make_ctx();
    let mut body = XdrWriter::new();
    body.write_opaque(&[0xBBu8; 32]); // 32-byte handle (any value)
    body.write_u64(100); // nonzero offset
    body.write_u32(3);
    body.write_u32(2);
    body.write_opaque(b"abc");
    let raw = build_nfs3_call(91, proc::WRITE, &body.into_bytes());

    let reply = handle_nfs3_first_message(&make_header(91, proc::WRITE), &raw, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert_eq!(
        nfs_status,
        status::NFS3ERR_IO,
        "RFC 1813 §3.3.7: WRITE @ nonzero offset on immutable composition MUST yield NFS3ERR_IO"
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 1813 §A canonical GETATTR exchange
// ===========================================================================

/// RFC 1813 does not embed verbatim hex examples like RFC 4506. Every
/// real GETATTR call from a Linux `mount.nfs -o vers=3` client is the
/// canonical seed: an ONC-RPC v2 CALL frame with `program=100003`,
/// `version=3`, `procedure=1`, AUTH_UNIX/AUTH_NONE creds, and a
/// 32-byte file handle as the only argument.
///
/// This test pins the byte layout of the call frame our code MUST
/// accept and compares it against the field-by-field sentinels. The
/// values below match what `nfs-utils` emits for the trivial root
/// GETATTR path (xid is per-client; we pick `0xCAFE_BABE` for the
/// fixture).
#[test]
fn rfc_1813_seed_canonical_getattr_call_frame_decodes() {
    // 32-byte file handle — all 0xAB bytes for a deterministic seed.
    let fh = [0xABu8; 32];

    // Build the call frame manually so the test pins the byte
    // sequence rather than relying on the helper.
    let mut w = XdrWriter::new();
    w.write_u32(0xCAFE_BABE); // xid
    w.write_u32(0); // CALL
    w.write_u32(2); // RPC version
    w.write_u32(NFS3_PROGRAM); // 100003
    w.write_u32(NFS3_VERSION); // 3
    w.write_u32(proc::GETATTR); // 1
                                // AUTH_NONE creds + verifier (RFC 1057 §9.1).
    w.write_u32(0);
    w.write_opaque(&[]);
    w.write_u32(0);
    w.write_opaque(&[]);
    // Procedure body: one nfs_fh3 (32-byte opaque).
    w.write_opaque(&fh);
    let frame = w.into_bytes();

    // The first 24 bytes are: xid, CALL, rpc_v2, program, version,
    // procedure — all 4 bytes each. Pin them.
    assert_eq!(&frame[0..4], &0xCAFE_BABE_u32.to_be_bytes());
    assert_eq!(&frame[4..8], &0u32.to_be_bytes(), "msg_type CALL = 0");
    assert_eq!(&frame[8..12], &2u32.to_be_bytes(), "RPC v2 = 2");
    assert_eq!(
        &frame[12..16],
        &NFS3_PROGRAM.to_be_bytes(),
        "RFC 1813 §1.2: program = 100003"
    );
    assert_eq!(
        &frame[16..20],
        &NFS3_VERSION.to_be_bytes(),
        "RFC 1813 §1.2: version = 3"
    );
    assert_eq!(
        &frame[20..24],
        &proc::GETATTR.to_be_bytes(),
        "RFC 1813 §3.3.1: procedure = 1 (GETATTR)"
    );

    // Now feed the frame through the kiseki dispatcher and confirm
    // the reply parses as a v3 GETATTR error (the all-0xAB handle is
    // not registered) — the wire path is exercised end-to-end.
    let ctx = make_ctx();
    let header = make_header(0xCAFE_BABE, proc::GETATTR);
    let reply = handle_nfs3_first_message(&header, &frame, &ctx);
    let mut r = reader_at_proc_result(&reply);
    let nfs_status = r.read_u32().expect("nfs_status");
    assert!(
        nfs_status == status::NFS3ERR_BADHANDLE
            || nfs_status == status::NFS3ERR_NOENT
            || nfs_status == status::NFS3ERR_IO,
        "RFC 1813 §3.3.1: unregistered handle MUST yield BADHANDLE / NOENT / IO; got {nfs_status}"
    );
    // Strict reading of §2.6: the canonical answer here is BADHANDLE.
    assert_eq!(
        nfs_status,
        status::NFS3ERR_BADHANDLE,
        "RFC 1813 §2.6: unknown 32-byte handle MUST be NFS3ERR_BADHANDLE"
    );
}
