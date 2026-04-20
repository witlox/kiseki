# Phase E: Test-Driven Depth — 5 Items

## Context

Phase D complete. 508 tests, 281/320 BDD, 44 adversarial findings.
14 fixed, rest tracked. Next 5 items address the highest-value gaps,
each driven by tests first (BDD scenarios where they exist, TDD where
they don't).

**Discipline**: For each item:
1. Identify or create BDD scenario / test
2. Run it — verify it fails (red)
3. Implement minimum code to pass
4. Verify green
5. Adversarial review before moving to next item

---

## E.1: NFS Directory Index (test-driven)

**Existing BDD**: 3 scenarios in protocol-gateway.feature (READDIR, CREATE,
small file). No LOOKUP scenario.

### E.1a: Add LOOKUP BDD scenario

Add to `specs/features/protocol-gateway.feature`:
```gherkin
Scenario: NFS LOOKUP — resolve filename to file handle
  Given a file "results.h5" was created via NFS WRITE
  When the NFS client issues LOOKUP for "results.h5"
  Then a valid file handle is returned
  And GETATTR on the handle returns the correct file size
```

Add step definitions to `crates/kiseki-acceptance/tests/steps/gateway.rs`.

### E.1b: TDD — directory index data structure

Create `crates/kiseki-gateway/src/nfs_dir.rs`:
- `DirectoryIndex` struct: `HashMap<(NamespaceId, String), (FileHandle, CompositionId)>`
- `insert(ns, name, fh, comp_id)`
- `lookup(ns, name) -> Option<(FileHandle, CompositionId)>`
- `list(ns) -> Vec<(String, FileHandle)>`
- `remove(ns, name)`

Unit tests first:
- insert + lookup roundtrip
- lookup miss returns None
- list returns all entries
- remove + lookup returns None

### E.1c: Wire into NFS ops

| File | Change |
|------|--------|
| `nfs_ops.rs` | Add `DirectoryIndex` to `NfsContext`, wire `lookup_by_name()` + `readdir()` |
| `nfs3_server.rs` | CREATE populates directory index with filename, LOOKUP resolves via index |
| `nfs4_server.rs` | OPEN(create) + LOOKUP use same index |

### E.1d: Python e2e NFS test

`tests/e2e/test_nfs_gateway.py`: Send raw NFS3 NULL + GETATTR via TCP
(python struct.pack for ONC RPC framing). Proves NFS server accepts connections.

**Exit**: NFS LOOKUP returns real file handles. READDIR returns filenames.
BDD scenario passes. Python e2e connects to NFS port.

---

## E.2: mTLS on S3 + NFS (test-driven)

**Existing BDD**: 9 scenarios in authentication.feature. Transport tests exist
for TLS handshake. No S3/NFS mTLS tests.

### E.2a: Python e2e mTLS test (red first)

Generate test certificates (self-signed CA + server + client certs) in
`tests/e2e/fixtures/certs/`. Create `tests/e2e/test_mtls.py`:

```python
@pytest.mark.e2e
def test_s3_mtls_valid_cert():
    """S3 with valid client cert succeeds."""

@pytest.mark.e2e
def test_s3_no_cert_rejected():
    """S3 without client cert returns TLS error."""
```

These fail initially (S3 TLS not wired).

### E.2b: Wire S3 TLS acceptor

| File | Change |
|------|--------|
| `s3_server.rs` | Accept `Option<Arc<rustls::ServerConfig>>`, wrap listener with `TlsAcceptor` |
| `runtime.rs` | Build `ServerConfig` from TLS files, pass to `run_s3_server()` |

### E.2c: Wire gRPC auth interceptor

| File | Change |
|------|--------|
| `crates/kiseki-log/src/grpc.rs` | `auth_interceptor()` extracts OrgId from `request.peer_certs()` |
| `runtime.rs` | Wire interceptor: `LogServiceServer::with_interceptor(svc, auth_interceptor)` |

### E.2d: NFS TLS

| File | Change |
|------|--------|
| `nfs_server.rs` | Accept optional `TlsAcceptor`, wrap TCP streams |
| `runtime.rs` | Build TLS config, pass to NFS server |

**Exit**: S3 + NFS require mTLS when TLS files configured. Auth interceptor
validates tenant identity. E2e tests pass with test certs. D-ADV-1/2/7 resolved.

---

## E.3: ViewStore in Read Path (test-driven)

**Existing BDD**: 7 scenarios in view-materialization.feature (staleness,
read-your-writes, SP crash recovery).

### E.3a: BDD step upgrade (red first)

Upgrade `steps/view.rs` `then_view_state` to actually verify staleness:
```rust
// After poll_views(), check staleness bound:
let view = w.view_store.get_view(view_id).unwrap();
view.check_staleness(now_ms).expect("staleness within bound");
```

This fails because views are disconnected from the gateway read path.

### E.3b: Share ViewStore via Arc

| File | Change |
|------|--------|
| `runtime.rs` | Wrap `view_store` in `Arc<Mutex<ViewStore>>` |
| `runtime.rs` | Pass clone to `InMemoryGateway` (add view_store param) |
| `runtime.rs` | Pass clone to stream processor task |
| `mem_gateway.rs` | Accept optional `Arc<Mutex<ViewStore>>`, check staleness before read |

### E.3c: Staleness enforcement in gateway read

| File | Change |
|------|--------|
| `mem_gateway.rs` | Before `read()`, look up composition's shard → view → check watermark age |
| `error.rs` | Add `GatewayError::StaleView(ViewId, lag_ms)` |

### E.3d: Pipeline integration test

`crates/kiseki-composition/tests/pipeline_integration.rs`:
- Write via composition → poll stream proc → verify view watermark
- Attempt read with expired staleness → get StaleView error

**Exit**: Gateway reads enforce staleness bounds. Views shared between
stream processor and gateway. BDD staleness scenarios pass. D-ADV-3 resolved.

---

## E.4: Persistence Layer (test-driven)

**Existing BDD**: "SP crashes — recovery from last watermark",
"discard and rebuild a view". Raft MemLogStore tests exist.

### E.4a: TDD — disk-backed log store

Create `crates/kiseki-raft/src/disk_log_store.rs`:
- Wraps a directory path
- `append()` writes to WAL file (append-only)
- `read()` reads from WAL
- `truncate()` removes prefix
- `snapshot()` / `restore()` for Raft snapshots

Unit tests first:
- Write + read roundtrip
- Restart (drop + reopen) preserves data
- Truncate removes old entries
- Snapshot + restore equals original state

### E.4b: Wire into openraft

| File | Change |
|------|--------|
| `crates/kiseki-raft/src/lib.rs` | Export `DiskLogStore` |
| `crates/kiseki-log/src/raft/openraft_store.rs` | Option to use DiskLogStore instead of MemLogStore |
| `crates/kiseki-server/src/runtime.rs` | When `KISEKI_DATA_DIR` set, use DiskLogStore |

### E.4c: Raft snapshot implementation

| File | Change |
|------|--------|
| `crates/kiseki-log/src/raft/state_machine.rs` | `build_snapshot()` serializes delta state |
| `crates/kiseki-log/src/raft/state_machine.rs` | `install_snapshot()` restores from serialized state |

### E.4d: Recovery e2e test

`tests/e2e/test_persistence.py`:
- Write delta via gRPC
- Docker restart: `docker compose restart kiseki-server`
- Read delta back — verify it survived
- This is the milestone test for durability (I-L2)

**Exit**: Server persists Raft log to disk. Restart preserves deltas.
E2e test proves durability. HIGH Raft findings partially resolved.

---

## E.5: Go BDD Assertion Depth (test-driven)

**Analysis**: 53 then_ steps return nil without assertions across
advisory_policy (29), federation (10), maintenance (14).

### E.5a: Advisory policy assertions

| Step | Current | Should verify |
|------|---------|---------------|
| `thenCeilingsEnforced` | check nil | `ValidateBudgetInheritance()` returns error for exceeding |
| `thenEffectiveProfiles` | count | `ValidateProfileInheritance()` rejects missing parent profile |
| `thenWorkloadBudgetUnchanged` | nil | budget equals pre-mutation snapshot |
| `thenDataPathCorrect` | nil | verify no error propagation from advisory to data path |
| `thenPolicyProspective` | nil | new workflow gets new policy, existing keeps old |

### E.5b: Federation assertions

| Step | Current | Should verify |
|------|---------|---------------|
| `thenConfigReplicatesAsync` | flag check | actual config diff between peers |
| `thenDataCiphertextOnly` | flag check | verify data payload is encrypted bytes |
| `thenSameKMS` | nil | verify KMS endpoint matches across sites |

### E.5c: Maintenance assertions

| Step | Current | Should verify |
|------|---------|---------------|
| `thenShardsReadOnly` | set flag | attempt write → verify rejection error |
| `thenWritesRejected` | nil | actual write attempt returns error |
| `thenDataPathContinues` | nil | read succeeds during outage |

### E.5d: Run godog strict mode

Change `acceptance_test.go` from `Strict: false` to `Strict: true`.
Any step returning `godog.ErrPending` now fails the test. This forces
all steps to either pass or be explicitly skipped.

**Exit**: 32/32 Go BDD still passing with `Strict: true`. Every then_
step invokes real domain logic. D-ADV-8/9 resolved.

---

## Execution Order

```
E.1 NFS dir index ──→ E.2 mTLS ──→ E.3 ViewStore ──→ E.4 Persistence ──→ E.5 Go BDD
```

Each gets adversarial review before proceeding. E.1-E.3 are one session
each. E.4 is the biggest (may span sessions). E.5 is a quality pass.

## Test Projections

| Item | New Tests | Type |
|------|-----------|------|
| E.1 | +4 unit, +1 BDD, +1 e2e | NFS directory ops |
| E.2 | +2 e2e, +1 integration | mTLS handshake |
| E.3 | +2 integration | Staleness enforcement |
| E.4 | +4 unit, +1 e2e | Disk log, recovery |
| E.5 | +0 new, 53 upgraded | Go assertions |
| **Total** | **+15 new** | ~523 total |

## Verification

After all 5 items:
1. `cargo test` — all Rust tests pass
2. `go test ./...` with `Strict: true` — 32/32 pass
3. `make e2e` — Docker e2e pass (including NFS, mTLS, persistence)
4. Adversarial review — CRITICALs from D/C/BA reviews resolved
5. Fidelity index updated — no LOW confidence crates remaining

---

## Addendum: Design Decisions (2026-04-20)

### Storage backend: redb (pure Rust)

**Decision**: Use `redb` v2 for all persistent storage:
- Raft WAL + log entries
- State machine snapshots
- Chunk metadata index (chunk_id → placement, refcount)
- View watermark checkpoints

**Rationale**: Pure Rust, zero build deps, ACID with copy-on-write B-tree,
crash-safe via atomic commit. ~50KB binary overhead. No compaction needed.
Matches our needs (Raft log append, snapshot read/write, metadata lookup).

**Not used for**: Chunk ciphertext data — stored as files in pool directories
(one file per chunk, 4KB-aligned). redb handles the index, not the blobs.

### Protocol compliance: RFC-driven BDD

**Decision**: Create dedicated feature files mapping to RFC sections:
- `specs/features/nfs3-rfc1813.feature` — 7 procedures × 2 scenarios = 14
- `specs/features/nfs4-rfc7862.feature` — 10 operations × 2 scenarios = 20
- `specs/features/s3-api.feature` — 5 operations × 2 scenarios = 10

Each scenario tests wire format compliance, not just domain semantics.
Python e2e tests validate actual wire encoding via raw TCP (NFS) and
HTTP (S3).

### Updated E.4 scope

E.4 now uses redb instead of custom files:
- `crates/kiseki-raft/src/redb_log_store.rs` — redb-backed Raft log
- `crates/kiseki-raft/Cargo.toml` — add `redb = "2"`
- State machine snapshots via redb transactions
- Chunk metadata index: `redb::TableDefinition<&[u8; 32], &[u8]>`

### New item: E.0 (inserted before E.3)

**E.0: RFC-driven BDD scenarios** — create the 44 feature file scenarios
before implementing protocol fixes. Red first, then green.
