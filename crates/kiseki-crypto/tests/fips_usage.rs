//! Layer 1 reference tests for **FIPS 140-2 / 140-3 cryptographic
//! primitives** as kiseki uses them.
//!
//! ADR-023 §D2: per-spec-section unit tests. The FIPS validation
//! itself is owned upstream by `aws-lc-rs`; kiseki's responsibility
//! is to use those primitives correctly. This file pins the *usage*
//! invariants:
//!
//!   1. **Nonce uniqueness**: AES-256-GCM nonces in our envelopes
//!      MUST never repeat for a given key. Reusing a nonce with the
//!      same key is a catastrophic FIPS / GCM violation.
//!   2. **HKDF info-string domain separation** (RFC 5869): every
//!      semantic key-purpose uses a distinct info string so the
//!      derived keys live in disjoint sub-spaces.
//!   3. **Key-purpose binding**: `(master_key, chunk_id, info)`
//!      determines a DEK. Same chunk_id + different info MUST yield
//!      different DEKs.
//!
//! Owner: `kiseki-crypto` — `envelope::seal_envelope`,
//! `envelope::wrap_for_tenant`, `hkdf::derive_system_dek`.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "FIPS 140-2/3 cryptographic primitives".
//!
//! Spec text:
//! - FIPS 140-3: Implementation Guidance.
//! - NIST SP 800-38D: AES-GCM mode.
//! - RFC 5869: HKDF.
//! - ADR-003 §domain — kiseki's domain-separation rules.

use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::aead::Aead;
use kiseki_crypto::envelope::{seal_envelope, wrap_for_tenant};
use kiseki_crypto::hkdf::derive_system_dek;
use kiseki_crypto::keys::{SystemMasterKey, TenantKek};

// ---------------------------------------------------------------------------
// Helpers — small fixtures used across tests.
// ---------------------------------------------------------------------------

fn test_master() -> SystemMasterKey {
    SystemMasterKey::new([0x42; 32], KeyEpoch(1))
}

fn test_tenant_kek() -> TenantKek {
    TenantKek::new([0xaa; 32], KeyEpoch(1))
}

fn chunk(byte: u8) -> ChunkId {
    ChunkId([byte; 32])
}

// ===========================================================================
// §NIST SP 800-38D — AES-GCM nonce uniqueness invariant
// ===========================================================================

/// NIST SP 800-38D §8.2 — "the probability that the authenticated
/// encryption function ever will be invoked with the same IV and the
/// same key on two (or more) distinct sets of input data shall be
/// no greater than 2^{-32}".
///
/// In practice for kiseki: two seals with the same `(master_key,
/// chunk_id)` MUST produce different nonces. The AEAD module uses a
/// CSPRNG-generated nonce per call (`aws_lc_rs::rand::fill`).
#[test]
fn aead_nonce_uniqueness_two_seals_same_key() {
    let aead = Aead::new();
    let master = test_master();
    let chunk_id = chunk(0xbb);
    let plaintext = b"identical plaintext for two seals";

    let env1 = seal_envelope(&aead, &master, &chunk_id, plaintext).expect("seal 1");
    let env2 = seal_envelope(&aead, &master, &chunk_id, plaintext).expect("seal 2");

    assert_ne!(
        env1.nonce, env2.nonce,
        "NIST SP 800-38D: two seals with same (key, plaintext) MUST produce \
         different nonces (otherwise GCM is trivially broken)"
    );

    // Defense in depth: two seals also produce different ciphertext
    // bytes. (A nonce reuse with same plaintext would yield identical
    // ciphertext — which would be observable by an attacker.)
    assert_ne!(
        env1.ciphertext, env2.ciphertext,
        "NIST SP 800-38D: same plaintext + same key MUST not produce \
         identical ciphertext (otherwise nonce was reused)"
    );
}

/// Stronger statistical check: across N seals, all nonces are
/// distinct. Birthday bound for 96-bit nonces gives a collision
/// probability ≈ N^2 / 2^97; at N=64 this is ~2^{-85}, well below
/// FIPS's 2^{-32} requirement.
#[test]
fn aead_nonce_uniqueness_many_seals() {
    let aead = Aead::new();
    let master = test_master();
    let chunk_id = chunk(0xcc);
    let plaintext = b"another identical plaintext";

    const N: usize = 64;
    let mut nonces = Vec::with_capacity(N);
    for _ in 0..N {
        let env = seal_envelope(&aead, &master, &chunk_id, plaintext).expect("seal");
        nonces.push(env.nonce);
    }

    // All distinct.
    let mut sorted = nonces.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        N,
        "NIST SP 800-38D: {N} seals must produce {N} distinct nonces, got {} unique",
        sorted.len()
    );
}

/// Nonce uniqueness must hold across DIFFERENT chunk IDs as well —
/// even though distinct chunk IDs derive distinct DEKs (so reuse
/// would be safe under those keys), the AEAD module is agnostic to
/// the derivation chain. The uniqueness invariant is global.
#[test]
fn aead_nonce_uniqueness_across_different_chunks() {
    let aead = Aead::new();
    let master = test_master();
    let plaintext = b"x";

    let env_a = seal_envelope(&aead, &master, &chunk(0x10), plaintext).expect("seal a");
    let env_b = seal_envelope(&aead, &master, &chunk(0x20), plaintext).expect("seal b");
    assert_ne!(
        env_a.nonce, env_b.nonce,
        "AES-GCM: nonces independent of chunk_id; both seals draw fresh nonces"
    );
}

/// Tenant wrapping (`wrap_for_tenant`) also issues a fresh AEAD seal
/// with its own nonce. Two wraps of the same envelope MUST produce
/// distinct wrapping nonces.
#[test]
fn aead_nonce_uniqueness_in_tenant_wrap() {
    let aead = Aead::new();
    let master = test_master();
    let kek = test_tenant_kek();
    let chunk_id = chunk(0xde);
    let plaintext = b"wrap me twice";

    let mut env1 = seal_envelope(&aead, &master, &chunk_id, plaintext).expect("seal 1");
    let mut env2 = seal_envelope(&aead, &master, &chunk_id, plaintext).expect("seal 2");
    wrap_for_tenant(&aead, &mut env1, &kek).expect("wrap 1");
    wrap_for_tenant(&aead, &mut env2, &kek).expect("wrap 2");

    let w1 = env1
        .tenant_wrapped_material
        .as_ref()
        .expect("wrap material 1");
    let w2 = env2
        .tenant_wrapped_material
        .as_ref()
        .expect("wrap material 2");
    // First 12 bytes of the wrapped material is the wrap nonce
    // (per envelope.rs: `combined = nonce || wrapped_ct`).
    assert_ne!(
        w1[..12],
        w2[..12],
        "NIST SP 800-38D: two tenant-wraps of the same envelope MUST use \
         distinct nonces"
    );
}

// ===========================================================================
// §RFC 5869 — HKDF info-string domain separation
// ===========================================================================

/// RFC 5869 §3.2 — the `info` parameter to HKDF-Expand provides
/// "a context and application specific information (e.g. protocol
/// transcript hash, conversation identifier, etc.). … the use of
/// the info field is to bind the derived keying material to
/// application-specific information, thereby preventing it from
/// being reused for other purposes."
///
/// Kiseki's ADR-003 names a concrete info string for the system DEK
/// (`"kiseki-chunk-dek-v1"`). Any other key purpose (tenant KEK,
/// pNFS file-handle MAC, future advisory token) MUST use a distinct
/// info string, otherwise two purposes share the same derived key
/// space — a domain-separation failure.
#[test]
fn hkdf_info_string_for_system_dek_is_versioned() {
    // The info string is private to `hkdf.rs`. A fidelity test pins
    // the *contract*: any purpose-binding info MUST be non-empty,
    // namespace-prefixed, and version-suffixed. We assert the rule
    // using a representative sample (the actual HKDF_INFO constant
    // in hkdf.rs is `b"kiseki-chunk-dek-v1"`).
    const SAMPLE: &[u8] = b"kiseki-chunk-dek-v1";
    let s = std::str::from_utf8(SAMPLE).expect("info must be UTF-8");
    assert!(
        s.starts_with("kiseki-"),
        "ADR-003 §domain: HKDF info MUST be namespace-prefixed (`kiseki-…`)"
    );
    assert!(
        s.ends_with("-v1") || s.ends_with("-v2") || s.ends_with("-v3"),
        "ADR-003 §domain: HKDF info MUST carry a version suffix \
         (versioned for crypto-agility)"
    );
    assert!(
        !s.is_empty(),
        "RFC 5869: empty info defeats domain separation"
    );
}

/// RFC 5869 — different info strings yield independent keys. This
/// is the empirical check: `derive_system_dek` (with the canonical
/// info `"kiseki-chunk-dek-v1"`) MUST produce a different output
/// than a hypothetical sister derivation under a different info
/// string. We exercise the property through `derive_system_dek` for
/// the production info, and a hand-rolled HKDF call for the
/// hypothetical alternative.
#[test]
fn hkdf_different_info_strings_yield_different_keys() {
    use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};
    use zeroize::Zeroizing;

    struct K(usize);
    impl aws_lc_rs::hkdf::KeyType for K {
        fn len(&self) -> usize {
            self.0
        }
    }

    let master = test_master();
    let chunk_id = chunk(0xab);

    // Production derivation — uses info "kiseki-chunk-dek-v1".
    let dek_prod = derive_system_dek(&master, &chunk_id).expect("derive production DEK");

    // Alternative derivation: same key + same salt, different info.
    let alt_info: &[u8] = b"kiseki-pnfs-fh-mac-v1";
    let salt = Salt::new(HKDF_SHA256, &chunk_id.0);
    let prk = salt.extract(master.material());
    let mut dek_alt = Zeroizing::new([0u8; 32]);
    prk.expand(&[alt_info], K(32))
        .and_then(|okm| okm.fill(&mut *dek_alt))
        .expect("alt HKDF expand");

    assert_ne!(
        *dek_prod, *dek_alt,
        "RFC 5869: HKDF info-string domain separation — distinct info MUST \
         yield distinct keys, even with the same (key, salt)"
    );
}

/// ADR-003 §domain — separate canonical info strings for each
/// key-purpose are required. This test pins the LIST of purposes
/// kiseki uses or plans to use (the *names* of the info strings
/// are part of the cryptographic contract). A future PR adding a
/// new purpose MUST extend this list.
#[test]
fn hkdf_info_strings_per_key_purpose_pinned() {
    /// Every distinct cryptographic purpose kiseki has identified.
    /// Each ROW is a separate HKDF info string; collisions = a
    /// domain-separation failure.
    const PURPOSES: &[(&str, &[u8])] = &[
        // Production today (hkdf.rs::HKDF_INFO).
        ("derive_system_dek", b"kiseki-chunk-dek-v1"),
        // Tenant wrap AAD (envelope.rs::wrap_for_tenant uses this
        // string as AAD, not HKDF info; it still must not collide
        // with any HKDF info).
        ("wrap_for_tenant_aad", b"kiseki-tenant-wrap-v1"),
        // Future: pNFS file-handle MAC (ADR-038). Pinned here so the
        // implementer cannot accidentally reuse `derive_system_dek`'s
        // info.
        ("derive_pnfs_fh_mac_key", b"kiseki-pnfs-fh-mac-v1"),
        // Future: advisory token signing (ADR-021).
        ("derive_advisory_token_key", b"kiseki-advisory-token-v1"),
    ];

    // Every info string MUST be unique.
    let mut seen = std::collections::BTreeSet::new();
    for (name, info) in PURPOSES {
        assert!(
            seen.insert(*info),
            "ADR-003 §domain: info string for {name} collides with an earlier purpose"
        );
    }
    assert_eq!(
        seen.len(),
        PURPOSES.len(),
        "all info strings must be unique"
    );

    // Every info string MUST be namespace-prefixed and version-suffixed.
    for (name, info) in PURPOSES {
        let s = std::str::from_utf8(info).expect("info must be UTF-8");
        assert!(
            s.starts_with("kiseki-"),
            "ADR-003 §domain: {name} info must be `kiseki-`-prefixed"
        );
        assert!(
            s.contains("-v"),
            "ADR-003 §domain: {name} info must carry a version suffix"
        );
    }
}

// ===========================================================================
// Key-purpose binding — same chunk_id + different info → different DEK
// ===========================================================================

/// `derive_system_dek(master, chunk_id)` is deterministic — same
/// inputs always yield the same DEK. This is the property that lets
/// kiseki avoid per-chunk key storage (ADR-003).
#[test]
fn derive_system_dek_deterministic() {
    let master = test_master();
    let chunk_id = chunk(0x99);

    let dek1 = derive_system_dek(&master, &chunk_id).expect("derive 1");
    let dek2 = derive_system_dek(&master, &chunk_id).expect("derive 2");
    assert_eq!(
        *dek1, *dek2,
        "ADR-003: HKDF derivation is deterministic given (master, chunk_id)"
    );
}

/// Key-purpose binding: distinct chunk IDs yield distinct DEKs even
/// under the same master key.
#[test]
fn derive_system_dek_changes_with_chunk_id() {
    let master = test_master();
    let dek_a = derive_system_dek(&master, &chunk(0xaa)).expect("derive a");
    let dek_b = derive_system_dek(&master, &chunk(0xbb)).expect("derive b");
    assert_ne!(
        *dek_a, *dek_b,
        "ADR-003: distinct chunk IDs MUST yield distinct DEKs (HKDF salt distinguishes)"
    );
}

/// Key-purpose binding (the headline FIPS-usage rule for kiseki):
/// the same `master_key` + same `chunk_id` MUST produce different
/// DEKs when used with different HKDF info strings — i.e. the
/// "system DEK for chunk X" and the "pNFS FH MAC key for chunk X"
/// live in disjoint key spaces.
#[test]
fn key_purpose_binding_same_inputs_different_info_yield_different_keys() {
    use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};
    use zeroize::Zeroizing;

    struct K(usize);
    impl aws_lc_rs::hkdf::KeyType for K {
        fn len(&self) -> usize {
            self.0
        }
    }

    let master = test_master();
    let chunk_id = chunk(0xee);

    // Purpose 1: system DEK (production path).
    let dek_purpose_1 = derive_system_dek(&master, &chunk_id).expect("derive system DEK");

    // Purpose 2: hand-rolled HKDF with same salt + key but different
    // info — this is what `derive_pnfs_fh_mac_key` will look like
    // (ADR-038).
    let salt = Salt::new(HKDF_SHA256, &chunk_id.0);
    let prk = salt.extract(master.material());
    let mut dek_purpose_2 = Zeroizing::new([0u8; 32]);
    prk.expand(&[b"kiseki-pnfs-fh-mac-v1"], K(32))
        .and_then(|okm| okm.fill(&mut *dek_purpose_2))
        .expect("HKDF expand purpose 2");

    assert_ne!(
        *dek_purpose_1, *dek_purpose_2,
        "FIPS / ADR-003: same (master, chunk_id) + different info MUST \
         yield different keys — purpose-binding invariant"
    );

    // And purpose 3 (advisory token) is also distinct from both.
    let mut dek_purpose_3 = Zeroizing::new([0u8; 32]);
    let salt = Salt::new(HKDF_SHA256, &chunk_id.0);
    let prk = salt.extract(master.material());
    prk.expand(&[b"kiseki-advisory-token-v1"], K(32))
        .and_then(|okm| okm.fill(&mut *dek_purpose_3))
        .expect("HKDF expand purpose 3");
    assert_ne!(
        *dek_purpose_3, *dek_purpose_1,
        "FIPS / ADR-003: advisory-token key must differ from system DEK"
    );
    assert_ne!(
        *dek_purpose_3, *dek_purpose_2,
        "FIPS / ADR-003: advisory-token key must differ from pNFS FH MAC key"
    );
}

// ===========================================================================
// Cross-implementation seed — RFC 5869 Appendix A test vector A.1
// ===========================================================================

/// RFC 5869 Appendix A.1 — first published HKDF-SHA256 test vector.
/// Public test vector; running it through `aws-lc-rs`'s HKDF and
/// comparing to the spec's expected output anchors our derivation
/// to the IETF reference.
///
/// ```text
/// IKM   = 0x0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b      (22 octets)
/// salt  = 0x000102030405060708090a0b0c                          (13 octets)
/// info  = 0xf0f1f2f3f4f5f6f7f8f9                                (10 octets)
/// L     = 42
///
/// PRK   = 0x077709362c2e32df0ddc3f0dc47bba63
///         90b6c73bb50f9c3122ec844ad7c2b3e5                      (32 octets)
/// OKM   = 0x3cb25f25faacd57a90434f64d0362f2a
///         2d2d0a90cf1a5a4c5db02d56ecc4c5bf
///         34007208d5b887185865                                  (42 octets)
/// ```
#[test]
fn rfc_seed_5869_appendix_a1_hkdf_sha256() {
    use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};

    struct L(usize);
    impl aws_lc_rs::hkdf::KeyType for L {
        fn len(&self) -> usize {
            self.0
        }
    }

    let ikm: [u8; 22] = [0x0b; 22];
    let salt_bytes: [u8; 13] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
    ];
    let info: [u8; 10] = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];

    let salt = Salt::new(HKDF_SHA256, &salt_bytes);
    let prk = salt.extract(&ikm);

    let mut okm = vec![0u8; 42];
    prk.expand(&[&info], L(42))
        .and_then(|m| m.fill(&mut okm))
        .expect("RFC 5869 A.1 expand");

    // Expected OKM from RFC 5869 Appendix A.1.
    let expected: [u8; 42] = [
        0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36, 0x2f,
        0x2a, 0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56, 0xec, 0xc4,
        0xc5, 0xbf, 0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
    ];
    assert_eq!(
        okm, expected,
        "RFC 5869 Appendix A.1: HKDF-SHA256 OKM must match the published vector \
         (anchors aws-lc-rs against the IETF reference implementation)"
    );
}

/// RFC 5869 Appendix A.2 — second test vector (longer inputs, longer
/// OKM). Public, well-known IETF vector. Anchors HKDF for the larger
/// L=82 expand path that pNFS file-handle derivation will use.
#[test]
fn rfc_seed_5869_appendix_a2_hkdf_sha256_long_okm() {
    use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};

    struct L(usize);
    impl aws_lc_rs::hkdf::KeyType for L {
        fn len(&self) -> usize {
            self.0
        }
    }

    // IKM = 0x000102…4f (80 octets, 0x00..=0x4f)
    let ikm: Vec<u8> = (0x00u8..=0x4f).collect();
    // salt = 0x606162…af (80 octets, 0x60..=0xaf)
    let salt_bytes: Vec<u8> = (0x60u8..=0xaf).collect();
    // info = 0xb0b1…ff (80 octets, 0xb0..=0xff)
    let info: Vec<u8> = (0xb0u8..=0xff).collect();

    let salt = Salt::new(HKDF_SHA256, &salt_bytes);
    let prk = salt.extract(&ikm);

    let mut okm = vec![0u8; 82];
    prk.expand(&[&info], L(82))
        .and_then(|m| m.fill(&mut okm))
        .expect("RFC 5869 A.2 expand");

    // Expected OKM from RFC 5869 Appendix A.2.
    let expected: [u8; 82] = [
        0xb1, 0x1e, 0x39, 0x8d, 0xc8, 0x03, 0x27, 0xa1, 0xc8, 0xe7, 0xf7, 0x8c, 0x59, 0x6a, 0x49,
        0x34, 0x4f, 0x01, 0x2e, 0xda, 0x2d, 0x4e, 0xfa, 0xd8, 0xa0, 0x50, 0xcc, 0x4c, 0x19, 0xaf,
        0xa9, 0x7c, 0x59, 0x04, 0x5a, 0x99, 0xca, 0xc7, 0x82, 0x72, 0x71, 0xcb, 0x41, 0xc6, 0x5e,
        0x59, 0x0e, 0x09, 0xda, 0x32, 0x75, 0x60, 0x0c, 0x2f, 0x09, 0xb8, 0x36, 0x77, 0x93, 0xa9,
        0xac, 0xa3, 0xdb, 0x71, 0xcc, 0x30, 0xc5, 0x81, 0x79, 0xec, 0x3e, 0x87, 0xc1, 0x4c, 0x01,
        0xd5, 0xc1, 0xf3, 0x43, 0x4f, 0x1d, 0x87,
    ];
    assert_eq!(
        okm, expected,
        "RFC 5869 Appendix A.2: HKDF-SHA256 long-OKM vector"
    );
}
