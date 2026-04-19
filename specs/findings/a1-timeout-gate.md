# A.1 — Adversarial Gate: Transport Timeouts

**Reviewer**: adversary role. **Date**: 2026-04-19.
**Scope**: `kiseki-transport/src/tcp_tls.rs` timeout additions.

---

## Finding: Timeout propagates correctly through both paths

Severity: **Positive finding**
Category: Correctness

Both `TcpStream::connect` and `connector.connect` are wrapped with
`tokio::time::timeout`. The timeout duration comes from `TimeoutConfig`
which has configurable defaults (5s connect, 10s handshake). On
timeout, `TransportError::Timeout` is returned with a descriptive
message including the address and duration. Test verified with
TEST-NET-1 (RFC 5737) non-routable address.

---

## Finding: No keepalive or TCP_NODELAY

Severity: **Low**
Category: Robustness

TCP connection is created without `set_nodelay(true)` or keepalive
configuration. For storage fabric traffic, Nagle's algorithm can add
latency to small writes. Keepalive detects dead connections.

**Status**: OPEN — non-blocking (optimization, not correctness).

---

## Summary: 0 blocking.
