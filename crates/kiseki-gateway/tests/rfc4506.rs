//! Layer 1 reference tests for **RFC 4506 — XDR: External Data
//! Representation Standard** (May 2006).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::nfs_xdr` exposes the XDR codec primitives
//! kiseki uses everywhere it talks NFS. These tests assert that
//! codec against the RFC byte-for-byte.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 4506".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc4506> (no errata
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
    clippy::unicode_not_nfc
)]

use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};

// ===========================================================================
// §3 — primitive types
// ===========================================================================

/// RFC 4506 §3 — every XDR primitive is encoded in a multiple of
/// four bytes (the "block size"). The type-specific sub-sections
/// pin the exact width per type.
#[test]
fn s3_block_size_is_four_bytes_for_all_primitives() {
    // u32 (§4.2 Unsigned Integer)
    let mut w = XdrWriter::new();
    w.write_u32(0xDEAD_BEEF);
    assert_eq!(w.into_bytes().len(), 4);

    // u64 / i64 (§4.5 Hyper Integer / Unsigned Hyper Integer)
    let mut w = XdrWriter::new();
    w.write_u64(0x0123_4567_89AB_CDEF);
    assert_eq!(w.into_bytes().len(), 8);

    // bool (§4.4) — encoded as enum, four bytes.
    let mut w = XdrWriter::new();
    w.write_bool(true);
    assert_eq!(w.into_bytes().len(), 4);
    let mut w = XdrWriter::new();
    w.write_bool(false);
    assert_eq!(w.into_bytes().len(), 4);
}

// ===========================================================================
// §4.1 — Integer (signed)
// ===========================================================================

/// RFC 4506 §4.1: a signed integer is encoded as four bytes in
/// **two's complement, big-endian**.
#[test]
fn s4_1_signed_integer_two_complement_big_endian() {
    let mut w = XdrWriter::new();
    w.write_i32(-1);
    assert_eq!(w.into_bytes(), vec![0xFF, 0xFF, 0xFF, 0xFF]);

    let mut w = XdrWriter::new();
    w.write_i32(1);
    assert_eq!(w.into_bytes(), vec![0x00, 0x00, 0x00, 0x01]);

    let mut w = XdrWriter::new();
    w.write_i32(i32::MIN);
    assert_eq!(w.into_bytes(), vec![0x80, 0x00, 0x00, 0x00]);

    let mut w = XdrWriter::new();
    w.write_i32(i32::MAX);
    assert_eq!(w.into_bytes(), vec![0x7F, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn s4_1_signed_integer_round_trip() {
    for v in [-1, 0, 1, i32::MIN, i32::MAX, -42, 12345] {
        let mut w = XdrWriter::new();
        w.write_i32(v);
        let bytes = w.into_bytes();
        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_i32().unwrap(), v);
        assert_eq!(r.remaining(), 0);
    }
}

// ===========================================================================
// §4.2 — Unsigned Integer
// ===========================================================================

/// RFC 4506 §4.2 — unsigned 32-bit, big-endian.
#[test]
fn s4_2_unsigned_integer_big_endian() {
    let mut w = XdrWriter::new();
    w.write_u32(0xDEAD_BEEF);
    assert_eq!(w.into_bytes(), vec![0xDE, 0xAD, 0xBE, 0xEF]);

    let mut w = XdrWriter::new();
    w.write_u32(0);
    assert_eq!(w.into_bytes(), vec![0, 0, 0, 0]);

    let mut w = XdrWriter::new();
    w.write_u32(u32::MAX);
    assert_eq!(w.into_bytes(), vec![0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn s4_2_unsigned_integer_short_input_rejected() {
    let bytes = [0xFFu8; 3]; // one byte too few
    let mut r = XdrReader::new(&bytes);
    assert!(r.read_u32().is_err(), "must reject short u32");
}

// ===========================================================================
// §4.4 — Boolean
// ===========================================================================

/// RFC 4506 §4.4 — booleans are encoded as enums (4 bytes).
/// `0 = FALSE`, `1 = TRUE`. Any other value is **invalid**.
#[test]
fn s4_4_boolean_encoded_as_zero_or_one() {
    let mut w = XdrWriter::new();
    w.write_bool(false);
    assert_eq!(w.into_bytes(), vec![0, 0, 0, 0]);

    let mut w = XdrWriter::new();
    w.write_bool(true);
    assert_eq!(w.into_bytes(), vec![0, 0, 0, 1]);
}

#[test]
fn s4_4_boolean_invalid_value_rejected() {
    // Section 4.4 — values other than 0 / 1 are not valid booleans.
    // A strict decoder rejects them. Our XdrReader::read_bool reads
    // a u32 and tests != 0 — that's permissive. Layer 1 captures
    // the gap: we EXPECT this test to fail until we tighten the
    // decoder.
    let bytes = [0, 0, 0, 2];
    let mut r = XdrReader::new(&bytes);
    let got = r.read_bool();
    // Today's permissive behavior — flag for fix:
    if let Ok(b) = got {
        // Will fire once we tighten read_bool to reject value=2.
        assert!(
            !b,
            "RFC 4506 §4.4: value 2 is not a valid boolean — \
             must error, not silently succeed"
        );
        // Mark as a known fidelity gap until the codec is fixed.
        // When this assert flips and the test fails, the fix lands
        // in the same commit.
    }
}

// ===========================================================================
// §4.5 — Hyper Integer / Unsigned Hyper Integer
// ===========================================================================

/// RFC 4506 §4.5 — 64-bit, big-endian.
#[test]
fn s4_5_hyper_integer_big_endian() {
    let mut w = XdrWriter::new();
    w.write_u64(0x0123_4567_89AB_CDEF);
    assert_eq!(
        w.into_bytes(),
        vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]
    );
}

#[test]
fn s4_5_hyper_integer_round_trip() {
    for v in [0u64, 1, u64::MAX, 0x0123_4567_89AB_CDEF] {
        let mut w = XdrWriter::new();
        w.write_u64(v);
        let bytes = w.into_bytes();
        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_u64().unwrap(), v);
    }
}

// ===========================================================================
// §4.10 — Variable-Length Opaque Data
// ===========================================================================

/// RFC 4506 §4.10 — variable-length opaque data is encoded as:
/// `length: uint32` then `length` bytes of data, then enough zero
/// bytes (0..3) to round the total length to a multiple of four.
#[test]
fn s4_10_variable_length_opaque_pads_to_4_bytes() {
    // 1-byte payload → length=1 + 1 data + 3 pad = 8 bytes total.
    let mut w = XdrWriter::new();
    w.write_opaque(&[0xAB]);
    assert_eq!(
        w.into_bytes(),
        vec![0, 0, 0, 1, 0xAB, 0, 0, 0],
        "1-byte opaque must pad to 4 bytes after length prefix"
    );

    // 4-byte payload → no padding needed.
    let mut w = XdrWriter::new();
    w.write_opaque(&[0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(w.into_bytes(), vec![0, 0, 0, 4, 0xDE, 0xAD, 0xBE, 0xEF],);

    // 5-byte payload → length=5 + 5 data + 3 pad = 12 bytes.
    let mut w = XdrWriter::new();
    w.write_opaque(&[1, 2, 3, 4, 5]);
    assert_eq!(w.into_bytes(), vec![0, 0, 0, 5, 1, 2, 3, 4, 5, 0, 0, 0]);

    // 0-byte payload → just the length prefix.
    let mut w = XdrWriter::new();
    w.write_opaque(&[]);
    assert_eq!(w.into_bytes(), vec![0, 0, 0, 0]);
}

#[test]
fn s4_10_variable_length_opaque_round_trip() {
    for sample in [
        vec![],
        vec![0xAB],
        vec![0xDE, 0xAD, 0xBE, 0xEF],
        vec![1, 2, 3, 4, 5],
        b"hello".to_vec(),
        (0..255u8).collect(),
    ] {
        let mut w = XdrWriter::new();
        w.write_opaque(&sample);
        let bytes = w.into_bytes();
        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_opaque().unwrap(), sample);
        assert_eq!(r.remaining(), 0, "trailing bytes after opaque");
    }
}

#[test]
fn s4_10_truncated_payload_rejected() {
    // Length says 4, only 2 bytes follow — must error.
    let bytes = [0, 0, 0, 4, 0xAB, 0xCD];
    let mut r = XdrReader::new(&bytes);
    assert!(r.read_opaque().is_err());
}

#[test]
fn s4_10_missing_padding_rejected() {
    // Length says 1, payload byte present, but no 3-byte padding.
    let bytes = [0, 0, 0, 1, 0xAB];
    let mut r = XdrReader::new(&bytes);
    assert!(
        r.read_opaque().is_err(),
        "RFC 4506 §4.10: pad bytes are mandatory; truncation must error"
    );
}

// ===========================================================================
// §4.9 — Fixed-Length Opaque Data
// ===========================================================================

/// RFC 4506 §4.9 — fixed-length opaque is `n` bytes of payload,
/// followed by 0..3 zero pad bytes to round to multiple of 4.
/// **No length prefix.**
#[test]
fn s4_9_fixed_length_opaque_no_length_prefix() {
    let mut w = XdrWriter::new();
    w.write_opaque_fixed(&[0xAA, 0xBB]);
    // 2 bytes + 2 pad = 4. No length prefix.
    assert_eq!(w.into_bytes(), vec![0xAA, 0xBB, 0, 0]);
}

#[test]
fn s4_9_fixed_length_opaque_round_trip_when_multiple_of_4() {
    // The reader's `read_opaque_fixed(n)` requires the caller to
    // know n out-of-band.
    let mut w = XdrWriter::new();
    w.write_opaque_fixed(&[1, 2, 3, 4]);
    let bytes = w.into_bytes();
    let mut r = XdrReader::new(&bytes);
    assert_eq!(r.read_opaque_fixed(4).unwrap(), vec![1, 2, 3, 4]);
}

// ===========================================================================
// §4.11 — String
// ===========================================================================

/// RFC 4506 §4.11 — `string<>` is "ASCII text" but encoded
/// identically to variable-length opaque (§4.10): length prefix
/// + bytes + pad. The test asserts the wire shape and a round
/// trip on a UTF-8 string.
#[test]
fn s4_11_string_wire_shape_matches_opaque() {
    let mut w = XdrWriter::new();
    w.write_string("kiseki");
    let mut equiv = XdrWriter::new();
    equiv.write_opaque(b"kiseki");
    assert_eq!(w.into_bytes(), equiv.into_bytes());
}

#[test]
fn s4_11_string_round_trip() {
    let s = "hello, world";
    let mut w = XdrWriter::new();
    w.write_string(s);
    let bytes = w.into_bytes();
    let mut r = XdrReader::new(&bytes);
    assert_eq!(r.read_string().unwrap(), s);
}

// ===========================================================================
// Cross-implementation seed — the canonical RFC 4506 §4.10 example
// ===========================================================================

/// RFC 4506 §4.10 example (verbatim from the spec text):
///
/// ```text
///      0     1     2     3     4     5   ...
/// +-----+-----+-----+-----+-----+-----+...+-----+-----+...+-----+
/// |        length n       |byte0|byte1|...| n-1 |  0  |...|  0  |
/// +-----+-----+-----+-----+-----+-----+...+-----+-----+...+-----+
/// |<-------4 bytes------->|<------n bytes------>|<---r bytes--->|
///                         |<----n+r (where (n+r) mod 4 = 0)---->|
///                                              VARIABLE-LENGTH OPAQUE
/// ```
///
/// We seed with `n=3, bytes={0x01, 0x02, 0x03}, r=1` — this is
/// the smallest non-trivial padding case and gives any future
/// reader a concrete reference for what the codec must produce.
#[test]
fn rfc_example_s4_10_three_bytes_with_one_pad() {
    let mut w = XdrWriter::new();
    w.write_opaque(&[0x01, 0x02, 0x03]);
    assert_eq!(
        w.into_bytes(),
        vec![
            0x00, 0x00, 0x00, 0x03, // length = 3
            0x01, 0x02, 0x03, // payload
            0x00, // 1 pad byte
        ],
        "RFC 4506 §4.10 example: n=3 → length(4) + payload(3) + pad(1) = 8"
    );
}
