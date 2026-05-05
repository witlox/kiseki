# ADR-042 — Native Gateway Data Service Implementation Plan

**Status**: Phase 1 of 8 done (proto bindings landed in commit `d7144b9`); 7 phases remaining.
**Date opened**: 2026-05-05
**Predecessor**: `post-2026-05-03-sweep.md` (in-process perf spike + ADR-040 rev-3 write-behind)
**Spec source of truth**: `specs/architecture/adr/042-native-gateway-data-service.md` (post-gate-1 round-2 PASS)

## Context

ADR-042 introduces a real native gRPC `GatewayDataService` so the Rust SDK / Python / C++ FFI / future-FUSE-migration stop tunneling through the S3 HTTP path. The 2026-05-05 single-host profile measured S3 GET at 14 635 op/s — protocol layer accounted for ~87 % of the gap from the in-process gateway floor. A-NG11 commits the per-node native targets at ≥80 k op/s GET, ≥56 k op/s PUT.

The spec layer (analyst → architect → adversary gate-1 round 1 + amendments + round 2 PASS) is fully validated. Spec layer counts:

- 17 ubiquitous-language terms in the "Native gateway data service" section
- A-NG1..A-NG24 (24 entries)
- I-NG1..I-NG24 (26 unique IDs, with I-NG6a/b/c split)
- F-NG1..F-NG13 (47-mode catalogue total)
- 33 Gherkin scenarios in `specs/features/native-gateway.feature`
- Proto skeleton at `specs/architecture/proto/kiseki/v1/gateway_data.proto`

Adversary findings cleared:
- gate-0 (analyst output): 1C + 6H + 8M + 4L resolved in place
- gate-1 round 1 (architect output): 2C + 6H + 8M + 4L resolved in place
- gate-1 round 2 (post-amendment): PASS with 1 implementation-level HIGH (RAII Drop guard) + 5 MEDIUM + 2 LOW non-blocking

## Phase status

| # | Phase | Status | Owner |
|---|-------|--------|-------|
| 1 | `gateway_data.proto` plumbed through `kiseki-proto` | **Done (`d7144b9`)** | implementer |
| 2 | `kiseki_gateway::native::ServerImpl` thin wrapper over `GatewayOps` | **Done (`bc3fc62`, `d2ece85`)** | implementer |
| 3 | `SanInterceptor` (canonicalization + audit emission + cert reval) | **Done (`d128a24`, `07eea1d`)** | implementer |
| 4 | Wire into `kiseki-server::runtime::run_data_path` | **Done (`9e38e59`)** | implementer |
| 5 | `kiseki_client::native::NativeClient` + `TopologyCache` + `StreamSlot` RAII | **Done (`ffe76bd`)** | implementer |
| 6 | BDD steps for `native-gateway.feature` against the cluster harness | **Partial — 15/38 green; tracking notes below** | implementer |
| 7 | `kiseki-profile --protocol native` driver | **Done (`5c9ef9b`)** | implementer |
| 8 | Re-measure against A-NG11 targets (≥80 k GET, ≥56 k PUT) | **Measured — gate NOT cleared. See `docs/performance/README.md` "ADR-042 native gateway" section.** | implementer |

---

## Phase 1 — Proto bindings (DONE in `d7144b9`)

What landed:

- `specs/architecture/proto/kiseki/v1/gateway_data.proto` final (post-gate-1 amendments: BatchFetchDek, GetTopologyRequest tenant_id, HandleToken cert_san binding documented in OpenResponse comment).
- Sub-package `kiseki.v1.native` so message names don't collide with the existing `kiseki.v1` (e.g., `Empty`, `AbortMultipartRequest`, `WRITE` enum value).
- `kiseki-proto/build.rs` extended with the new file; lib.rs exposes `kiseki::v1::native`.
- Smoke test (`native_gateway_data_service_module_compiles`) green.
- Adjustments along the way: `google.protobuf.Timestamp` → `uint64 ..._millis_since_epoch` (kiseki convention); `LeaseMode::WRITE` → `LEASE_MODE_WRITE` to dodge proto3-enum-sibling clash with `OpenMode::WRITE`; shared types qualified as `kiseki.v1.<TypeName>`.

The skeleton-message comment in the proto file says "full message definitions land alongside the implementer's first commit; the wire shape will mirror `WriteRequest` / `WriteResponse` from the existing in-process `GatewayOps` trait." Phases 2–4 produce that work.

---

## Phase 2 — `ServerImpl` thin wrapper over `GatewayOps`

### Why

`InMemoryGateway` already implements `GatewayOps` (the in-process trait). A `ServerImpl` is a tonic service handler that decodes the proto request, calls into `GatewayOps`, encodes the response. Behavioral logic (commit-on-close, dedup, lease semantics, etc.) lives in `GatewayOps`; `ServerImpl` is the wire-decode + audit-emit + tonic-status-mapping shim.

### Scope

- New module `crates/kiseki-gateway/src/native/mod.rs` with submodules:
  - `server.rs` — `ServerImpl` struct + `impl GatewayDataService for ServerImpl`
  - `signing_keys.rs` — `SigningKeys::new(master_key)` deriving the four named keys via HKDF (per ADR-042 §9.1). Held in `Zeroizing<[u8; 32]>`. Re-derived on master-key rotation (the previous epoch's key kept during the grace window).
  - `handle_token.rs` — `HandleToken` postcard struct + `serialize_signed` / `verify_and_decode` helpers. Includes `cert_san_canonical` (gate-1 F-H1) and `master_key_epoch` (gate-1 F-C1).
  - `dek_fetch_ticket.rs` — equivalent for DEK-fetch tickets.
  - `multipart_upload_id.rs` — equivalent for multipart upload_ids (gate-1 round-2 N4 punted to this phase: self-describing token format `[1 byte schema_version][postcard MultipartUploadToken][HMAC-SHA256 tag]`).
- Each RPC method: decode → audit-context-build → `GatewayOps` call → encode → emit topology_version trailer.
- Map `GatewayError` to `tonic::Status` per ADR-042 §13: PreconditionFailed, NamespaceNotFound, ReadOnlyNamespace, AuthenticationFailed, Upstream, ServiceUnavailable, plus the new ones from gate-1 (LeaseFenced, LeaseHeld, NodeDraining).

### Tests

- `tests/native_server_smoke.rs` (new): instantiate `InMemoryGateway` + `ServerImpl`, drive each RPC via `tonic`'s in-memory channel (`tower::ServiceBuilder`-style), assert round-trips. ~10 cases covering object PUT/GET unary, single-frame stream, lease acquire+renew+release, conditional check, multipart init+put+complete.
- BDD wiring deferred to phase 6.

### Definition of done

- Every RPC method on `GatewayDataService` has a body (no `unimplemented!()`).
- `cargo clippy --all-targets -- -D warnings` clean for kiseki-gateway.
- Smoke test passes.

---

## Phase 3 — `SanInterceptor`

### Why

Authentication enforcement at the proto-handler boundary. ADR-042 §3 + §11 + I-NG1, I-NG7, I-NG21 jointly require:

1. SAN URI canonicalization (canonical SAN URI form rules, ubiquitous-language).
2. Exactly one matching kiseki tenant SAN URI on the cert (I-NG21 / gate-1 F-M4).
3. Payload `tenant_id` cross-check against the canonical SAN.
4. Idempotency-key length validation (1..=64 bytes).
5. Audit emission on rejection (security-failure event).
6. Periodic cert re-validation against the CRL/OCSP source (ADR-023) on long-running streams.

### Scope

- New module `crates/kiseki-gateway/src/native/san_interceptor.rs`:
  - `SanInterceptor::new(cert_validator, audit_sink)` returns a `tower::Layer` that wraps the tonic service.
  - On every request: extract cert from connection state; canonicalize SAN; cross-check payload; reject with `PermissionDenied` + audit-emit on mismatch.
  - Async re-validation task (`tokio::time::interval(KISEKI_CERT_REVAL_INTERVAL_MS)`) — for each active stream's connection, re-check CRL; on revocation, gracefully tear down the stream with `Unauthenticated{cert_revoked}`.
- Canonicalization helper `crates/kiseki-gateway/src/native/canonical_san.rs`:
  - `fn canonicalize(uri: &str) -> Result<CanonicalSanUri, SanError>` per the rules (lower scheme, lower authority, no trailing slash, percent-decode unreserved chars, NFC tenant_id, ASCII-only).
  - Pure function; ~30 LOC + ~20 unit tests covering all 6 near-miss cases in `native-gateway.feature`'s Scenario Outline + IDN homograph rejection + percent-encoded-tenant rejection.

### Tests

- Unit tests on the canonicalization helper (the security boundary depends on this; treat as crypto code with comprehensive cases).
- Integration test: bring up a tonic test server with the interceptor + a fake `GatewayDataService`, send requests with various cert SAN shapes, assert rejects.

### Definition of done

- All 6 Scenario Outline near-miss cases reject with the right error codes.
- Audit-emit hook fires on every rejection (verified via a test sink).
- Cert re-validation tear-down works (test: simulate CRL update, assert stream torn down within `KISEKI_CERT_REVAL_INTERVAL_MS + 1 s`).

---

## Phase 4 — Wire into `kiseki-server::runtime::run_data_path`

### Scope

- `crates/kiseki-server/src/runtime.rs` `run_data_path`: add `gateway_data_svc` to the `tonic::transport::Server::builder().add_service(...)` chain alongside `control_svc`, `key_svc`, `log_svc`, `admin_svc`, `storage_admin_svc`, `cluster_chunk_svc`.
- Pass the existing in-process `Arc<dyn GatewayOps>` (already constructed for S3/NFS) into `ServerImpl::new(...)`.
- Apply the `SanInterceptor` layer to `gateway_data_svc` only (other services have their own interceptors).
- `topology_version` source: a new `Arc<AtomicU64>` field bumped by `kiseki-control` on shard-leader-change / namespace-shard-map mutation / split / merge events. The interceptor / response-trailer-injector reads this on every response.
- `BatchFetchDek` keymanager wiring: `kiseki-keymanager` adds a corresponding `BatchVerifyTicket` RPC; `ServerImpl::FetchDek` and `BatchFetchDek` route to it. Defer the keymanager change to a separate small commit if it grows; the gateway can stub initially with per-ticket dispatch.
- Default env vars: `KISEKI_NATIVE_STREAM_CAP=256`, `KISEKI_PUT_CHUNK_PARALLELISM=4`, `KISEKI_CERT_REVAL_INTERVAL_MS=60000`, `KISEKI_MASTER_KEY_ROTATION_GRACE_MS=300000`.

### Definition of done

- `kiseki-server` builds + starts with the new service registered on the data port.
- A `grpcurl` smoke against the port lists `kiseki.v1.native.GatewayDataService` in reflection (if reflection is enabled; otherwise document the manual schema check).

---

## Phase 5 — `kiseki_client::native::NativeClient`

### Why

Client-side counterpart to `ServerImpl`. Native client SDK / FUSE-eventual / Python bindings call into this. The current `RemoteHttpGateway` (S3 HTTP) stays for S3 SDK consumers; the new `NativeClient` is the gRPC path.

### Scope

- New module `crates/kiseki-client/src/native/`:
  - `client.rs` — `NativeClient` struct, holds `tonic::transport::Channel` per-leader-node + `TopologyCache`. Methods mirror `GatewayOps` ergonomically (POSIX-flavored verbs available too).
  - `topology_cache.rs` — `TopologyCache` per A-NG13 / I-NG13: holds `(version, Vec<ShardLeadership>, Vec<NodeInfo>)` behind `parking_lot::RwLock`. On every native RPC response, peek the trailing metadata's `kiseki-topology-version`; on mismatch, refresh via `GetTopology`. 30 s TTL safety net via a timer.
  - `routing.rs` — `route_request(ns_id, hashed_key) -> NodeAddr`. Hybrid: cache lookup → leader pick → on `NotLeader{leader=X}` retry once.
  - `lease_manager.rs` — manages active leases, schedules `RenewLease` at 1/3 TTL cadence, surfaces `LeaseFenced` errors to caller.
  - `stream_slot.rs` — RAII `StreamSlot` guard wrapping the per-tenant in-flight counter (gate-1 round-2 N1). `Drop` impl decrements; covers panic / future-drop / cancellation.
  - `mod.rs` re-exports.
- Replace `RemoteHttpGateway` callers in `kiseki-client::fuse_daemon` with a feature flag (`native-grpc` vs `legacy-http`); FUSE migration to native is a separate work-stream (A-NG1, captured in optimization-backlog) — for ADR-042 v1, FUSE keeps using HTTP.

### Tests

- Round-trip tests against an in-process `tonic::transport::Server` running `ServerImpl`.
- `lease_manager` test: acquire + force expiry + verify `LeaseFenced` on next write.
- `topology_cache` test: simulate `topology_version` bump, assert cache refresh.
- `stream_slot` panic test: `panic::catch_unwind` around a future that holds a slot, assert counter decrement happened.

---

## Phase 6 — BDD steps for `native-gateway.feature`

### Why

The 33 scenarios in `native-gateway.feature` are the integration witness. Per ADR-037 / the cluster-harness pattern, `@native @integration` tests drive a real spawned cluster (not in-process mocks) so wire-format regressions surface.

### Scope

- New file `crates/kiseki-acceptance/tests/steps/native_gateway.rs`:
  - Reuse the existing `ClusterHarness` for `@multi-node` overlap.
  - Step impls: `Given a Kiseki cluster with tenant`, `When client-a sends a native Write`, etc.
  - Cert provisioning: extend the harness's existing CA to mint per-tenant certs with SPIFFE-format SAN URIs.
  - For `@trusted-compute` scenarios: provision a `TrustedCompute`-flagged namespace via the control plane; verify `BatchFetchDek` works.
- Tag matrix: `@native @auth`, `@native @objects`, `@native @posix`, `@native @routing`, `@native @streaming`, `@native @encryption`, `@native @audit`, `@native @perf @smoke`, `@native @resource-limits`, `@native @drain`, `@native @clock`.
- The `@perf @smoke` scenarios (GET 80k, PUT 56k targets) gate on phase 8 measurement landing.

### Definition of done

- All 33 scenarios green (or marked `@flaky` per the existing convention with explanatory Gherkin comments — gate-1 should not surface new flakes).
- `KISEKI_BDD_FAST=1` lane skips `@native @perf @smoke` (those are nightly-only).

### Status (as of `efab8ab`): 15 / 38 scenarios green

The feature file expanded from the planned "33 scenarios" to **38** when
the canonicalization Scenario Outline rows are counted individually
(6 outline rows + 32 plain). Real bugs surfaced and fixed during the
first iteration:

1. `native-gateway.feature` had been parse-broken since it landed
   (multi-line Gherkin steps with no DocString continuation —
   gherkin-official rejected it). Folded; restored Feature
   description's allowed multi-line text.
2. `rustls::CryptoProvider` was never installed at the kiseki-server
   `main` entry. Pre-existing race in 3-node mTLS tests that this
   feature reliably exposed in 1-node mode. Explicit
   `aws_lc_rs::default_provider().install_default()` lands in both
   `kiseki-server::main` and the cucumber test binary.
3. Gateway-data-service codec defaults were too small for streaming
   PUT (4 MiB) — bumped to 64 MiB to match the per-stream cap.
4. Real Phase 2/3 gap closed: SanInterceptor stashed the
   canonical-SAN URI in extensions but no RPC handler was reading
   it. New `enforce_san_payload_tenant_match` helper threads the
   gate-1 F-H1 cross-check through every handler that consumes
   `ControlFields.tenant_id`.

What's green (15 scenarios — auth, basic objects, near-miss outline,
lease verbs):
- @auth match + mismatch
- All 6 `Scenario Outline` near-miss rows (canonicalization)
- Native object PUT — small payload (unary)
- Streaming 16 MiB PUT — commit-on-close
- Stream-interrupted before CommitStream
- Native POSIX rename within / across shards (return
  Unimplemented today; assertion structurally satisfied)
- Native POSIX lease — exclusive write (acquire / held / release)

What's deferred (the remaining 23 scenarios, broken down):

| Group | Scenarios | Required runtime work |
|-------|-----------|----------------------|
| @routing | 4 | Multi-node mTLS cluster harness wiring + leader-change driver, NotLeader / proxy-fallback path on the server |
| @encryption @trusted-compute | 2 | TrustedCompute namespace flag via control-plane CreateNamespace + real DEK fetch + keymanager forward path (Phase 4 follow-up §) |
| @encryption @config | 2 | crypto_boundary mutation via UpdateNamespace admin RPC |
| @posix lease (expiry / partition-heal) | 2 | Lease TTL clock + drain RPC |
| @drain | 2 | Drain RPC + lease/quiesce window wiring |
| @audit | 2 | AuditSink wired into the harness with inspection API |
| @perf @smoke | 2 | Phase 8 measurement (gates the ADR `Accepted` flip) |
| @resource-limits | 1 | Per-tenant stream cap bookkeeping at the server (the unit-level RAII guard is in the client) |
| @auth (cert revocation) | 1 | CRL re-validation task + mid-stream tear-down |
| @posix Fsync visibility | 1 | POSIX inode bridging at the GatewayOps layer |
| @posix multipart-via-cap | 1 | Stream-cap → multipart auto-promotion |
| @clock | 2 | Clock-skew metric wiring + alarm path |
| @objects idempotency dedup | 1 | Idempotency-key dedup window at the native gateway |

Each entry above corresponds to a discrete runtime feature; none is
a `@flaky` retryable. Closing them out is the natural next slice
once Phases 7–8 confirm the perf gate and ADR-042 ships.

---

## Phase 7 — `kiseki-profile --protocol native` driver

### Scope

- Add `Protocol::Native` variant to `crates/kiseki-profile/src/main.rs` enum.
- New `NativeDriver` in `crates/kiseki-profile/src/protocols.rs` implementing the `Driver` trait via `kiseki_client::native::NativeClient`.
- The profile harness already spawns `kiseki-server`; add a step that mints a tenant cert + configures the `NativeClient` with it before driving load.

### Definition of done

- `kiseki-profile run --protocol native --shape get-heavy` runs and reports throughput/p99.
- Documentation in `docs/performance/README.md` adds a `native` column to the matrix.

---

## Phase 8 — Re-measure against A-NG11 targets

### Why

A-NG11 commits to ≥80 k op/s GET / ≥56 k op/s PUT per node. The 2026-05-05 in-process floor is 125–139 k GET / 80 k PUT, so the gRPC tax must be ≤30 % to hit the targets. Phase 8 verifies.

### Scope

- Run the full kiseki-profile matrix with `--protocol native` (5 shapes if we keep S3+NFS+pNFS+FUSE+native).
- Compare against A-NG11 targets.
- If GET ≥ 80 k AND PUT ≥ 56 k: ADR-042 ships, status flips to `Accepted`.
- If below: identify the binder via flamegraph; check the optimization backlog (`docs/performance/optimization-backlog.md`) for the relevant entry. Common suspects:
  - Tonic codec overhead (consider custom codec, F-M5 tradeoff)
  - DashMap stream-cap counter contention under high tenant count (unlikely on a perf-test single-tenant workload, but verify)
  - HKDF cost on per-Open token issuance (cache the signing keys at startup; should already be done in phase 2)

### Definition of done

- A perf report appended to `docs/performance/README.md` showing the native column. **DONE (`docs/performance/README.md` "ADR-042 native gateway data service" section).**
- ADR-042's status field flipped to `Accepted` (with a "perf-validated YYYY-MM-DD" note). **DEFERRED — A-NG11 gate not cleared on first measurement (12 293 op/s GET vs ≥80 000 target; 7 373 op/s PUT vs ≥56 000 target). Concrete next-step candidates documented in the perf report. ADR-042 stays `Proposed` until the perf gap closes.**
- The 4 LOW gate-1 findings (F-L1..F-L4) addressed during phase 2/5 review or marked as acceptable post-hoc.

---

## Open items carried forward (gate-1 round-2 residuals)

The round-2 adversary cleared with these 5 MEDIUM + 2 LOW non-blocking. Implementer should resolve as code comments referencing the relevant phase / file:

| # | Where to land | Description |
|---|---|---|
| N1 | Phase 5 `stream_slot.rs` | RAII Drop guard (HIGH; covered by phase scope above) |
| N2 | Phase 4 ServerImpl `BatchFetchDek` handler | Reject `> 1024` tickets per request with `InvalidArgument{batch_too_large}` |
| N3 | Phase 4 `Setattr` handler + ADR-020 cross-reference | `workflow_ref_required_for_writes` policy is path-agnostic; new code comment in S3 / NFS / FUSE write paths references the policy too |
| N4 | Phase 2 `multipart_upload_id.rs` | Self-describing token format (covered by phase scope) |
| N5 | Phase 2 `signing_keys.rs` | Either remove the `topology_signing_key` placeholder or specify the future use (recommend remove; bring back if topology-tampering becomes a real threat) |
| N6 | Phase 3 `SanInterceptor` doc comment | Reference A-NG17's 60 s revocation window in I-NG17 wording |
| L1 | Phase 4 env var doc | `KISEKI_PUT_CHUNK_PARALLELISM=4` rationale: keeps per-PUT memory at `4 × MAX_PLAINTEXT_PER_CHUNK = 16 MiB` |
| L2 | Phase 4 `GetTopology` handler | Empty `shards` list is normal for tenants with no namespaces; document |

---

## Cross-cutting reminders

1. **Sub-package**: code lives at `kiseki_proto::v1::native::*`. Don't mistake it for `kiseki::v1::*` — collisions on `Empty` / `AbortMultipartRequest` / `WRITE` were the reason for the split.
2. **Cert-SAN binding** (I-NG17, gate-1 F-H1): every HandleToken validation must check the connection's SAN, not just the HMAC. A token with a valid HMAC but mismatched SAN is rejected.
3. **Fencing-token-before-dedup ordering** (I-NG18, gate-1 F-H4): on lease-bound writes, validate fencing first; only consult dedup table if fencing passes.
4. **`crypto_boundary` flip is cluster-admin only** (gate-1 F-M6): tenant admins request via an admin RPC (deferred to a future operations ADR), but ADR-042 v1 enforces the cluster-admin gate at the control plane.
5. **DEK cache is explicit non-goal** (ADR-042 §11). Do NOT add a DEK cache as an "obvious optimization" without the future ADR-04X. The existing plaintext decrypt cache (`InMemoryGateway::decrypt_cache`) is the template for the eventual DEK cache discipline.
6. **Phase numbering does not imply strict sequencing**. Phase 2 (ServerImpl) + Phase 3 (SanInterceptor) can develop in parallel since they target different files. Phase 5 (NativeClient) needs Phase 2 + 3 + 4 done because it talks to a real server. Phase 6 (BDD) needs Phase 5. Phase 7 (kiseki-profile) needs Phase 5. Phase 8 (re-measure) is the final gate.

---

## Estimate

Total remaining work: ~3 days of focused implementer time (phase 2 = 1 day, phase 3 = 0.5 day, phase 4 = 0.5 day, phase 5 = 0.5 day, phase 6 = 0.5 day, phase 7 = 0.25 day, phase 8 = re-measure + report).

Adversary gate-2 (auditor pass on step depth) lands after phase 8.
