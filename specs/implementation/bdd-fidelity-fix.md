# Fix BDD Test Fidelity: Real Server Harness + Role Hardening

## Context

The BDD suite (214 scenarios, 1719 steps "passing") tests in-memory
mocks, not the running server. GCP deployment exposed: NFS writes fail,
S3 chunk replication returns "quorum lost", FUSE doesn't connect — none
caught by BDD. The Python e2e suite catches these because it runs a
real `kiseki-server` binary.

**Root causes:**
- `KisekiWorld` holds `InMemoryGateway`, `MemShardStore`, `MemKeyStore`
- Steps call `w.gateway.write()` directly — no network
- 431 step bodies are empty `{}` — silently pass
- 609 steps only set World fields — test the test
- Implementer role says "real path" but doesn't define it
- Auditor role defines THOROUGH as calling mock objects

## Part A: Role + Guideline Text Changes

### A1. `.claude/roles/implementer.md` — replace lines 35-43

Replace ambiguous "wire production code through real integrated path"
with concrete network-only rules:

- BDD steps MUST use gRPC (`world.grpc_channel`), HTTP (`world.http_client`),
  or TCP socket — never call domain objects directly
- MUST NOT: have empty body (use `todo!()`), call `w.gateway.*` or
  `w.log_store.*`, set World fields as sole assertion
- Add litmus test: "if removing all kiseki-* deps except kiseki-proto
  makes the step fail to compile, it's calling library code, not the server"
- Add banned patterns table with examples and fixes

### A2. `.claude/roles/auditor.md` — replace lines 8-27

- Add NETWORK depth level (required for @integration)
- Redefine THOROUGH → rename to NETWORK
- MOCK depth = calls domain objects = acceptable for @unit ONLY
- Gate 2 checks: empty body scan, domain import scan, tautology scan
- The example on line 24-27 (calling `gateway.write()` is THOROUGH)
  is exactly what's broken — replace with gRPC/HTTP example

### A3. `.claude/guidelines/engineering.md` — add after line 42

- BDD Step Fidelity section: @integration steps use network clients only
- KisekiWorld holds running server + clients, not domain objects
- Forbidden: importing production crates in @integration steps

## Part B: BDD Harness Rewrite

### B1. New KisekiWorld struct

Replace in-memory domain objects with:
```
server_process: Option<Child>     // running kiseki-server
server_data_dir: Option<TempDir>  // per-feature isolation
ports: AllocatedPorts             // ephemeral port block (8 ports)
grpc_channel: Channel             // tonic
log_stub: LogServiceClient        // gRPC
key_stub: KeyManagerServiceClient // gRPC
http_client: reqwest::Client      // S3 HTTP
s3_base: String                   // http://127.0.0.1:{port}
last_status, last_body, last_etag // response state (not domain state)
names: HashMap<String, String>    // Gherkin name → UUID
```

### B2. Server lifecycle

- One server per feature file (shared across scenarios, ~2-3s startup)
- Cleanup between scenarios via gRPC admin calls
- Port allocation: bind 8 ephemeral TCP sockets, record, close, pass
  to server — avoids conflicts in parallel
- Readiness: retry gRPC connect (60s) + S3 PUT probe (30s)
  Same pattern as `tests/e2e/helpers/cluster.py`

### B3. Cargo.toml changes

Add: `reqwest`, `tonic`, `kiseki-proto`, `tempfile`
Eventually remove (Phase 8): all `kiseki-*` except proto + common

### B4. Migration phases

| Phase | Files | What changes |
|-------|-------|-------------|
| 0 | acceptance.rs + harness.rs | Server lifecycle, 1 smoke gRPC roundtrip |
| 1 | gateway.rs, protocol.rs | `w.gateway.write()` → HTTP PUT |
| 2 | log.rs, composition.rs | `w.log_store` → gRPC LogService |
| 3 | control.rs, admin.rs | gRPC ControlService |
| 4 | kms.rs, crypto.rs | gRPC KeyManagerService; crypto stays @unit |
| 5 | advisory.rs, view.rs, chunk.rs | gRPC + HTTP |
| 6 | raft.rs, cluster.rs | Multi-node (spawn 3 servers) |
| 7 | pnfs.rs, client.rs | TCP for NFS, HTTP for FUSE |
| 8 | cleanup | Remove in-process deps from Cargo.toml |

### B5. What stays @unit (in-process, acceptable)

- Crypto primitives (AES-GCM, HKDF) — pure functions
- EC encode/decode — math
- Block device I/O — device layer
- Budget enforcer — pure logic

Tag `@unit` explicitly. May import production crates.

### B6. NFS

Full mount tests need root → stay in Python e2e.
BDD can test NFS wire framing via TCP without root.
Mount-requiring scenarios get `@e2e-deferred`.

## Verification

Per phase:
1. `cargo test -p kiseki-acceptance` — scenarios pass
2. `grep -rn 'async fn.*{}' steps/` — zero empty bodies in migrated files
3. `grep -rn 'w\.gateway\.\|w\.log_store\.' steps/` — zero direct calls
4. Kill server binary → tests fail (proves real dependency)

Final proof (Phase 8):
- Remove all kiseki-* deps except proto + common
- `cargo check -p kiseki-acceptance` passes
- All @integration scenarios pass
- **Server binary is required. No server = no tests.**

## Critical files

| File | Change |
|------|--------|
| `.claude/roles/implementer.md` | Network-only BDD rules, banned patterns |
| `.claude/roles/auditor.md` | NETWORK depth, gate 2 domain-import check |
| `.claude/guidelines/engineering.md` | BDD fidelity section |
| `crates/kiseki-acceptance/tests/acceptance.rs` | New World, server lifecycle |
| `crates/kiseki-acceptance/Cargo.toml` | Add clients, eventually remove prod crates |
| `crates/kiseki-acceptance/tests/steps/*.rs` | All — incremental per phase |
