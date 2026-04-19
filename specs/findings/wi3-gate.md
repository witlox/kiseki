# WI-3 — Adversarial Gate: gRPC Server Wiring

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-keymanager/src/grpc.rs`, `kiseki-advisory/src/grpc.rs`

---

## Finding: KeyManagerService never sends key material over gRPC

Severity: **Low** (by design)
Category: Security > Cryptographic correctness
Location: `kiseki-keymanager/src/grpc.rs:68`

**Description**: `FetchMasterKeyResponse.key_material` is always empty.
The comment says "never sent over gRPC" — storage nodes get keys via
a secure bootstrap channel. This is correct per ADR-003 (local HKDF
derivation) but the RPC name `FetchMasterKey` is misleading. The RPC
effectively confirms epoch existence.

**Status**: OPEN — non-blocking (design is correct; naming is
confusing but matches the proto spec).

---

## Finding: No mTLS interceptor on gRPC services

Severity: **Medium**
Category: Security > Authentication
Spec reference: I-WA3, I-Auth1

**Description**: Neither `KeyManagerGrpc` nor `AdvisoryGrpc` validates
the caller's mTLS identity. The proto spec requires per-operation
certificate validation (I-WA3 for advisory, I-Auth1 for all data
fabric services). The gRPC handlers accept any authenticated caller.

**Suggested resolution**: Add a tonic interceptor layer in
`kiseki-server` that extracts `PeerIdentity` from the TLS session
and passes it as request metadata. Each handler validates the identity
against the requested scope.

**Status**: OPEN — non-blocking (interceptor is WI-4 server wiring).

---

## Finding: Go ControlService and AuditExportService not wired

Severity: **Medium**
Category: Correctness > Specification compliance

**Description**: The plan calls for 4 gRPC services. Only 2 (Rust:
KeyManager, Advisory) are implemented. The Go services (Control,
AuditExport) are deferred.

**Status**: OPEN — non-blocking (Go gRPC server wiring depends on
proto codegen for Go, which is Phase 11 infrastructure).

---

## Finding: Advisory streaming RPCs unimplemented

Severity: **Low**
Category: Correctness > Specification compliance

**Description**: `advisory_stream` and `subscribe_telemetry` return
`UNIMPLEMENTED`. These require the full advisory runtime with isolated
tokio runtime (ADR-021 §1).

**Status**: OPEN — non-blocking (WI-4 scope).

---

## Summary: 0 blocking. 2 Medium (mTLS interceptor, Go services) + 2 Low.
