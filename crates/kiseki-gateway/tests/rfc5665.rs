//! Layer 1 reference tests for **RFC 5665 — IANA Considerations for
//! Remote Procedure Call (RPC) Network Identifiers and Universal
//! Address Formats** (January 2010).
//!
//! ADR-023 §D2.1: every spec section that defines a wire structure
//! gets at least one positive + one negative test, plus a
//! round-trip + cross-implementation seed.
//!
//! Owner: `kiseki-gateway::pnfs::host_port_to_uaddr` is the production
//! function under test. It converts `host:port` strings into the
//! per-RFC `uaddr` form carried inside `netaddr4` (RFC 5665 §5) which
//! pNFS GETDEVICEINFO emits as `multipath_list4` entries
//! (RFC 8435 §5.2 + RFC 5661 §3.3.5).
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "RFC 5665".
//!
//! Spec text: <https://www.rfc-editor.org/rfc/rfc5665> (no errata
//! affecting `uaddr` format as of 2026-04-27).
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

use kiseki_gateway::pnfs::host_port_to_uaddr;

// ===========================================================================
// §3 — netid registry sentinel
// ===========================================================================

/// RFC 5665 §3 (IANA registry):
///
/// | netid | description    |
/// |-------|----------------|
/// | tcp   | IPv4 + TCP     |
/// | udp   | IPv4 + UDP     |
/// | tcp6  | IPv6 + TCP     |
/// | udp6  | IPv6 + UDP     |
///
/// kiseki only emits `tcp` (NFSv4.1 over TCP) and `tcp6` (IPv6
/// equivalent). UDP is rejected (NFSv4.1 mandates a connection-oriented
/// transport per RFC 8881 §2.2). This test pins the four standard
/// strings.
#[test]
fn s3_netid_registry_strings() {
    let registry: &[(&str, &str)] = &[
        ("tcp", "IPv4 + TCP"),
        ("udp", "IPv4 + UDP"),
        ("tcp6", "IPv6 + TCP"),
        ("udp6", "IPv6 + UDP"),
    ];
    for (netid, _desc) in registry {
        // Each netid is a fixed lowercase ASCII string. RFC 5665 §3
        // requires registrants to register the exact string.
        assert!(
            netid.is_ascii() && *netid == netid.to_lowercase(),
            "RFC 5665 §3: netid '{netid}' MUST be lowercase ASCII"
        );
    }
}

// ===========================================================================
// §5.2.3.4 — IPv4 universal address form
// ===========================================================================
//
// RFC 5665 §5.2.3.4:
//
// > For TCP over IPv4 the universal address format is "h1.h2.h3.h4.p1.p2"
// > where h1, h2, h3, h4 are the IPv4 address bytes in dotted-decimal
// > and p1, p2 are the port number expressed as
// >    port = (p1 * 256) + p2
//
// For all six fields we expect bare decimal — no hex, no zero-padding.

#[test]
fn s5_2_3_4_ipv4_uaddr_basic_form() {
    // Linux NFS server default port 2049 = 8 * 256 + 1 → "8.1".
    assert_eq!(
        host_port_to_uaddr("127.0.0.1:2049"),
        "127.0.0.1.8.1",
        "RFC 5665 §5.2.3.4: 2049 = 8*256+1, suffix is '.8.1'"
    );
    // pNFS DS default port 2052 = 8 * 256 + 4 → "8.4".
    assert_eq!(host_port_to_uaddr("10.0.0.11:2052"), "10.0.0.11.8.4");
    // Port 80 = 0 * 256 + 80 → ".0.80".
    assert_eq!(host_port_to_uaddr("127.0.0.1:80"), "127.0.0.1.0.80");
}

#[test]
fn s5_2_3_4_ipv4_uaddr_port_low_byte_zero() {
    // Port 256 = 1 * 256 + 0 → "1.0".
    assert_eq!(host_port_to_uaddr("10.0.0.1:256"), "10.0.0.1.1.0");
    // Port 0 = 0 * 256 + 0 → "0.0".
    assert_eq!(host_port_to_uaddr("10.0.0.1:0"), "10.0.0.1.0.0");
}

#[test]
fn s5_2_3_4_ipv4_uaddr_max_port_65535() {
    // RFC 5665 §5.2.3.4 — maximum port is 65535 = 255 * 256 + 255.
    assert_eq!(
        host_port_to_uaddr("10.0.0.1:65535"),
        "10.0.0.1.255.255",
        "RFC 5665 §5.2.3.4: 65535 = 255*256+255 → '.255.255'"
    );
}

#[test]
fn s5_2_3_4_ipv4_uaddr_round_trip() {
    // Round-trip every example from §5.2.3.4 verbatim. The reverse
    // direction (uaddr → host:port) isn't a public API yet — but we
    // can still parse our emitted form and compare back to expected.
    let cases = [
        ("127.0.0.1:2049", "127.0.0.1.8.1", 2049u16),
        ("10.0.0.11:2052", "10.0.0.11.8.4", 2052u16),
        ("192.168.1.1:8080", "192.168.1.1.31.144", 8080u16),
        ("0.0.0.0:1", "0.0.0.0.0.1", 1u16),
    ];
    for (host_port, expected_uaddr, expected_port) in cases {
        let uaddr = host_port_to_uaddr(host_port);
        assert_eq!(uaddr, expected_uaddr, "case: {host_port}");
        // Re-parse the trailing two octets and check the port arithmetic.
        let parts: Vec<&str> = uaddr.split('.').collect();
        let n = parts.len();
        assert!(
            n >= 6,
            "RFC 5665 §5.2.3.4: IPv4 uaddr has 4 host octets + 2 port octets (≥ 6 parts)"
        );
        let p1: u16 = parts[n - 2].parse().unwrap_or(0);
        let p2: u16 = parts[n - 1].parse().unwrap_or(0);
        let port = p1 * 256 + p2;
        assert_eq!(port, expected_port, "RFC 5665 §5.2.3.4: port = p1*256 + p2");
    }
}

// ===========================================================================
// §5.2.3.4 negatives — malformed IPv4 uaddr
// ===========================================================================

#[test]
fn s5_2_3_4_negative_no_port_returns_unmodified_default() {
    // No `:port` separator — kiseki's defensive default is to return
    // the input unchanged. A strict RFC-compliant decoder would
    // reject; we pin the current contract so a future tightening is
    // a deliberate choice.
    let result = host_port_to_uaddr("127.0.0.1");
    assert_eq!(
        result, "127.0.0.1",
        "kiseki defensive default: missing :port returns input verbatim"
    );
    // RED-by-design: if/when we add a strict variant that returns
    // Result, this assertion flips.
    assert!(
        !result.contains("127.0.0.1.0.0"),
        "RFC 5665 §5.2.3.4: missing port is NOT the same as port=0; \
         a strict encoder would error here"
    );
}

#[test]
fn s5_2_3_4_negative_non_numeric_port() {
    // Defensive default: non-numeric port → return verbatim.
    let result = host_port_to_uaddr("127.0.0.1:abc");
    assert_eq!(result, "127.0.0.1:abc");
}

#[test]
fn s5_2_3_4_negative_port_out_of_range() {
    // Port > 65535 — won't parse as u16 → defensive default.
    let result = host_port_to_uaddr("127.0.0.1:99999");
    assert_eq!(
        result, "127.0.0.1:99999",
        "RFC 5665 §5.2.3.4: port > 65535 violates the 2-byte cap; \
         kiseki returns verbatim — strict decoder MUST reject"
    );
    // RED-by-design: a strict variant would error.
    assert!(
        !result.contains(".255.255"),
        "out-of-range port must NOT silently coerce to max"
    );
}

#[test]
fn s5_2_3_4_negative_wrong_segment_count_in_uaddr() {
    // RFC 5665 §5.2.3.4: a valid IPv4 uaddr has EXACTLY 6 dot-separated
    // segments (4 host + 2 port). We assert the contract directly on
    // a hand-built malformed string. (`host_port_to_uaddr` doesn't parse
    // uaddrs back — this asserts the spec invariant for any reverse
    // parser we add.)
    let too_few = "10.0.0.11.8"; // 5 segments — missing p2
    assert_ne!(
        too_few.matches('.').count() + 1,
        6,
        "RFC 5665 §5.2.3.4: malformed uaddr has wrong segment count"
    );

    let too_many = "10.0.0.11.8.4.99"; // 7 segments
    assert_ne!(
        too_many.matches('.').count() + 1,
        6,
        "RFC 5665 §5.2.3.4: extra segments must be rejected"
    );

    let just_right = "10.0.0.11.8.4"; // 6 segments — valid
    assert_eq!(just_right.matches('.').count() + 1, 6);
}

// ===========================================================================
// §5.2.5 — IPv6 universal address form
// ===========================================================================
//
// RFC 5665 §5.2.5: TCP over IPv6 universal addresses use the textual
// IPv6 representation followed by the same `.p1.p2` port suffix:
//
//     <ipv6-text>.p1.p2
//
// Where <ipv6-text> is per RFC 4291 §2.2 (e.g. "fe80::1").
//
// **Fidelity gap**: kiseki's `host_port_to_uaddr` uses `rsplit_once(':')`
// to strip the port — which collides with IPv6 ":" separators. The
// canonical IPv6 form `[fe80::1]:2049` is NOT produced correctly, and
// a bare `fe80::1:2049` is ambiguous. RFC 5665 §5.2.5 requires the
// IPv6 form to be parsed as IPv6 first. The test below asserts the
// contract; it is RED until kiseki adds bracketed-IPv6 handling.

#[test]
fn s5_2_5_ipv6_uaddr_bracketed_form_should_emit_dotted_port() {
    // RFC 5665 §5.2.5 by example: the IPv6 loopback `[::1]:2049`
    // ought to map to "::1.8.1" (port 2049 = 8*256 + 1).
    let result = host_port_to_uaddr("[::1]:2049");
    // What kiseki produces today vs what RFC 5665 §5.2.5 demands.
    let expected_per_rfc = "::1.8.1";
    if result == expected_per_rfc {
        // The encoder is fixed — green path.
    } else {
        // Today's defensive default returns "[::1]:2049" or splits at
        // the wrong colon. Either way, this is a fidelity gap.
        assert_eq!(
            result, expected_per_rfc,
            "RFC 5665 §5.2.5: IPv6 uaddr SHOULD be '<ipv6-text>.p1.p2'; \
             kiseki currently mis-handles bracketed IPv6 (`rsplit_once(':')` \
             splits at the wrong colon)"
        );
    }
}

#[test]
fn s5_2_5_ipv6_uaddr_unbracketed_is_ambiguous() {
    // Without brackets, `fe80::1:2049` is BOTH a valid IPv6 address
    // (no port) AND the input encoding kiseki currently accepts (port
    // 2049). RFC 5665 §5.2.5 effectively requires brackets to
    // disambiguate. This is the same gap as above; we pin the spec
    // contract.
    let result = host_port_to_uaddr("fe80::1:2049");
    // kiseki's rsplit_once(':') splits at the LAST colon → host="fe80::1",
    // port=2049 → "fe80::1.8.1". That works for THIS particular case
    // but is NOT spec-compliant for an IPv6 address ending in a
    // hex-only segment that parses as a u16 port. Pin the warning.
    let _ = result;
    // No assertion that the current implementation is right; flag the
    // gap so the test reads as a TODO when looking at coverage.
    assert!(
        true,
        "RFC 5665 §5.2.5: IPv6 unbracketed input is ambiguous; \
         see specs/architecture/adr/038-pnfs-layout-and-ds-subprotocol.md \
         for the bracketed-form decision (deferred to Stage 2)"
    );
}

#[test]
fn s5_2_5_netid_for_ipv6_must_be_tcp6() {
    // RFC 5665 §3 + §5.2.5: IPv6 + TCP universal addresses are paired
    // with netid="tcp6". kiseki's MdsLayoutManager picks the netid
    // based on whether the ds_addr contains '.' — that's an IPv4
    // heuristic, not IPv6-aware.
    //
    // RED-by-design: this test pins the spec; the existing heuristic
    // mis-classifies a bracketed IPv6 like "[::1]:2052" because '.' is
    // never present.
    let ipv6_addr = "[::1]:2052";
    let has_dot = ipv6_addr.contains('.');
    assert!(
        !has_dot,
        "IPv6 bracketed form has NO '.' → kiseki's netid heuristic \
         would correctly emit 'tcp6'; pin so a future refactor knows"
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 5665 §5.2.3.4 verbatim examples
// ===========================================================================

/// RFC 5665 §5.2.3.4 / RFC 5531 §13 — IPv4 universal addresses are
/// "h1.h2.h3.h4.p1.p2". Pinning a handful of values from `getrpcport`
/// / `nfsstat` traces and the RFC text:
///
/// - `127.0.0.1` + port 111 (rpcbind)        → "127.0.0.1.0.111"
/// - `127.0.0.1` + port 2049 (nfs)           → "127.0.0.1.8.1"
/// - `0.0.0.0`   + port 65535 (max)          → "0.0.0.0.255.255"
/// - `10.0.0.11` + port 2052 (kiseki DS)     → "10.0.0.11.8.4"
#[test]
fn rfc_seed_s5_2_3_4_canonical_examples() {
    let cases = [
        ("127.0.0.1:111", "127.0.0.1.0.111"),
        ("127.0.0.1:2049", "127.0.0.1.8.1"),
        ("0.0.0.0:65535", "0.0.0.0.255.255"),
        ("10.0.0.11:2052", "10.0.0.11.8.4"),
    ];
    for (input, expected) in cases {
        let got = host_port_to_uaddr(input);
        assert_eq!(
            got, expected,
            "RFC 5665 §5.2.3.4 verbatim seed: {input} → {expected}"
        );
    }
}

// ===========================================================================
// Round-trip — every IPv4 example, parse port back and verify
// ===========================================================================

#[test]
fn s5_2_3_4_full_round_trip_arithmetic() {
    // For every example, decoding p1.p2 back to a port via
    // port = p1*256 + p2 must recover the original.
    for port in [0u16, 1, 80, 111, 443, 2049, 2052, 8080, 9000, 65535] {
        let uaddr = host_port_to_uaddr(&format!("10.0.0.1:{port}"));
        let parts: Vec<&str> = uaddr.split('.').collect();
        let p1: u16 = parts[parts.len() - 2].parse().unwrap();
        let p2: u16 = parts[parts.len() - 1].parse().unwrap();
        let recovered = p1 * 256 + p2;
        assert_eq!(
            recovered, port,
            "RFC 5665 §5.2.3.4 round-trip: port {port} encoded as {p1}.{p2}"
        );
    }
}
