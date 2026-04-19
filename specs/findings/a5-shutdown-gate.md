# A.5 — Adversarial Gate: Graceful Shutdown

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-server/src/runtime.rs` shutdown changes.

---

## Finding: Both servers use serve_with_shutdown

Severity: **Positive finding**

Both the data-path and advisory gRPC servers use
`serve_with_shutdown` with `tokio::signal::ctrl_c()`. On
Ctrl-C/SIGINT, tonic stops accepting new connections and drains
in-flight RPCs before returning.

---

## Finding: Both runtimes listen for ctrl_c independently

Severity: **Low**
Category: Correctness

Both runtimes register their own `ctrl_c()` handler. On a single
signal, both will trigger shutdown. This is correct behavior — both
servers should stop together. However, the ordering is not
deterministic (whichever runtime receives the signal first shuts
down first).

**Status**: OPEN — non-blocking (correct behavior, non-deterministic
ordering is acceptable for graceful shutdown).

---

## Summary: 0 blocking. Stage A complete.
