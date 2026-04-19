# A.2 — Adversarial Gate: X.509 OU/SAN Parsing

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-transport/src/tcp_tls.rs` X.509 parsing rewrite.

---

## Finding: OU parsing works correctly with test certs

Severity: **Positive finding**

`extract_peer_identity` now uses `x509-parser` to extract CN and OU
from the X.509 subject. Test `ou_extracted_as_org_id` verifies that
OU="test-tenant" produces the expected UUID v5-derived `OrgId`, and
CN="server" is extracted correctly. Fingerprint is computed via
`aws-lc-rs` (FIPS).

---

## Finding: `x509-parser` does not perform crypto

Severity: **Positive finding**

Verified: `x509-parser` 0.16 uses `der-parser` and `asn1-rs` for
ASN.1 parsing only. No crypto operations. Does not pull in `ring`
or any non-FIPS crypto backend. Safe to use alongside `aws-lc-rs`.

---

## Finding: Fingerprint fallback still reachable

Severity: **Low**
Category: Correctness

If a cert has neither OU nor SPIFFE SAN, `extract_peer_identity`
falls back to UUID v5 from the fingerprint. This is a valid
degradation path (some certs may not have OU), but it means two
certs without OU get different `OrgId`s — they can't be grouped
into the same tenant.

**Status**: OPEN — non-blocking (intentional fallback with clear
semantics).

---

## Finding: Multi-valued OU not handled

Severity: **Low**
Category: Correctness > Edge cases

`iter_organizational_unit().next()` takes only the first OU value.
If a cert has multiple OU fields, the others are silently ignored.
In practice, Kiseki-issued certs will have exactly one OU.

**Status**: OPEN — non-blocking.

---

## Summary: 0 blocking. 2 positive + 2 Low.
