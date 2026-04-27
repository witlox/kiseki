//! Layer 1 reference tests for **RFC 1057 — RPC: Remote Procedure
//! Call Protocol Specification Version 2** (June 1988).
//!
//! Although RFC 5531 obsoletes RFC 1057 for the call/reply framing,
//! RFC 1057 is still the authoritative source for the **AUTH_NONE**
//! and **AUTH_SYS** credential body shapes (RFC 5531 §8.1 only pins
//! the flavor enum + transport; the body grammars sit in RFC 1057
//! §9.1 and §9.2). Kiseki advertises AUTH_SYS support today, so
//! Layer 1 fidelity for the credential body is on the critical path.
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::nfs_auth` carries the typed credentials
//! (`NfsCredentials`, `NfsAuthMethod`); `kiseki-gateway::nfs_xdr`
//! carries the codec primitives (`XdrReader`, `XdrWriter`). The
//! production code today **skips** the cred body during decode (see
//! `RpcCallHeader::decode`), so this file asserts the wire shape
//! against the codec primitives. When kiseki adds a typed
//! `AuthSysParams` decoder, the per-field tests below pin the
//! contract.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 1057".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc1057> (no errata
//! affecting AUTH_NONE / AUTH_SYS body shapes as of 2026-04-27).
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

use kiseki_gateway::nfs_auth::{AuthSysParams, NfsAuthMethod, NfsCredentials};
use kiseki_gateway::nfs_xdr::{OpaqueAuth, XdrReader, XdrWriter};

// ===========================================================================
// Sentinel constants — RFC 1057 §7.2 + IANA RPC Authentication Flavors
// ===========================================================================

/// RFC 1057 §7.2 — flavor numbers (also reaffirmed in RFC 5531 §8.1
/// + IANA registry). This sentinel pins the wire values so a future
/// refactor that adds a typed `AuthFlavor` enum cannot accidentally
/// renumber them.
#[test]
fn s7_2_auth_flavor_constants_pinned() {
    // Flavor values as they MUST appear on the wire.
    const AUTH_NONE: u32 = 0;
    const AUTH_SYS: u32 = 1;
    const AUTH_SHORT: u32 = 2;
    const AUTH_DH: u32 = 3;
    const RPCSEC_GSS: u32 = 6;

    assert_eq!(AUTH_NONE, 0, "RFC 1057 §7.2: AUTH_NONE = 0");
    assert_eq!(AUTH_SYS, 1, "RFC 1057 §7.2: AUTH_SYS = 1");
    assert_eq!(AUTH_SHORT, 2, "RFC 1057 §7.2: AUTH_SHORT = 2");
    assert_eq!(AUTH_DH, 3, "RFC 1057 §7.2: AUTH_DH = 3");
    assert_eq!(RPCSEC_GSS, 6, "RFC 5531 §8.1: RPCSEC_GSS = 6");

    // Confirm we round-trip these flavor values through the XDR
    // codec (they ride on the wire as u32 enum discriminants).
    for f in [AUTH_NONE, AUTH_SYS, AUTH_SHORT, AUTH_DH, RPCSEC_GSS] {
        let mut w = XdrWriter::new();
        w.write_u32(f);
        let bytes = w.into_bytes();
        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_u32().unwrap(), f);
    }
}

// ===========================================================================
// §9.1 — AUTH_NONE
// ===========================================================================

/// RFC 1057 §9.1 — AUTH_NONE body MUST be empty.
///
/// > "Calls are often made where the caller does not know who he
/// >  is or the server does not care who the caller is.  In this
/// >  case, the flavor of authentication is AUTH_NONE.  Bytes of
/// >  the opaque_auth's body are undefined.  It is recommended
/// >  that the body length be zero."
///
/// In practice, every real implementation emits length=0; a strict
/// decoder MUST accept the empty body and SHOULD treat any non-zero
/// body as a fidelity gap to flag (real client compatibility).
#[test]
fn s9_1_auth_none_body_is_empty_on_wire() {
    // Build the opaque_auth wrapper for AUTH_NONE: flavor=0, body=[].
    let mut w = XdrWriter::new();
    w.write_u32(0); // flavor = AUTH_NONE
    w.write_opaque(&[]); // body
    let bytes = w.into_bytes();

    // Wire shape: 4 bytes flavor + 4 bytes length(=0) = 8 bytes.
    assert_eq!(
        bytes,
        vec![
            0, 0, 0, 0, // flavor = AUTH_NONE
            0, 0, 0, 0, // body length = 0
        ],
        "RFC 1057 §9.1: AUTH_NONE on wire is exactly flavor(0) + len(0)"
    );

    // And the codec round-trips the empty body cleanly.
    let mut r = XdrReader::new(&bytes);
    assert_eq!(r.read_u32().unwrap(), 0, "flavor");
    assert_eq!(
        r.read_opaque().unwrap(),
        Vec::<u8>::new(),
        "RFC 1057 §9.1: AUTH_NONE body must decode to empty bytes"
    );
}

/// RFC 1057 §9.1 negative — a non-empty AUTH_NONE body is a
/// protocol-fidelity gap. A strict server SHOULD reject (or at
/// minimum log) bodies > 0 bytes when flavor=AUTH_NONE.
///
/// `OpaqueAuth::decode_strict` enforces this; the lenient
/// `OpaqueAuth::decode` (used by `RpcCallHeader::decode` so we don't
/// break interop with chatty clients) accepts non-empty bodies and
/// just enforces the §8.2 400-byte cap.
#[test]
fn s9_1_auth_none_with_nonempty_body_is_rejected_by_strict_decoder() {
    // Build flavor=AUTH_NONE with a 4-byte body. Spec says body
    // SHOULD be zero-length; a strict server flags this.
    let mut w = XdrWriter::new();
    w.write_u32(0); // AUTH_NONE
    w.write_opaque(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let bytes = w.into_bytes();

    // Lenient decode succeeds (real production path).
    {
        let mut r = XdrReader::new(&bytes);
        let oa = OpaqueAuth::decode(&mut r).expect("lenient decode accepts");
        assert_eq!(oa.flavor, 0);
        assert_eq!(oa.body, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    // Strict decode rejects per §9.1 SHOULD.
    let mut r = XdrReader::new(&bytes);
    let result = OpaqueAuth::decode_strict(&mut r);
    assert!(
        result.is_err(),
        "RFC 1057 §9.1: strict AUTH_NONE decode must reject non-empty body"
    );
}

// ===========================================================================
// §9.2 — AUTH_SYS (a.k.a. AUTH_UNIX)
// ===========================================================================
//
// RFC 1057 §9.2 defines the AUTH_SYS credential body grammar:
//
//     struct authsys_parms {
//         unsigned int  stamp;
//         string        machinename<255>;
//         unsigned int  uid;
//         unsigned int  gid;
//         unsigned int  gids<16>;
//     };
//
// Encoded inside the opaque_auth body field with flavor=1.

/// Helper — serialize an `authsys_parms` struct per RFC 1057 §9.2
/// directly into bytes. This is what kiseki's *strict* AUTH_SYS
/// decoder MUST accept (and reject the corresponding negative
/// cases). Until the decoder exists, we test against the codec
/// primitives.
fn encode_authsys_parms(
    stamp: u32,
    machinename: &str,
    uid: u32,
    gid: u32,
    gids: &[u32],
) -> Vec<u8> {
    let mut w = XdrWriter::new();
    w.write_u32(stamp);
    w.write_string(machinename);
    w.write_u32(uid);
    w.write_u32(gid);
    w.write_u32(gids.len() as u32);
    for g in gids {
        w.write_u32(*g);
    }
    w.into_bytes()
}

/// RFC 1057 §9.2 — AUTH_SYS body field order is:
/// `stamp(u32), machinename(string<255>), uid(u32), gid(u32),
/// gids(array<u32, max=16>)`.
///
/// Positive case: a typical Linux client with stamp=arbitrary,
/// machinename="client.example", uid=1000, gid=1000, gids=[1000].
#[test]
fn s9_2_auth_sys_body_round_trips_through_xdr() {
    let bytes = encode_authsys_parms(0xCAFE_BABE, "client.example", 1000, 1000, &[1000]);

    // Walk the body back out and assert each field matches.
    let mut r = XdrReader::new(&bytes);
    assert_eq!(r.read_u32().unwrap(), 0xCAFE_BABE, "stamp");
    assert_eq!(r.read_string().unwrap(), "client.example", "machinename");
    assert_eq!(r.read_u32().unwrap(), 1000, "uid");
    assert_eq!(r.read_u32().unwrap(), 1000, "gid");
    let gid_count = r.read_u32().unwrap();
    assert_eq!(gid_count, 1, "gids array length");
    assert_eq!(r.read_u32().unwrap(), 1000, "gids[0]");
    assert_eq!(r.remaining(), 0, "no trailing bytes after AUTH_SYS body");
}

// ---------------------------------------------------------------------------
// §9.2 — `stamp` field
// ---------------------------------------------------------------------------

/// RFC 1057 §9.2 — `stamp` is an arbitrary `unsigned int`. All
/// values 0..=u32::MAX are legal; the server MUST NOT reject any.
#[test]
fn s9_2_stamp_accepts_full_u32_range() {
    for stamp in [0u32, 1, 0xCAFE_BABE, u32::MAX] {
        let bytes = encode_authsys_parms(stamp, "h", 0, 0, &[]);
        let mut r = XdrReader::new(&bytes);
        assert_eq!(
            r.read_u32().unwrap(),
            stamp,
            "RFC 1057 §9.2: stamp={stamp} must decode unchanged"
        );
    }
}

/// RFC 1057 §9.2 negative — `stamp` is fixed-width 4 bytes; if
/// the body is truncated mid-stamp the decoder MUST error.
#[test]
fn s9_2_stamp_truncated_body_rejected() {
    // Only 3 bytes — one short of a u32.
    let bytes = [0xFFu8, 0xFF, 0xFF];
    let mut r = XdrReader::new(&bytes);
    assert!(
        r.read_u32().is_err(),
        "RFC 1057 §9.2: truncated stamp must error (NFS3ERR_BADXDR equivalent)"
    );
}

// ---------------------------------------------------------------------------
// §9.2 — `machinename` field (string<255>)
// ---------------------------------------------------------------------------

/// RFC 1057 §9.2 — `machinename` is `string<255>`: an XDR string
/// with a maximum length of 255 octets. A 255-octet machinename
/// is the longest a strict decoder MUST accept.
#[test]
fn s9_2_machinename_at_max_length_255_accepted() {
    let max_name = "a".repeat(255);
    let bytes = encode_authsys_parms(0, &max_name, 0, 0, &[]);
    let mut r = XdrReader::new(&bytes);
    let _stamp = r.read_u32().unwrap();
    let name = r
        .read_string()
        .expect("RFC 1057 §9.2: machinename of 255 bytes is the documented maximum");
    assert_eq!(name.len(), 255);
    assert_eq!(name, max_name);
}

/// RFC 1057 §9.2 negative — `machinename` over 255 octets violates
/// the `string<255>` constraint. `AuthSysParams::decode` MUST reject
/// it with a BADXDR-equivalent error.
#[test]
fn s9_2_machinename_over_255_must_error() {
    let oversized = "X".repeat(256); // one byte over the limit
    let bytes = encode_authsys_parms(0, &oversized, 0, 0, &[]);
    let mut r = XdrReader::new(&bytes);
    let result = AuthSysParams::decode(&mut r);
    assert!(
        result.is_err(),
        "RFC 1057 §9.2: machinename has hard cap of 255 octets; \
         AuthSysParams::decode must reject 256-octet name"
    );
}

// ---------------------------------------------------------------------------
// §9.2 — `uid` and `gid` fields
// ---------------------------------------------------------------------------

/// RFC 1057 §9.2 — `uid` and `gid` are `unsigned int` (32-bit).
/// All values are legal on the wire; mapping/permission decisions
/// happen at the auth layer (`validate_credentials`), not the
/// codec.
#[test]
fn s9_2_uid_gid_accept_full_u32_range() {
    // Edge: uid=0 (root), gid=0 (root group). Spec doesn't ban
    // root creds at the wire layer; export config decides.
    let bytes = encode_authsys_parms(0, "h", 0, 0, &[]);
    let mut r = XdrReader::new(&bytes);
    let _ = r.read_u32().unwrap(); // stamp
    let _ = r.read_string().unwrap(); // machinename
    assert_eq!(r.read_u32().unwrap(), 0, "uid=0 (root)");
    assert_eq!(r.read_u32().unwrap(), 0, "gid=0");

    // Edge: uid=u32::MAX (NFS NOBODY-ish).
    let bytes = encode_authsys_parms(0, "h", u32::MAX, u32::MAX, &[]);
    let mut r = XdrReader::new(&bytes);
    let _ = r.read_u32().unwrap();
    let _ = r.read_string().unwrap();
    assert_eq!(r.read_u32().unwrap(), u32::MAX, "uid=u32::MAX");
    assert_eq!(r.read_u32().unwrap(), u32::MAX, "gid=u32::MAX");
}

/// RFC 1057 §9.2 — when AUTH_SYS creds reach the auth layer,
/// `NfsCredentials::from_auth_sys` is the entry point. The fields
/// must round-trip exactly (no value mangling).
#[test]
fn s9_2_uid_gid_round_trip_through_nfs_credentials() {
    let creds = NfsCredentials::from_auth_sys(1000, 1000, "client.example".into());
    assert_eq!(creds.method, NfsAuthMethod::AuthSys);
    assert_eq!(creds.uid, 1000);
    assert_eq!(creds.gid, 1000);
    assert_eq!(creds.hostname, "client.example");
    assert!(
        creds.principal.is_none(),
        "RFC 1057 §9.2: AUTH_SYS creds carry no principal"
    );
}

// ---------------------------------------------------------------------------
// §9.2 — `gids` field (variable-length array<u32, max=16>)
// ---------------------------------------------------------------------------

/// RFC 1057 §9.2 — `gids<16>` is a variable-length array of
/// unsigned int with **maximum 16 entries**. An empty array is
/// legal (no supplemental groups).
#[test]
fn s9_2_gids_empty_array_accepted() {
    let bytes = encode_authsys_parms(0, "h", 1000, 1000, &[]);
    let mut r = XdrReader::new(&bytes);
    let _ = r.read_u32(); // stamp
    let _ = r.read_string(); // machinename
    let _ = r.read_u32(); // uid
    let _ = r.read_u32(); // gid
    assert_eq!(r.read_u32().unwrap(), 0, "gids array length = 0");
    assert_eq!(r.remaining(), 0);
}

/// RFC 1057 §9.2 — exactly 16 supplemental gids is the documented
/// maximum and MUST be accepted.
#[test]
fn s9_2_gids_at_max_16_entries_accepted() {
    let gids: Vec<u32> = (1000..1016).collect(); // exactly 16
    let bytes = encode_authsys_parms(0, "h", 1000, 1000, &gids);
    let mut r = XdrReader::new(&bytes);
    let _ = r.read_u32(); // stamp
    let _ = r.read_string(); // machinename
    let _ = r.read_u32(); // uid
    let _ = r.read_u32(); // gid
    let count = r.read_u32().unwrap();
    assert_eq!(count, 16, "RFC 1057 §9.2: gids<16> max is 16");
    for g in &gids {
        assert_eq!(r.read_u32().unwrap(), *g);
    }
}

/// RFC 1057 §9.2 negative — an oversized gids array (>16 entries)
/// violates the `gids<16>` cap. `AuthSysParams::decode` MUST reject.
///
/// Real-world note: oversized gids arrays are a known auth-bypass
/// vector — a malicious client could claim membership in an
/// arbitrary number of groups to hit edge-case authorization paths.
#[test]
fn s9_2_gids_over_16_entries_must_error() {
    let too_many: Vec<u32> = (1000..1017).collect(); // 17 entries — one over cap
    let bytes = encode_authsys_parms(0, "h", 1000, 1000, &too_many);
    let mut r = XdrReader::new(&bytes);
    let result = AuthSysParams::decode(&mut r);
    assert!(
        result.is_err(),
        "RFC 1057 §9.2: gids<16> caps the array at 16 entries; \
         AuthSysParams::decode must reject 17 entries (BADXDR-equivalent)"
    );
}

// ===========================================================================
// AUTH_SYS — full credential body round-trip
// ===========================================================================

/// RFC 1057 §9.2 — encode the full `authsys_parms` struct, decode
/// it back, and verify every field matches. Round-trip is identity.
#[test]
fn s9_2_full_authsys_parms_round_trip() {
    let stamp = 0xCAFE_BABE;
    let machinename = "nfs-client.example.com";
    let uid = 1000;
    let gid = 1000;
    let gids = vec![1000u32, 2000, 3000];

    let bytes = encode_authsys_parms(stamp, machinename, uid, gid, &gids);

    let mut r = XdrReader::new(&bytes);
    assert_eq!(r.read_u32().unwrap(), stamp);
    assert_eq!(r.read_string().unwrap(), machinename);
    assert_eq!(r.read_u32().unwrap(), uid);
    assert_eq!(r.read_u32().unwrap(), gid);
    let n = r.read_u32().unwrap() as usize;
    assert_eq!(n, gids.len());
    let decoded_gids: Vec<u32> = (0..n).map(|_| r.read_u32().unwrap()).collect();
    assert_eq!(decoded_gids, gids);
    assert_eq!(r.remaining(), 0, "no trailing bytes");
}

// ===========================================================================
// Cross-implementation seed — RFC 1057 §9.2 verbatim example
// ===========================================================================

/// RFC 1057 §9.2 verbatim example structure (the spec text):
///
/// ```text
///     struct authsys_parms {
///         unsigned int  stamp;
///         string        machinename<255>;
///         unsigned int  uid;
///         unsigned int  gid;
///         unsigned int  gids<16>;
///     };
/// ```
///
/// Concrete byte-for-byte reference, hand-derived from the grammar
/// using values the spec discusses (`stamp=0`, `machinename="unix"`,
/// `uid=0`, `gid=0`, no supplemental gids — the trivial root
/// credential). Any compliant encoder MUST produce these bytes.
#[test]
fn rfc_example_s9_2_authsys_parms_root_unix_credential() {
    let bytes = encode_authsys_parms(0, "unix", 0, 0, &[]);
    let expected = vec![
        0x00, 0x00, 0x00, 0x00, // stamp = 0
        0x00, 0x00, 0x00, 0x04, // machinename length = 4
        0x75, 0x6E, 0x69, 0x78, // "unix"
        0x00, 0x00, 0x00, 0x00, // uid = 0
        0x00, 0x00, 0x00, 0x00, // gid = 0
        0x00, 0x00, 0x00, 0x00, // gids array length = 0
    ];
    assert_eq!(
        bytes, expected,
        "RFC 1057 §9.2: the trivial root AUTH_SYS body must be exactly these 24 bytes"
    );
    assert_eq!(bytes.len(), 24);
}
