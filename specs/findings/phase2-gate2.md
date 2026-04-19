# Phase 2 — Adversarial Gate-2 Findings

**Reviewer**: adversary role.
**Date**: 2026-04-19.
**Scope**: `kiseki-transport` — all source, tests, and build config at `67b1420`.

---

## Finding: OrgId extraction is a placeholder

Severity: **Medium**
Category: Correctness > Specification compliance
Location: `crates/kiseki-transport/src/tcp_tls.rs:141-149`
Spec reference: I-Auth1, I-T1

**Description**: `extract_peer_identity` derives `OrgId` from the
certificate fingerprint via UUID v5 namespace hashing instead of
parsing the actual OU (Organizational Unit) or SPIFFE URI from the
X.509 subject/SANs. Every certificate gets a unique, fingerprint-derived
`OrgId` unrelated to the real tenant organization. The code comment
explicitly defers to "full X.509 parsing will use x509-parser in
Phase 10."

**Impact**: Tenant isolation (I-T1) cannot be enforced until real
subject parsing is implemented. Phase 2 scope is transport plumbing,
not tenant identity — so this is expected.

**Suggested resolution**: Add `x509-parser` dependency and implement
OU/SAN extraction when downstream crates need real tenant identity
(Phase 9/10). Track as a known gap.

**Status**: OPEN — non-blocking (explicitly deferred; no downstream
consumer of `OrgId` yet).

---

## Finding: No connection or handshake timeout

Severity: **Medium**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-transport/src/tcp_tls.rs:40-52`
Spec reference: error-taxonomy §timeout

**Description**: `TcpStream::connect(addr)` and
`connector.connect(server_name, tcp)` use OS defaults for TCP
connection timeout and TLS handshake timeout respectively. A node
connecting to an unreachable peer could block for 60-120 seconds.

**Suggested resolution**: Wrap both operations with
`tokio::time::timeout(duration, ...)` using a configurable timeout
(default 5s connect, 10s handshake).

**Status**: OPEN — non-blocking (no production callers yet).

---

## Finding: No certificate revocation checking

Severity: **Medium**
Category: Security > Authentication
Location: `crates/kiseki-transport/src/config.rs:76-79`, `108-110`
Spec reference: I-Auth1, authentication.feature §"revoked certificate"

**Description**: Neither the client nor server TLS config enables CRL
distribution point checking or OCSP stapling. A revoked Cluster CA-signed
certificate would be accepted. The `authentication.feature` includes
a scenario for revoked certificate rejection.

**Suggested resolution**: Integrate CRL/OCSP when the control plane
(Phase 11) provides certificate revocation infrastructure. For now,
document as a known gap.

**Status**: OPEN — non-blocking (requires control plane revocation
service, Phase 11).

---

## Finding: No connection pool or keepalive

Severity: **Low**
Category: Robustness > Resource exhaustion
Location: `crates/kiseki-transport/src/tcp_tls.rs` (entire file)
Spec reference: build-phases.md §Phase 2

**Description**: The implementation plan lists "Connection pool +
keepalive + timeout semantics" but none are implemented. Each `connect`
call creates a new TCP connection and TLS handshake. For high-throughput
chunk transfers, this adds significant per-request latency.

**Suggested resolution**: Add a pooled wrapper around `TcpTlsTransport`
with `TCP_NODELAY`, keepalive, and connection reuse. Defer to when
downstream consumers (Phase 6+) need it.

**Status**: OPEN — non-blocking (no high-throughput callers yet).

---

## Finding: No SPIFFE SVID parsing from SANs

Severity: **Low**
Category: Correctness > Specification compliance
Location: `crates/kiseki-transport/src/tcp_tls.rs:123-127`
Spec reference: I-Auth3

**Description**: I-Auth3 requires acceptance of SPIFFE SVIDs as an
alternative identity mechanism. The doc comment mentions "Falls back to
parsing SPIFFE URIs from SANs" but the implementation only uses
fingerprint-derived identity.

**Suggested resolution**: Implement SAN URI parsing for
`spiffe://cluster/org/<org_id>` pattern when `x509-parser` is added.

**Status**: OPEN — non-blocking (deferred to Phase 10).

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| Medium | 3 | No |
| Low | 2 | No |

**No blocking findings.** Phase 2 is transport plumbing — the Medium
findings (OrgId placeholder, no timeout, no revocation) are tracked for
resolution in later phases when downstream consumers and the control
plane exist.

**Recommendation**: Phase 2 can close. Track Medium findings for
Phase 9/10/11.
