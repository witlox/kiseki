# Next Phase Plan: E2E → BDD Real → Wire Protocols

## Context

Remediation R0-R10 complete. 171 Rust tests, ~19 Go tests, 249/288 BDD
scenarios "passing." Core pipeline wired (Composition → Log → View).
But: BDD harness tests isolated stores, not the integrated pipeline.
No test crosses a process boundary. The 249 passing scenarios give
false confidence.

This plan addresses three gaps in order: prove the system works across
process boundaries (B), make BDD scenarios trustworthy (A), then add
real protocol surfaces (C).

---

## Phase B: Python E2E Test Suite (`tests/e2e/`)

**Goal**: First test that boots the real server binary, connects over
gRPC, writes data, reads it back — across process + network boundaries.

### B.1: Python scaffolding

Create `tests/e2e/` at project root:

| File | Purpose |
|------|---------|
| `tests/e2e/pyproject.toml` | Deps: pytest, grpcio, grpcio-tools, tenacity, pydantic |
| `tests/e2e/conftest.py` | `@pytest.fixture(scope="session")` — build binary, spawn subprocess, wait for health, teardown SIGTERM |
| `tests/e2e/helpers/__init__.py` | Package |
| `tests/e2e/helpers/cluster.py` | Subprocess lifecycle: spawn, health-wait (tenacity retry), teardown |

Server fixture pattern:
- `cargo build --release -p kiseki-server` (or use debug)
- Spawn with `KISEKI_DATA_ADDR=127.0.0.1:19100` (test port, no TLS)
- Health check via `KeyManagerService.Health` RPC with tenacity retry
- SIGTERM on teardown

### B.2: Python proto generation

| File | Purpose |
|------|---------|
| `tests/e2e/generate_proto.sh` | `python -m grpc_tools.protoc` against `specs/architecture/proto/kiseki/v1/{common,log,key}.proto` |
| `tests/e2e/proto/` | Generated `*_pb2.py` + `*_pb2_grpc.py` |

### B.3: Bootstrap shard in server

**Blocker**: `MemShardStore::new()` is empty. `LogService.AppendDelta`
to a non-existent shard returns `NOT_FOUND`. No `CreateShard` RPC exists.

**Fix**: Add `KISEKI_BOOTSTRAP_SHARD=true` env var to `runtime.rs`.
When set, creates a well-known shard (UUID v5 from "bootstrap") with a
well-known tenant. E2E tests use this deterministic ID.

| File | Change |
|------|--------|
| `crates/kiseki-server/src/config.rs` | Add `bootstrap: bool` from env |
| `crates/kiseki-server/src/runtime.rs` | Create shard on boot when `bootstrap=true` |

### B.4: E2E tests

| File | Tests |
|------|-------|
| `tests/e2e/test_server_health.py` | Server boots, KeyManager.Health returns epoch |
| `tests/e2e/test_log_roundtrip.py` | AppendDelta via gRPC → ReadDeltas → verify payload roundtrip |
| `tests/e2e/test_maintenance_mode.py` | SetMaintenance → AppendDelta rejected → clear → AppendDelta succeeds |

### B.5: Makefile integration

| File | Change |
|------|--------|
| `Makefile` | Add `e2e` target: `cd tests/e2e && pytest -m e2e -v` |
| `lefthook.yml` | NOT in pre-commit (too slow), manual only |

**Exit criteria**: `pytest tests/e2e/ -m e2e` passes 3+ tests against
a real subprocess. CI-runnable.

---

## Phase A: Make BDD Harness Real

**Goal**: Wire the pipeline in `KisekiWorld` so BDD scenarios test
integrated behavior, not isolated stores.

### A.1: Arc-wrap log_store in KisekiWorld

| File | Change |
|------|--------|
| `crates/kiseki-acceptance/tests/acceptance.rs` | Change `log_store: MemShardStore` → `log_store: Arc<MemShardStore>` |
| Same file | Wire `comp_store: CompositionStore::new().with_log(Arc::clone(&log_store))` |

`Arc<MemShardStore>` implements `Deref<Target=MemShardStore>`, so
existing step calls like `w.log_store.append_delta(req)` continue to
work via auto-deref. Minimal step file changes needed.

### A.2: Add stream processor helper

| File | Change |
|------|--------|
| `crates/kiseki-acceptance/tests/acceptance.rs` | Add `pub fn poll_views(&mut self)` method that runs `TrackedStreamProcessor` |
| `crates/kiseki-acceptance/Cargo.toml` | Add `kiseki-view` to deps (for `TrackedStreamProcessor`) |

### A.3: Upgrade Then steps with real assertions

Target: 10-15 composition/view scenarios where the Then step currently
just checks local state. Add assertions that verify cross-context effects:

**Composition steps** (`steps/composition.rs`):
- After create → assert delta exists in `log_store` with correct operation type
- After delete → assert tombstone delta in log
- After update → assert new version delta in log

**View steps** (`steps/view.rs`):
- After composition mutation + `poll_views()` → assert view watermark advanced
- Verify view transitions Building → Active after first delta consumed

**Log steps** (`steps/log.rs`):
- Mostly already real. Verify `Arc` deref works, no regressions.

### A.4: Document skipped scenarios

The 39 skipped scenarios (31 Go control-plane, 8 complex integration)
stay skipped. Document why in a comment at the top of the BDD runner.

**Exit criteria**: 249 BDD scenarios still pass. 10+ scenarios now
verify cross-context pipeline behavior (delta emission, watermark
advancement). Zero false positives on pipeline-relevant scenarios.

---

## Phase C: S3 Gateway (Wire Protocol)

**Goal**: First real protocol surface — S3 `GET`/`PUT` over HTTP.

### C.1: S3 HTTP server

The domain types already exist: `S3Gateway<G: GatewayOps>` with
`GetObjectRequest`/`PutObjectRequest`. What's missing is the HTTP layer.

| File | Change |
|------|--------|
| `crates/kiseki-gateway/Cargo.toml` | Add `axum`, `tokio` deps |
| `crates/kiseki-gateway/src/s3_server.rs` | Create: axum router with `GET /:bucket/:key`, `PUT /:bucket/:key` |

S3 API subset (minimum viable):
- `PUT /:bucket/:key` → `PutObject` → returns ETag
- `GET /:bucket/:key` → `GetObject` → returns body
- No SigV4 auth initially (all requests accepted, dev mode)

### C.2: Wire into server runtime

| File | Change |
|------|--------|
| `crates/kiseki-server/src/config.rs` | Add `s3_addr: SocketAddr` (default `:9102`) |
| `crates/kiseki-server/src/runtime.rs` | Spawn S3 HTTP server alongside gRPC |

### C.3: Python E2E for S3

| File | Tests |
|------|-------|
| `tests/e2e/test_s3_gateway.py` | PUT object via HTTP → GET back → verify content |
| `tests/e2e/helpers/s3.py` | `requests` or `boto3` helper (no SigV4 → use `requests` directly) |

### C.4: Cross-protocol test

| File | Tests |
|------|-------|
| `tests/e2e/test_cross_protocol.py` | Write via LogService gRPC → read via S3 GET (requires view materialization wired in server) |

**Exit criteria**: S3 gateway serves GET/PUT on `:9102`. Python e2e
tests verify S3 roundtrip + cross-protocol read.

### NFS and FUSE: Deferred

NFS (NFSv4.1 in Rust) and FUSE (macFUSE dependency) are high-risk and
deferred. Document specific dependency analysis and effort estimate
for future planning.

---

## Execution Order

```
B.1-B.2 scaffolding + proto ──→ B.3 bootstrap shard ──→ B.4 e2e tests
                                                              │
                                                              ▼
                                              A.1 Arc wiring ──→ A.2 stream proc ──→ A.3 assertions
                                                                                          │
                                                                                          ▼
                                                                        C.1 S3 HTTP ──→ C.2 server ──→ C.3 e2e
```

Each phase gets an adversarial review before proceeding to the next.

## Test Count Projections

| Phase | New Tests | Cumulative |
|-------|-----------|------------|
| Current | — | 171 Rust + 19 Go + 249 BDD = 439 |
| After B | +3 Python e2e | 442 |
| After A | +0 new, 10+ upgraded assertions | 442 (higher confidence) |
| After C | +3 Python e2e | 445 |

## Key Files

| File | Phases | Role |
|------|--------|------|
| `crates/kiseki-server/src/runtime.rs` | B, C | Server composition, bootstrap, S3 server |
| `crates/kiseki-server/src/config.rs` | B, C | Env var config |
| `crates/kiseki-acceptance/tests/acceptance.rs` | A | BDD World wiring |
| `crates/kiseki-acceptance/tests/steps/composition.rs` | A | Real assertions |
| `crates/kiseki-gateway/src/s3_server.rs` | C | New: axum S3 handler |
| `tests/e2e/conftest.py` | B, C | Server lifecycle fixture |
| `tests/e2e/test_log_roundtrip.py` | B | Core e2e test |
| `tests/e2e/test_s3_gateway.py` | C | S3 e2e test |
