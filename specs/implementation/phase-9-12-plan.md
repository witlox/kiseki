# Phase 9-12 Execution Plan

**Date**: 2026-04-22
**Context**: Post Phase 7-9 findings resolution. 529/534 BDD pass.
ADR-030 accepted. All production readiness phases complete.

---

## Execution order

```
Phase 9 (gateways ~70% → ~95%)
  ↓
Phase 10 (native client hardening)
  ↓
Phase 11 (control plane hardening)
  ↓
Phase 12 (final integration + ADR-030 implementation)
```

---

## Phase 9: Protocol Gateways → 95%

**Goal**: Wire NFS lock manager, verify NFS protocol dispatch depth,
verify S3 multipart coverage.

### 9a: Wire LockManager into nfs4_server

- Instantiate `LockManager` in `NfsContext`
- Wire LOCK/LOCKU/LOCKT operations in `nfs4_server.rs` to call
  `LockManager::lock()`, `unlock()`, `test_lock()`
- Add lease expiry background task (periodic `expire_all()`)

### 9b: NFS protocol dispatch audit

- Read `nfs3_server.rs` — verify all NFSv3 procedures are dispatched
  (GETATTR, SETATTR, LOOKUP, READ, WRITE, CREATE, REMOVE, RENAME,
  READDIR, MKDIR, RMDIR, SYMLINK, READLINK, FSSTAT, FSINFO, PATHCONF)
- Read `nfs4_server.rs` — verify NFSv4 compound operations dispatched
- Flag any that return NFS3ERR_NOTSUPP or are stubbed

### 9c: S3 multipart + error mapping verification

- Read S3 gateway router — verify PutObject, GetObject, HeadObject,
  DeleteObject, ListObjectsV2, CreateMultipartUpload,
  UploadPart, CompleteMultipartUpload, AbortMultipartUpload
- Verify HTTP status codes match S3 spec
- Flag missing endpoints

### 9d: Gateway encryption e2e confidence

- Verify the write→read round-trip in data_path tests encrypts
  and decrypts correctly (already passing, but confirm test
  assertions check actual ciphertext vs plaintext)

**Exit**: NFS lock state wired, protocol dispatch coverage documented,
S3 endpoints verified, no stubbed critical operations.

---

## Phase 10: Native Client Hardening

**Goal**: FUSE mount working end-to-end, transport selection logic.

### 10a: FUSE filesystem audit

- Read `kiseki-client/src/fuse_fs.rs` — verify all FUSE operations
  (getattr, read, write, readdir, create, unlink, rename, mkdir,
  rmdir, open, release, statfs)
- Fix any stubs or panics
- Ensure it creates `InMemoryGateway` correctly (uses `Box<dyn ChunkOps>`)

### 10b: Transport selection

- Read `kiseki-transport/` — verify TCP transport works
- Verify transport abstraction trait allows future fabric backends
- Ensure client can discover server via seed address

### 10c: Client-side encryption

- Verify native client encrypts before sending (I-K1)
- Or confirm native client goes through gateway (which encrypts)

**Exit**: FUSE operations functional, transport abstraction clean,
client builds and connects to server.

---

## Phase 11: Control Plane Hardening

**Goal**: Full tenant CRUD, IAM policy enforcement, placement wiring.

### 11a: Tenant management depth

- Read `kiseki-control/` — verify org/project/workload CRUD
- Verify quota enforcement (I-T2)
- Wire placement constraints into shard creation

### 11b: IAM and policy

- Verify access request validation
- Zero-trust boundary enforcement
- Policy-driven placement (ADR-024 device classes)

### 11c: Flavor management

- Best-fit matching algorithm
- Flavor → pool mapping

**Exit**: Tenant lifecycle complete, IAM enforced, placement
constraints respected.

---

## Phase 12: Final Integration + ADR-030

**Goal**: Compose all crates into production-ready server binary.
Implement ADR-030 small-file placement.

### 12a: ADR-030 implementation

- System disk auto-detection at boot (sysfs media type, capacity)
- `NodeMetadataCapacity` reporting via gRPC health
- `small/objects.redb` store in KISEKI_DATA_DIR
- Inline threshold routing in chunk write path
- State machine offload (payload → redb on apply)
- Snapshot includes inline content from redb
- GC covers `small/objects.redb`
- Raft throughput guard (I-SF7)

### 12b: Persistent Raft log

- Replace `MemLogStore` with redb-backed Raft log store
- Raft state (vote, term, membership) survives restart
- Node restart catches up from local log, not just snapshot

### 12c: Full e2e validation

- Single-node Docker: all existing tests pass
- 3-node Docker: all multi-node tests pass including failover
- Cross-protocol tests (NFS + S3)
- Persistence across restart with Raft

### 12d: Server binary completeness

- All Rust crates wired into `kiseki-server`
- Advisory runtime on isolated tokio runtime
- Process management for per-tenant stream processors
- Node health reporting
- Maintenance mode

**Exit**: Production-ready server binary. All e2e tests pass.
All BDD scenarios have step definitions. ADR-030 operational.

---

## Estimated effort

| Phase | Sessions | Priority |
|-------|----------|----------|
| 9 (gateways) | 1-2 | Now |
| 10 (native client) | 1-2 | Next |
| 11 (control plane) | 2-3 | Then |
| 12 (integration + ADR-030) | 3-5 | Final |
| **Total** | **~7-12** | |
