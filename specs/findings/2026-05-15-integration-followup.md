# Integration follow-up — Step C / S3 307 + ForwardToLeader (2026-05-15)

**Status**: Integration commit landed. Closes the deferred follow-up
called out in commit `26bb4e2` (Step C merge) — "S3 path doesn't yet
consume `GatewayError::ForwardToLeader`".
**Date**: 2026-05-15
**Author**: integrator
**Spec references**: ADR-014 (S3), ADR-042 §4 (native), ADR-008 rev 2
(client discovery), `specs/findings/2026-05-15-leader-forwarding-posture.md`,
`specs/findings/2026-05-15-step-c-gate1.md`.

## What this commit changed

The Step C merge (commit `26bb4e2`) shipped the S3 307 redirect path
on `GatewayError::LeaderUnavailable { leader_hint: Option<NodeId> }`
but left the `GatewayError::ForwardToLeader { leader_node_id: NodeId }`
arm unwired in S3. Step A's `LogError::ForwardToLeader` surfaces a
**definite** leader hint through `write_with_forwarding`, but the S3
adapter (`S3Gateway::put_object`) called the legacy
`GatewayOps::write(...)` which collapses `ForwardToLeader` onto
`Upstream`. Net effect: a follower's openraft hint with a concrete
`leader_node_id` was dropped at the S3 boundary and the request fell
through to `500 Internal Server Error` instead of `307 Temporary
Redirect`.

Two changes close the gap:

1. **`crates/kiseki-gateway/src/s3.rs::S3Gateway::put_object`** — swap
   `inner.write(...)` → `inner.write_with_forwarding(...)`. Backends
   without per-shard Raft inherit the default `write_with_forwarding`
   impl in the trait (`crates/kiseki-gateway/src/ops.rs:116-121`),
   which delegates to `write` — zero behavior change for the
   `InMemoryGateway` test path or any other single-leader backend.

2. **`crates/kiseki-gateway/src/s3_server.rs::put_or_upload_part`** —
   add a sibling match arm for `GatewayError::ForwardToLeader`
   alongside the existing `LeaderUnavailable` arm. Reuses the same
   `leader_unavailable_response` helper (scheme preservation per S3,
   method-gating per S6, peer-map lookup, optional metric bump) by
   passing `Some(leader_node_id.0)` as the hint.

## Test coverage added

Two new unit tests in `s3_server::tests`:

| Test | Asserts |
|---|---|
| `forward_to_leader_put_emits_307_with_peer_location` | PUT to a follower whose stub `write_with_forwarding` returns `ForwardToLeader{leader_node_id=2}` produces `307` + `Location: http://10.0.0.2:9000/...` + metric bump (1 increment, labels `protocol="s3", tenant="unknown"`). |
| `forward_to_leader_unknown_peer_falls_back_to_503` | `ForwardToLeader{leader_node_id=99}` with node 99 absent from the peer map falls back to `503` + `Retry-After: 1` — same fallback contract as `LeaderUnavailable` with an unresolvable hint. |

A new `ForwardingStubGateway` impl in the test module exercises the
adapter chain end-to-end: it satisfies `GatewayOps` by returning
`GatewayError::ForwardToLeader` from `write_with_forwarding` and
`GatewayError::Upstream` from the legacy `write` (verifying the
adapter actually calls the forwarding variant — a `write`-only stub
would 500 instead of 307).

## What closed (cross-referenced against Step C gate 1)

| Finding | Status | Why |
|---|---|---|
| S1 (stale-leader retry cascade enforcement) | Open — client-side concern | The hop cap belongs to `kiseki-client::native::client.rs`, not the S3 server. Step C's plan correctly says "server-side 307 emission does NOT track hop count (stateless)". This integration commit does NOT close S1. |
| S2 (multi-seed bootstrap fall-through) | Open — kiseki-client concern | Not in the S3 integration scope. |
| S3 (TLS scheme preservation) | Inherited PASS | The new arm calls `leader_unavailable_response` which already takes `request_scheme` from `request_scheme_from_uri_and_headers`. Existing test `leader_unavailable_put_preserves_https_scheme` covers the helper; the new tests inherit the same code path. |
| S4 (HTTP bootstrap version=0 regression) | Open — kiseki-client concern | Not in the S3 integration scope. |
| S6 (GET MUST NOT 307) | Inherited PASS | The new arm passes `Method::PUT` and the helper's method-gate (line 275-284) returns 503 for non-write methods. For consistency, GET against `ForwardToLeader` would also be a bug worth surfacing — but the S3 GET handler doesn't call `write_with_forwarding`, so the arm isn't reachable from GET. |
| S8 (metric tenant label) | Inherited OPEN | The `kiseki-server::metrics` registration is `&["protocol"]` (single label) but the handler passes `&["s3", "unknown"]` (two labels). This integration commit does NOT change that — it reuses the existing helper. Live runtime impact today is zero because `runtime.rs:1179` calls `s3_router_full` (no metric wired), not `s3_router_with_peers`. **Action**: when the runtime is upgraded to pass the metric, the registration must move to `&["protocol", "tenant"]` to match. |

## What this commit does NOT touch

- **Native gRPC proxy fallback** — Step A's `KISEKI_NATIVE_PROXY_FALLBACK=off`
  default is preserved. The native server's `Status::unavailable` path
  on `LogError::ForwardToLeader` (when the proxy gate is off) is
  unchanged. Clients refresh topology and retry. No regression.
- **NFS / pNFS leader forwarding** — Per
  `2026-05-15-leader-forwarding-posture.md` §"Decision" NFS row, this
  is deferred to a follow-up ADR. The NFS path's `LogError::ForwardToLeader`
  still falls through whatever error mapping the NFS gateway has today
  (no 307-analog defined for NFSv4 yet).
- **S3 multipart finalize / DELETE / bucket-level ops** — Only
  `put_or_upload_part` is wired today. `post_multipart` (multipart
  finalize), `delete_or_abort`, `create_bucket`, `delete_bucket` still
  collapse all `GatewayError` to `500 Internal Server Error`. These
  are in scope for a follow-up that audits the S3 router's error
  taxonomy comprehensively. The `LeaderUnavailable` arm has the same
  gap today (only `put_or_upload_part` consumes it), so the new
  `ForwardToLeader` arm has parity with the existing posture.
- **`@deferred-feature` BDD scenarios** in
  `specs/features/native-gateway.feature:173-190` (transparent proxy
  fallback + proxy-node-dies-mid-proxy). Those exercise the wire-level
  byte-for-byte gateway-to-gateway `put_object` dial inside the
  native-server proxy — a separate slice from the S3 307 integration.
  Tag stays `@deferred-feature`; the follow-up ADR-042 §4 wire path
  remains the canonical owner.

## Verification

- `cargo test -p kiseki-gateway --lib s3_server::tests` — 38/38 pass
  (36 existing + 2 new for `ForwardToLeader`).
- `cargo fmt --check` — clean.
- `cargo clippy -p kiseki-gateway --lib --tests -- -D warnings` — clean.
- `make test-fast` — see commit message for tier-1 status.

## Follow-up backlog

1. **S8 metric label arity** — **CLOSED 2026-05-15** by branch
   `feat/s3-307-completion`. The registration was already
   `&["protocol", "tenant"]` (`kiseki-server/src/metrics.rs:319`);
   the handler-side call sites previously passed `"unknown"` because
   no SigV4 resolution had run at the 307 emission point. The fix
   resolves the SigV4 tenant up front in each mutation handler
   (`put_or_upload_part`, `post_multipart`, `delete_or_abort`,
   `create_bucket`) via the new `S3State::resolve_auth_tenant` +
   `tenant_label_from_auth` helpers. Authenticated requests now
   tick `tenant="<uuid>"`; anonymous / failed-auth requests tick
   `tenant="unauthenticated"` (no longer `"unknown"`). Covered by
   `s3_server::tests::sigv4_authenticated_put_307_carries_tenant_label`
   and `s3_server::tests::unauthenticated_put_307_labels_metric_unauthenticated`.

2. **S3 multipart / DELETE 307 wiring** — **CLOSED 2026-05-15** by
   branch `feat/s3-307-completion`. Added `LeaderUnavailable` +
   `ForwardToLeader` arms to `post_multipart` (both
   `CreateMultipartUpload` and `CompleteMultipartUpload` branches),
   `delete_or_abort` (both `AbortMultipartUpload` and `DeleteObject`
   branches), `create_bucket` (covers `ensure_namespace`), and the
   `UploadPart` branch of `put_or_upload_part`. `delete_bucket` is
   a pure local-bucket-cache op and does NOT route through Raft so
   it has no 307 arm. Covered by six new `*_emits_307` tests in
   `s3_server::tests`.

3. **Wire-level proxy `put_object` re-issue** (ADR-042 §4 native row,
   step A's deferred wire scope) — Open. The `@deferred-feature`
   scenarios in `native-gateway.feature` lines 173-190 stay tagged
   until the gateway-to-gateway dial reuses `ControlFields` byte-
   for-byte. Step A's `ProxyClient` plumbing landed; the missing
   piece is the actual `put_object` RPC re-issue, which is the next
   implementer slot.

4. **`/cluster/info` peer-map → S3 router wiring in `runtime.rs`** —
   **CLOSED 2026-05-15** by branch `feat/s3-307-completion`.
   `runtime.rs` now calls `s3_router_with_peers` with a
   `NodeId → "host:s3_port"` map computed from `cfg.raft_peers` via
   the new `compute_s3_peer_addrs` helper, and plumbs
   `metrics.stale_leader_redirects_total` so every 307 emission
   ticks the live counter. Covered by three new unit tests in
   `runtime::tests`.
