# WI-4 — Adversarial Gate: Server Runtime Composition

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-server/src/{main,config,runtime}.rs`

---

## Finding: No mTLS on gRPC listeners

Severity: **High**
Category: Security > Authentication
Spec reference: I-Auth1, I-K2

**Description**: Both gRPC listeners (`tonic::transport::Server`)
accept plaintext connections. No TLS configuration is applied.
The `kiseki-transport` crate has `TlsConfig` with mTLS support, but
it's not wired into the tonic server builder.

**Suggested resolution**: Use `tonic::transport::Server::builder()
.tls_config(...)` with `ServerTlsConfig` from rustls certs. The
`TlsConfig::server_config()` from kiseki-transport provides the
necessary `rustls::ServerConfig`.

**Status**: OPEN — blocking for production, non-blocking for the
runtime composition proof (demonstrates dual-runtime architecture).

---

## Finding: No graceful shutdown

Severity: **Medium**
Category: Robustness
Location: `main.rs:44-50`

**Description**: The main runtime blocks on `run_main` which calls
`tonic::serve()` which blocks forever. There's no signal handler
(SIGTERM/SIGINT) for graceful shutdown. The advisory runtime handle
is created but `advisory_handle.await` is only reached if the main
server exits (which it doesn't under normal operation).

**Suggested resolution**: Use `tokio::signal::ctrl_c()` or
`tonic::transport::Server::serve_with_shutdown()` with a shutdown
signal. Drain sequence: stop accepting → drain in-flight → flush
Raft → close audit → shutdown advisory → exit.

**Status**: OPEN — non-blocking.

---

## Finding: Data-path contexts not injected into gRPC handlers

Severity: **Medium**
Category: Correctness > Implicit coupling

**Description**: `run_main` constructs `MemShardStore`, `AuditLog`,
`ChunkStore`, `CompositionStore`, `ViewStore` but they are all
`_`-prefixed and not passed to any gRPC handler. Only the key
manager is wired. The data-path contexts exist but don't serve
traffic.

**Suggested resolution**: As data-path gRPC services are defined
(Phase 3 log service, etc.), inject the stores into their handlers.
Currently there are no data-path gRPC services — only
KeyManagerService and WorkflowAdvisoryService.

**Status**: OPEN — non-blocking (no data-path gRPC services exist yet).

---

## Finding: Advisory runtime proven isolated

Severity: **Positive finding**
Category: Correctness

**Description**: The server correctly creates two separate tokio
runtimes with `Builder::new_multi_thread()`. The advisory gRPC
server runs on the advisory runtime via `advisory_rt.spawn()`.
The main server runs on the main runtime via `main_rt.block_on()`.
This matches ADR-021 §1 (isolated runtime for advisory).

Verified by `lsof` output: same PID, two listeners on different
ports, different file descriptors.

**Status**: Confirmed correct.

---

## Summary

| Severity | Count | Blocking |
|---|---|---|
| High | 1 (mTLS) | Non-blocking for proof |
| Medium | 2 | No |
| Positive | 1 | — |

**No blocking findings** for the runtime composition proof.
The High finding (no mTLS) is the next integration step.
