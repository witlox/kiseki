# Phase F: Usable System — Protocol Ops + Persistence

## Context

520+ tests, 281/320 BDD green, 178 red scenarios as backlog.
Architecture is stable (ADR-022 through 025, all adversarially reviewed).
redb log store exists (6 TDD tests) but not wired into Raft.
NFS/S3 have basic ops but missing REMOVE/RENAME/OPEN/LIST.

**Goal**: A client can mount NFS, write a file, list files, delete a file,
restart the server, and the data is still there. That's the demo milestone.

**Discipline**: BDD-first. Each item starts with a red scenario from
the existing feature files, implements until green. Adversarial review
after each sub-phase.

---

## F.1: NFS Protocol Completeness (test-driven from nfs3-rfc1813.feature)

**Red scenarios to turn green**: 5 in nfs3-rfc1813.feature, 9 in nfs4-rfc7862.feature

### F.1a: NFS3 REMOVE + RENAME

| File | Change |
|------|--------|
| `nfs_ops.rs` | Add `remove(ns, name)` → deletes from dir index + composition |
| `nfs3_server.rs` | Add REMOVE (proc 12) + RENAME (proc 14) dispatch + reply |
| `nfs_dir.rs` | Add `rename(ns, old_name, new_name)` |

BDD scenarios: RFC1813 §3.3.12 REMOVE (2), §3.3.14 RENAME (1)

### F.1b: NFS3 FSSTAT + FSINFO

| File | Change |
|------|--------|
| `nfs3_server.rs` | Add FSSTAT (proc 21) + FSINFO (proc 20) with pool capacity |
| `nfs_ops.rs` | Add `fsstat()` → returns pool used/free bytes |

BDD scenarios: RFC1813 §3.3.20 FSINFO (1), §3.3.21 FSSTAT (1)

### F.1c: NFSv4.2 OPEN + CLOSE + LOCK

| File | Change |
|------|--------|
| `nfs4_server.rs` | Add OPEN (op 18), CLOSE (op 2), LOCK (op 12) to COMPOUND |
| `nfs4_server.rs` | Stateid management in SessionManager |

BDD scenarios: RFC7862 §18.16 OPEN (3), §18.2 CLOSE (1), §18.10 LOCK (2)

### F.1d: NFSv4.2 LOOKUP + REMOVE + READDIR

| File | Change |
|------|--------|
| `nfs4_server.rs` | Wire LOOKUP → dir index, REMOVE → composition delete, READDIR → dir listing |

BDD scenarios: RFC7862 §18.15 LOOKUP (1), §18.25 REMOVE (1), §18.26 READDIR (1)

### F.1e: Python e2e — real NFS client test

| File | Test |
|------|------|
| `tests/e2e/test_nfs_gateway.py` | Raw TCP: send NFS3 NULL + GETATTR via struct.pack, verify reply |

**Exit**: 14 RFC NFS scenarios green. NFS mount can create, read, list, delete files.

---

## F.2: S3 Protocol Completeness (test-driven from s3-api.feature)

**Red scenarios to turn green**: 4 in s3-api.feature

### F.2a: S3 ListObjectsV2

| File | Change |
|------|--------|
| `s3_server.rs` | Add `GET /:bucket` route → list compositions in namespace |
| `mem_gateway.rs` | Add `list_compositions(ns)` → returns names + sizes |
| `composition.rs` | Add `list(ns_id) -> Vec<Composition>` to CompositionOps |

BDD scenarios: ListObjectsV2 (3), pagination (1)

### F.2b: S3 DELETE wired

| File | Change |
|------|--------|
| `s3_server.rs` | DELETE handler calls `CompositionOps::delete` instead of no-op |

**Exit**: 4 S3 scenarios green. `aws s3 ls` and `boto3.list_objects_v2` work.

---

## F.3: Persistence Wiring (test-driven from persistence.feature)

**Red scenarios to turn green**: 12 in persistence.feature

### F.3a: Wire redb into server runtime

| File | Change |
|------|--------|
| `runtime.rs` | When `KISEKI_DATA_DIR` set, create `RedbLogStore` at `$DIR/raft/db.redb` |
| `runtime.rs` | Pass redb store to `MemShardStore` (or replace with redb-backed variant) |

The simplest approach: keep `MemShardStore` for in-memory ops but persist
every `append_delta` to redb, and reload on startup.

### F.3b: Persist-and-reload wrapper

| File | Change |
|------|--------|
| `crates/kiseki-log/src/persistent_store.rs` | New: wraps `MemShardStore` + `RedbLogStore` |
| | `append_delta()` → write to redb + in-memory |
| | `new()` → load from redb into in-memory on startup |

TDD tests:
- Write 10 deltas, drop, reopen → all 10 readable
- Append after reload → sequence continues

### F.3c: View watermark persistence

| File | Change |
|------|--------|
| `runtime.rs` | After stream processor poll, checkpoint watermark to redb |
| | On startup, restore watermark from redb |

### F.3d: Key epoch persistence

| File | Change |
|------|--------|
| `crates/kiseki-keymanager/` | Persist epochs to redb; reload on startup |

### F.3e: Docker e2e — restart survival test

| File | Test |
|------|------|
| `tests/e2e/test_persistence.py` | Write delta → docker restart → read delta back |

**Exit**: Server persists data to disk. Restart preserves deltas, watermarks,
key epochs. E2e test proves it across Docker restart. 12 BDD scenarios green.

---

## Execution Order

```
F.1a NFS REMOVE/RENAME ──→ F.1b FSSTAT/FSINFO ──→ F.1c NFSv4 OPEN ──→ F.1d LOOKUP/READDIR
                                                                              │
F.2a S3 ListObjectsV2 ──→ F.2b S3 DELETE                                     │
                                                                              ▼
                                              F.3a redb wiring ──→ F.3b persist wrapper ──→ F.3e e2e
```

F.1 and F.2 are independent (can run in parallel).
F.3 depends on nothing but is the biggest lift.
Adversarial review after F.1+F.2, then after F.3.

## Test Projections

| Sub-phase | Scenarios turned green | New tests |
|-----------|----------------------|-----------|
| F.1 | 14 NFS RFC scenarios | +1 e2e |
| F.2 | 4 S3 scenarios | +0 (existing e2e covers) |
| F.3 | 12 persistence scenarios | +4 TDD, +1 e2e |
| **Total** | **30 red → green** | **+6 new** |

After Phase F: ~311/459 BDD green (68%), ~526 total tests.

## Key Files

| File | Sub-phase |
|------|-----------|
| `crates/kiseki-gateway/src/nfs_ops.rs` | F.1 |
| `crates/kiseki-gateway/src/nfs3_server.rs` | F.1 |
| `crates/kiseki-gateway/src/nfs4_server.rs` | F.1 |
| `crates/kiseki-gateway/src/nfs_dir.rs` | F.1 |
| `crates/kiseki-gateway/src/s3_server.rs` | F.2 |
| `crates/kiseki-gateway/src/mem_gateway.rs` | F.2 |
| `crates/kiseki-composition/src/composition.rs` | F.2 |
| `crates/kiseki-log/src/persistent_store.rs` | F.3 (new) |
| `crates/kiseki-server/src/runtime.rs` | F.3 |
| `crates/kiseki-raft/src/redb_log_store.rs` | F.3 |
| `tests/e2e/test_persistence.py` | F.3 |
| `tests/e2e/test_nfs_gateway.py` | F.1 |

## Verification

After Phase F:
1. `cargo test` — all Rust tests pass (~200)
2. `make e2e` — Docker e2e pass including NFS + persistence restart
3. 30 BDD scenarios turned from red to green
4. Manual demo: `mount -t nfs`, write, ls, rm, restart server, data persists
5. Adversarial review — protocol compliance + persistence correctness
