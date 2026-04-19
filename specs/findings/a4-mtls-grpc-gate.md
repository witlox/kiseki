# A.4 — Adversarial Gate: mTLS on gRPC Listeners

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-server/src/{config,runtime,main}.rs` mTLS wiring.

---

## Finding: Both listeners support mTLS

Severity: **Positive finding**

Both the data-path (9100) and advisory (9101) gRPC servers use the
same `build_tls()` function, which loads CA + cert + key PEMs and
constructs a `tonic::ServerTlsConfig` with `client_ca_root` (requires
client cert). Both listeners reject unauthenticated connections when
TLS is configured.

---

## Finding: Plaintext fallback for development

Severity: **Medium**
Category: Security > Authentication
Location: `runtime.rs:75-79`

**Description**: When `KISEKI_CA_PATH` etc. are not set, the server
runs in plaintext mode with a `WARNING` log. This is intentional for
development but could be accidentally deployed without TLS.

**Suggested resolution**: Add a `--require-tls` flag or
`KISEKI_REQUIRE_TLS=true` env var that makes the server exit if TLS
is not configured.

**Status**: OPEN — non-blocking (development convenience, logged).

---

## Finding: CRL not wired to tonic (uses tonic's built-in CA only)

Severity: **Low**
Category: Security

**Description**: `build_tls` uses tonic's `ServerTlsConfig` which
supports CA root but does not expose CRL configuration. The
`server_config_with_crl` from `kiseki-transport` is not used.
Tonic's TLS layer handles CA validation but not CRL checking.

To wire CRL, we'd need to bypass tonic's `ServerTlsConfig` and
inject a raw `rustls::ServerConfig` — which tonic 0.12 supports
via `service_builder.tls_config(...)` when using a custom connector.
This is deferred.

**Status**: OPEN — non-blocking (CRL API exists in transport but
tonic wiring needs custom connector).

---

## Finding: No integration test with mTLS client

Severity: **Medium**
Category: Correctness > Missing negatives

No test starts the server with mTLS and connects with a tonic client
using client certs. The wiring is verified by compilation and manual
testing (port binding confirmed), but there's no automated test.

**Status**: OPEN — non-blocking (deferred to C.5 integration test).

---

## Summary: 0 blocking. Closes the WI-4 High finding (mTLS available).
2 Medium (plaintext fallback, no integration test) + 1 Low (CRL not
in tonic path).
