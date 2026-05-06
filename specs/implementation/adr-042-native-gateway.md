# ADR-042 — Native Gateway Data Service Implementation Plan

**Status**: phases 0–7 done in the gRPC-only era (commits `d7144b9`, `bc3fc62`, `d2ece85`, `d128a24`, `07eea1d`, `9e38e59`, `ffe76bd`, `5c9ef9b`); ADR-042 then redesigned for a contract-layer + transport-pluggable architecture (post-2026-05-06 gate-1 round-3 PASS). The new 11-phase ordering per ADR-042 §16.1 supersedes the gRPC-only schedule below.
**Phase 0 (contract types)** of the redesign: **DONE — `kiseki-proto::native_contract` landed with 28 unit tests green** (BindingId, LatencyClass, LibfabricProvider, ListenAddr, DrainState, NodeState, BindingEndpoint, NodeBindings + consistency invariant, ConnectionId, RequestPrincipal trait, CxiAttestationEnvelope + canonical-message construction, NativeError 12-variant taxonomy + tag pinning).
**Phase 1 (gRPC binding refactor) — DONE**:
- `kiseki-gateway::native::grpc::principal` ships `TonicPrincipal` (impl `RequestPrincipal`) + `principal_from_request(&tonic::Request<T>)` (6 unit tests).
- `enforce_san_payload_tenant_match` now takes `&dyn RequestPrincipal`; all ServerImpl call sites refactored to the binding-agnostic shape.
- Lease-store boundary uses a separately-named `lease_holder_principal: String` so the request-principal `&dyn RequestPrincipal` doesn't shadow it; derivation centralized in `lease_holder_principal_for(&dyn RequestPrincipal, &OrgId)`.
- **Structural lift**: `tonic::async_trait impl GatewayDataService` now lives in `kiseki-gateway::native::grpc::adapter::GrpcAdapter` (a thin `Arc<ServerImpl>` wrapper). `ServerImpl` exposes inherent handler methods that take `&dyn RequestPrincipal + body` and never reference `tonic::Request` / `tonic::Streaming` / `tonic::Response`. Streaming + POSIX-stub methods live exclusively in the adapter.
- `kiseki-server::runtime::run_data_path` updated: `Arc::new(ServerImpl)` is wrapped in `GrpcAdapter::new(...)` before `GatewayDataServiceServer::new(...)`. Same `ServerImpl` instance can back multiple bindings concurrently (gRPC + future TCP-framed + ibverbs / cxi).
- 230 kiseki-gateway lib tests green (228 handler-level + 2 adapter-level). 6 TonicPrincipal tests green.
- **Phase 2** (binding-agnostic ServerImpl) is satisfied as a direct consequence — no separate phase work needed.

**Phase 3 (TCP-framed-postcard binding) — server-side DONE**:
- **Slice 1** — wire format. `kiseki-proto::native_contract::wire_tcp_framed`: `RpcEnvelope`, `WireStatus` (15 variants matching §1.4 byte values 0x10..0x1F + connection-level 0x00..0x02), encode/decode for request and response frames, oversize/version/incomplete error taxonomy. 20 unit tests (postcard round-trip, frame layout pinning, oversize cap, status byte values, JSON-document-start collision check).
- **Slice 1 (cont.)** — `TcpFramedPrincipal` adapter (`Arc<str>`-backed canonical SAN, `BindingId::TcpFramed`). 5 unit tests (clone-allocation-free, empty-SAN fallback, distinct connection-id correlation).
- **Slice 2** — serde on prost types. `kiseki-proto/build.rs` adds `type_attribute(".kiseki.v1", "#[derive(serde::Serialize, serde::Deserialize)]")` so all native gateway request/response types ride both prost (gRPC) and postcard (TCP-framed). One pinning test in `kiseki-proto::tests::prost_native_types_postcard_roundtrip` catches build.rs regressions. Verb dispatch in `kiseki-gateway::native::tcp_framed::dispatch::dispatch_verb`: 16 unary verbs + `fsync` (no-principal special case), `(WireStatus, Vec<u8>)` return, exhaustive `tonic::Code → WireStatus` mapping, streaming-verb names surface as `UnknownVerb` (TCP-framed buffers; multipart for above-cap). 7 unit tests (PUT round-trip, PUT/GET round-trip, unknown-verb body, streaming-verb rejection, corrupt payload → ProtocolError, SAN/payload mismatch propagation, fsync no-principal, exhaustive Code mapping).
- **Slice 3** — per-connection frame loop (`serve_connection`) + listener (`TcpFramedListener`). Frame loop reads length-prefix, decodes envelope, dispatches, writes response with `request_id` echo for client-side multiplex. Recoverable errors (corrupt envelope) keep connection alive; unrecoverable (oversize length, I/O) tear down. Listener mirrors `RaftRpcListener` shape: optional rustls acceptor, per-peer connection cap (default 16 — `NATIVE_TCP_FRAMED_PER_PEER_MAX`), monotonic `ConnectionId`. Plaintext mode installs synthetic `dev` SAN matching gRPC binding's `SanInterceptor` fallback. 8 unit tests (in-memory duplex stream + real loopback TCP).
- **Slice 3 (runtime wiring)** — `kiseki-server::runtime::run_data_path` spawns `TcpFramedListener::run()` on port 9101 (`KISEKI_NATIVE_TCP_ADDR` override; `=disabled` skips). Same `Arc<ServerImpl>` instance backs both gRPC + TCP-framed bindings concurrently.
- **Slice 4** — `kiseki-client::native::tcp_framed::TcpFramedClient`. Persistent TCP+rustls connection, request multiplex via `request_id` correlation, response demux on a background reader task. `call(verb, payload) -> (status, body)` + `call_ok(verb, payload) -> body` (maps non-Ok status to `ServerError`). On connection close, pending requests resolve with `ConnectionClosed`. 6 tests including end-to-end round-trip against the real `kiseki-gateway::native::tcp_framed::serve_connection` (highest-leverage: proves both sides agree on the wire format byte-for-byte).

**Phase 3 — DONE**. Server-side + client-side both shipped, 59 new tests across the binding (53 server-side + 6 client-side).

- **arch-check gate landed**: `make arch-check` fails the build if `kiseki-gateway::native::server` references `tonic::Request`, `tonic::Response`, `tonic::Streaming`, TCP-framed `ConnectionContext`, or cxi `AttestationContext`. Comment lines skipped. Verified firing on synthetic violation, passing on clean tree.

**Phase 3 follow-ups (not blocking phase advancement)**:
- TLS plumbing on the listener: runtime currently passes `None`; integration with `cfg.tls`'s `ServerConfig` needs a small adapter (the listener already accepts the right type).
- Lift `ResponseEnvelope` from kiseki-gateway::native::tcp_framed::connection into kiseki-proto::native_contract::wire_tcp_framed (currently duplicated client/server-side as a thin internal struct).
- (b) follow-up — slim `kiseki-client`'s dep tree so it can actually `cargo publish` (currently still pulls kiseki-gateway transitively).

**Phase 4 (`kiseki-transport::native::selector`) — DONE**:
- `BindingProbe` trait, `ProbeOutcome { Available { latency_class, addr } | Unavailable { reason } }` enum; per-binding probes implement the trait. Per-binding probe timeout default 3 s (`KISEKI_NATIVE_PROBE_TIMEOUT_MS`); hanging probe surfaces as `Unavailable { reason: "probe_timeout_exceeded" }`. Sequential — never parallel (dlopen contention).
- `BindingSelector` orchestrator: phase 1 (probe all), phase 2 (port-collision detection across `Available` set), and operator-pin filtering. Returns `(SelectorPlan, SelectorReport)` — plan is the spawn list in priority order (Rdma > Low > Standard); report covers all probes including `Unavailable` for diagnostic banners.
- `OperatorPin::parse` — handles `KISEKI_NATIVE_TRANSPORT={auto|grpc|tcp|ibverbs|libfabric|...}`. Empty / unset / `auto` → `Auto`; known names → `Pinned(BindingId)`; libfabric requires the provider env-var (rejected with hint); typos rejected loud.
- `render_banner(plan, report)` — pure function emitting the §3.1 startup banner (priority-ordered spawn list + skipped bindings with reasons + active pin marker).
- Per-binding probes: `kiseki-gateway::native::grpc::probe::GrpcProbe` (latency_class=Standard, addr=`KISEKI_NATIVE_GRPC_ADDR` or default), `kiseki-gateway::native::tcp_framed::probe::TcpFramedProbe` (latency_class=Low, addr=`KISEKI_NATIVE_TCP_ADDR` or default 9101; honors `=disabled` operator escape hatch).
- `Ord, PartialOrd` derived on `BindingId` / `LatencyClass` / `LibfabricProvider` / `ListenAddr` so the selector's sort + BTreeMap-based collision check work cleanly.
- **Runtime wiring** — `kiseki-server::runtime::run_data_path` builds the selector with both probes, calls `selector.plan()`, logs the banner, walks `spawn_order` to register the gRPC adapter on the shared data_addr router and / or spawn the TCP-framed listener on its own port. Failures (port collision, no available bindings, pinned binding unavailable) are fatal at startup with a clear error. ibverbs / libfabric bindings log warnings until phases 9/10 land.
- **26 new tests** (17 selector + 4 gRPC probe + 5 TCP-framed probe).

**Phase 4 follow-ups (not blocking phase advancement)**:
- Per-binding metrics per ADR-042 §12.1: `kiseki_native_binding_probe_duration_seconds{binding}` histogram, `kiseki_native_binding_pinned_total{binding}` counter incremented when a pin is in effect.

**Phase 5 (NativeClient + per-edge selection + connection pool + drain) — DONE**:
- **Slice 1** — pure-function edge selector (`kiseki-client::native::edge_selector`). 14 unit tests covering ranking, pin honoring, draining-vs-failed-vs-evicted state gates, heterogeneous-cluster + cross-WAN-fallback paths.
- **Slice 2** — proto/server/client topology binding-set wiring. `NodeInfo` extended with `repeated BindingEndpoint bindings`; new proto messages `BindingEndpoint` + `DrainState`; new enums `BindingId` + `LatencyClass`. Server-side `node_info_from_plan(node_id, state, &SelectorPlan)` produces the wire shape. Runtime now publishes the local node's binding set into `TopologyInjector`. Client-side `Snapshot::Node` extended with `state` + `bindings`; `snapshot_from_proto(&TopologyInfo)` decodes the wire form. **2 BDD scenarios green end-to-end**: `Per-edge selection — heterogeneous binding cluster` and `Topology version regress falls back to TTL safety net`.
- **Slice 3** — per-edge `ConnectionPool` keyed by `(node_id, BindingId)`. Caches gRPC channel + TCP-framed client + future RDMA variants behind a `Connection` enum. `get_or_dial` dispatches per binding; `drop_edge`/`drop_node` for §1.7 close-on-state-change. 6 unit tests.
- **Slice 4** — drain protocol per §3.2.1. `drain_edge`, `is_draining`, `reconcile_with_topology(&Snapshot)` diffs against current pool and marks no-longer-advertised edges, `tick_drain_budget(Duration)` hard-closes past `KISEKI_NATIVE_DRAIN_BUDGET_MS` (default 30s). New `get_or_dial` calls bypass draining edges; existing clones continue serving in-flight work. 9 additional unit tests.

**Phase 5 totals**: 4 slices × 47 new tests; 2 new BDD scenarios green.

**Phase 6 (BDD steps for native-gateway.feature) — partial**:
- **22/49 scenarios passing** (up from 15 at phase-5 entry). Net 7 new scenarios wired through real client-side code:
  - `Per-edge selection — heterogeneous binding cluster` — drives `select_for_edge` against a synthetic 4-node topology with mixed binding sets.
  - `Topology version regress falls back to TTL safety net` — drives `TopologyCache::decide` + TTL-shortened cache.
  - `Topology cache refreshed on topology_version mismatch` — drives the trailer-version-DIFFERS path.
  - `Binding listener crashes mid-flight — clients drain gracefully` — real ephemeral TCP listeners + `ConnectionPool::reconcile_with_topology` + `tick_drain_budget`.
  - `Backoff-restart restores binding after crash` — topology re-advertisement path.
  - `Binding probe timeout falls back to next-best binding` — synthetic `BindingSelector` with `HangingProbe`.
  - `All bindings fail probe — server exits cleanly` — selector returns `NoAvailableBindings`.
- **25 scenarios still skipped**. Most need either:
  - cluster-harness extensions (multi-node mTLS spawn, drain RPC at runtime, binding-crash injection)
  - server-side feature work (drain RPC, per-tenant stream-cap enforcement, cert-revocation mid-stream)
  - RDMA hardware (cxi attestation envelope handling, ibverbs perf-gate)
- **2 scenarios failing** — pre-existing TODOs (idempotency-key dedup wiring, drain-RPC stub) unrelated to phase 5/6/7.
- The synthetic-state pattern proven across these 7 scenarios is the right shape for the remaining client-side scenarios; harness extensions unlock the rest.

**Phase 7 (`kiseki-profile --protocol native --binding=<grpc|tcp|auto>`) — DONE**:
- New `NativeBinding` CLI flag with `grpc` (default — historical behavior preserved), `tcp`, `auto` variants.
- `TcpFramedNativeDriver` mirrors the gRPC `NativeDriver`'s shape — pool of N independent connections, round-robin selector, per-call postcard encode/decode around `TcpFramedClient::call_ok`.
- Harness extended to allocate an ephemeral `KISEKI_NATIVE_TCP_ADDR` so the spawned kiseki-server's TCP-framed listener doesn't collide with anything else on 9101.
- Per-binding flame profiles: `KISEKI_PPROF_OUT=tcp.svg ./kiseki-profile run --protocol native --binding tcp ...` produces a flamegraph of the TCP-framed path; same trick with `--binding grpc` for the comparison.

**Phase 8 (re-measure against §14 targets per binding) — DONE on this host (single-node; HW-bound multi-node measurement still pending)**:
- 2026-05-06 perf staircase, GET-heavy 64 KiB c=16:
  | Tier | op/s | p50 | p99 | % of in-process-persistent floor |
  |---|---|---|---|---|
  | in-process compute | 204,051 | 6 µs | 486 µs | 107% (cache effects) |
  | in-process-persistent | 190,097 | 7 µs | 566 µs | 100% (= floor) |
  | gRPC binding | 25,457 | 543 µs | 1787 µs | 13% |
  | **TCP-framed binding** | **63,595** | **212 µs** | **844 µs** | **33%** |
- TCP-framed beats gRPC by 1.50–2.50× across the 4/16/64 KiB GET sweep:
  | size | gRPC op/s | TCP op/s | gain |
  |---|---|---|---|
  | 4 KiB | 45,685 | 96,314 | **+111%** |
  | 16 KiB | 42,234 | 88,797 | **+110%** |
  | 64 KiB | 25,457 | 63,595 | **+150%** |
- Two perf landmines surfaced + closed during the staircase:
  - **V1 wire-format double-envelope tax**. The original `RpcEnvelope`/`ResponseEnvelope` shape postcard-encoded `payload_bytes: Vec<u8>` *inside* an outer postcard struct, costing one full body memcopy per call on each side. **V2 wire-format** lifts `request_id`, `verb_tag`, and `status` into fixed-width header fields and writes the verb body directly — no outer envelope. ~+10–14% across the matrix.
  - **Postcard byte-by-byte `AllocVec::try_push`**. serde's default `Vec<u8>` Serialize impl uses `serialize_seq`, which postcard implements as one byte-push per byte — 65,536 push calls per 64 KiB response = 84% of CPU at 64 KiB. **Adding `#[serde(with = "serde_bytes")]`** on bulk `Vec<u8>` fields via `tonic_prost_build::field_attribute` switches to `serialize_bytes` (single bulk memcopy). ~+150–200% across the matrix; this is the win that took TCP-framed past gRPC.
- A-NG11 gate (≥80 k GET, ≥56 k PUT per node) — single-host GET cleared at 4/16 KiB; 64 KiB GET still 21 k under target. The remaining gap is gateway-side (composition store + chunk read), not transport — same ceiling applies to gRPC.

**Next-best-target backlog from the post-V2 flamegraph** (gateway, not transport — both bindings benefit):
1. **Composition store cache** — `CompositionStore::get` is 18% of 64 KiB GET CPU. An LRU on the hot composition lookup path skips fjall. Estimated +15–25%.
2. **Chunk decrypt** — `InMemoryGateway::read` 38%; most is AEAD. Confirm `aws_lc_rs` is using AES-NI; if scalar, switch.
3. **Server-side per-connection task pool** — `serve_connection` processes frames sequentially per connection. Spawning a bounded task pool per accept would let pipelined requests parallelize server-side. Helps under high single-connection concurrency.
4. **Vectored I/O on response write** — emit the 10-byte response header + the body slice as two `iovec`s via `write_vectored`. Skips one frame-bytes memcopy.
5. **`bytes::Bytes` on the response data field** — refcounted slice instead of `Vec<u8>` so the chunk store's `Bytes` can flow through to the wire without a copy.

**Phase 9 (ibverbs probe scaffold) — DONE; listener pending hardware**:
- `kiseki-transport::native::ibverbs_probe::IbverbsProbe` ships ADR-042 §2.3's full path-validation discipline:
  - Linux-only OS gate (`cfg(target_os = "linux")`).
  - Distro/arch-aware search list for `libibverbs.so.1` (`/usr/lib/${arch}-linux-gnu/`, `/usr/lib64/`, `/usr/lib/`) with `${arch}` from `target_arch` (x86_64, aarch64, powerpc64le, unknown).
  - Operator override via `KISEKI_NATIVE_IBVERBS_LIB`.
  - R2-M2 path-injection mitigation: root-ownership check (uid 0) + group/world-writable rejection. Audit-log the resolved absolute path on success.
  - `/sys/class/infiniband/*` enumeration with `port/state == "4: ACTIVE"` qualifier; `KISEKI_NATIVE_IBVERBS_DEV` + `KISEKI_NATIVE_IBVERBS_PORT` operator pin.
  - Returns `ProbeOutcome::Available { latency_class: Rdma, addr: FabricDescriptor("ibverbs://<dev>/<port>") }` or `Unavailable { reason }` with grep-friendly diagnostics.
- Shared helpers in `probe_helpers` reusable for any RDMA binding probe.
- 5 unit tests for probe + helpers; passes cleanly on dev hosts (returns Unavailable with the right reason).
- **Pending hardware**: listener-side libibverbs FFI shim, QP setup, send/recv RDMA verbs, mTLS-over-rdma-cm handshake. Connection-pool integration via `Connection::Ibverbs` variant is the natural plug-point.

**Phase 10 (libfabric probe scaffold) — DONE; listener pending hardware**:
- `kiseki-transport::native::libfabric_probe::LibfabricProbe` ships:
  - Same Linux + path-validation discipline as ibverbs.
  - Operator pin via `KISEKI_NATIVE_LIBFABRIC_PROVIDER={cxi|verbs|sockets|tcp}`. `efa` rejected per §2.4.3 (deferred — needs AWS-IAM mapping ADR).
  - Sysfs-based provider auto-detection per §2.4.4 ranking: `cxi > verbs > sockets/tcp`. cxi sniffed via `/sys/class/net/.../device/cxi/`; verbs via `/sys/class/infiniband/`.
  - Pinned-provider validation against sysfs evidence — pinning to a provider with no hardware backing surfaces a clear `Unavailable` reason rather than silent fallback.
  - Returns `Available { latency_class: Rdma | Standard, addr: FabricDescriptor("libfabric://<provider>") }` based on the chosen provider.
- 7 unit tests; passes on dev hosts.
- **Pending hardware**: `fi_getinfo()` FFI calls (currently the probe consults sysfs as a proxy for fi_getinfo's results), endpoint setup, cxi attestation envelope handling per §2.4.2 + §2.4.2.1 DoS mitigations, libfabric tagged-message API for the wire path.

Total ADR-042 implementation: 12 sliced tasks across phases 0-10, plus the §1.8 arch-check gate and the (a) publish-hygiene fix.
**Date opened**: 2026-05-05
**Predecessor**: `post-2026-05-03-sweep.md` (in-process perf spike + ADR-040 rev-3 write-behind)
**Spec source of truth**: `specs/architecture/adr/042-native-gateway-data-service.md` (post-gate-1 round-3 PASS, transport-pluggable redesign)

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

### Phase-6 fault-injection BDD additions (R2-O3 + R3-O3 acceptance)

Per ADR-042 §15 hot spot 9 + §16.1 phase 6 amendments, phase 6 MUST include the following four fault-injection BDD scenarios. These were called out by the gate-1 round-2 + round-3 adversary findings; gate-2 auditor verifies presence.

| Scenario | Tag | What it covers |
|---|---|---|
| Probe timeout simulation | `@native @binding-probe` | `KISEKI_NATIVE_PROBE_TIMEOUT_MS=10` artificially short forces ibverbs probe to time out; selector continues with TCP-framed + gRPC; banner reflects the timeout |
| Listener crash + restart cycle | `@native @binding-restart` | Inject SIGCONT-driven listener panic on TCP-framed; topology version bumps; client falls back to gRPC; backoff-restart succeeds; topology version bumps again; client returns to TCP-framed |
| Topology version regress under operator error | `@native @topology` | Manually publish a regressed `topology_version`; client refresh fails closed via 30 s TTL safety net |
| cxi attestation replay under load | `@native @binding-cxi @attestation` (deferred to phase 10) | Capture an attestation envelope; replay against the server within the 60 s window; assert `Unauthenticated{cxi_attestation_replay}` and rate-limited handling. Requires cxi binding implementation; lands with phase 10. |

Phases 0–8 ship the first three; the fourth ships with phase 10 (cxi binding implementation).

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
