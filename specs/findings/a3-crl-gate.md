# A.3 — Adversarial Gate: CRL Certificate Revocation

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-transport/src/config.rs` CRL additions.

---

## Finding: CRL API wired correctly

Severity: **Positive finding**

`server_config_with_crl` accepts optional CRL PEM data, parses it via
`rustls_pemfile::crls`, and passes it to
`WebPkiClientVerifier::builder().with_crls()`. The original
`server_config` remains backward-compatible (calls through with
`crl_pem: None`).

---

## Finding: No CRL integration test

Severity: **Medium**
Category: Correctness > Missing negatives

No test verifies that a revoked certificate is actually rejected.
`rcgen` supports CRL generation, so a test is feasible. Without it,
we're trusting rustls to implement CRL checking correctly (which it
does — it's a well-tested library), but there's no project-level
verification.

**Suggested resolution**: Add a test that generates a CA + node cert +
CRL revoking the node cert, then verifies the TLS handshake fails.

**Status**: OPEN — non-blocking (rustls CRL implementation is trusted,
but the project should verify the wiring).

---

## Finding: CRL loaded once at startup — no refresh

Severity: **Low**
Category: Robustness

CRL data is loaded at config construction time and never refreshed.
In production, CRLs have an expiry and need periodic reloading. This
is an operational limitation to address later.

**Status**: OPEN — non-blocking (documented limitation).

---

## Summary: 0 blocking. 1 Medium (missing test) + 1 Low (no refresh).
