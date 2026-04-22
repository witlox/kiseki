# Phase 7-9 Assessment

**Date**: 2026-04-22
**Context**: Post-I2 (multi-node Raft wired). All production readiness
phases (Q1-Q2, P1-P4, I1-I2) complete. 530/530 BDD scenarios green.

---

## Phase 7: Composition — 85% done, exit criteria met

| Feature | Status | Location |
|---------|--------|----------|
| Composition CRUD | Done | `kiseki-composition/src/composition.rs` |
| Namespace management | Done | `kiseki-composition/src/namespace.rs` |
| Multipart upload FSM | Done | `kiseki-composition/src/multipart.rs` |
| Object versioning | Done | `composition.rs` version field + increment |
| Cross-shard rename EXDEV (I-L8) | Done | `composition.rs:257-286` |
| Log integration (delta emission) | Done | `kiseki-composition/src/log_bridge.rs` |
| Refcount tracking | Missing | Belongs in `kiseki-chunk`, not blocking |
| Inline data below threshold | Missing | See small-files discussion below |
| Composition persistence | In-memory | Durability via log replay; acceptable |

**Verdict**: Exit criteria met. No blockers for Phase 12 integration.

### Invariants covered

- I-X1 (composition belongs to tenant) — enforced in `create()`
- I-X3 (mutation history reconstructible) — deltas emitted to log
- I-L5 (not visible until finalize) — multipart FSM
- I-L8 (cross-shard rename EXDEV) — enforced with error

### Unit test coverage

7 tests: create_and_get, delete, cross_shard_rename_exdev,
same_shard_rename, read_only_rejects, multipart_lifecycle, versioning.

---

## Phase 8: View / Stream Processor — 70% done

| Feature | Status | Location |
|---------|--------|----------|
| Stream processor (poll + watermark) | Done | `kiseki-view/src/stream_processor.rs` |
| View lifecycle (Building -> Active) | Done | `kiseki-view/src/view.rs` |
| MVCC read pins (acquire/release/TTL) | Done | `kiseki-view/src/pin.rs` |
| Staleness tracking | Done | `view.rs` BoundedStaleness check |
| View descriptor | Done | `kiseki-view/src/descriptor.rs` |
| Wired in server runtime | Done | `runtime.rs:256-275` (100ms poll) |
| **Payload decryption** | **Missing** | No crypto call in stream processor |
| ReadYourWrites enforcement | Missing | Accepted but not validated |
| Pull-based descriptor updates | Missing | Descriptors immutable |
| Multi-shard view ordering | Missing | Single-shard only |

### Blockers

1. **Payload decryption**: Stream processor reads deltas but never
   decrypts. Gateways receive ciphertext. Needs `kiseki-crypto`
   integration in `stream_processor.rs`. Small change, high impact.

2. **ReadYourWrites**: Gateway-boundary concern. Session must track
   last-written sequence and block reads until view catches up.
   Not a stream processor issue — belongs in gateway read path.

3. **Multi-shard ordering**: `source_shards` vec supports multiple
   shards but no cross-shard merge or ordering. Single-shard views
   work correctly today.

### Invariants covered

- I-V1 (view derivable from shards alone) — stream processor re-consumes
- I-V2 (consistent prefix up to watermark) — watermark tracked per view
- I-V3 (consistency model per descriptor) — partial (BoundedStaleness only)
- I-V4 (MVCC read pins with TTL) — fully implemented

### Unit test coverage

6 tests: create_and_get_view, watermark_transitions,
discard_and_rebuild, mvcc_pin_lifecycle, release_pin, staleness.

---

## Phase 9: Protocol Gateways — 40% done

| Feature | Status | Location |
|---------|--------|----------|
| S3 gateway (axum router) | Done | `kiseki-gateway/src/s3/`, 8 e2e tests |
| NFS gateway (NFSv3 + v4.2) | Structure exists | `kiseki-gateway/src/nfs/` |
| Gateway-side encryption | Unclear | `mem_gateway.rs` references I-K1 |
| NFS lock state | **Missing** | No lock tracking visible |
| Protocol error mapping | Exists | Not fully verified |
| View integration | Wired | `runtime.rs:221` view_store shared |

### Blockers

1. **Gateway encryption audit**: `InMemoryGateway` references I-K1
   (encrypt before write) but implementation needs verification.
   Critical for security invariants.

2. **NFS lock state**: NFSv4 requires LOCK/UNLOCK/LOCKU state machine
   with lock holders, byte ranges, and lease callbacks. Not visible
   in current code.

3. **NFS protocol depth**: NFSv3 and NFSv4.2 modules exist but
   operational completeness not verified beyond structure.

### E2E test coverage (from Phase I1)

S3: put_and_get, head, get_not_found, delete (4 tests)
Cross-protocol: NFS + S3 interaction tests (4 tests)
Control plane: org/project CRUD (3 tests)

---

## Recommended execution order

1. Phase 8 payload decryption (small, unblocks gateway read path)
2. Phase 9 gateway encryption audit (verify I-K1/I-K2)
3. Phase 9 NFS lock state (new feature, medium effort)
4. Phase 8 ReadYourWrites (gateway-side, not stream processor)
5. Phase 10+ (native client, control plane, advisory)

---

## Open design question: small files and metadata sizing

At scale (10B+ files, 100PB+), inline data stored in the delta log
causes the metadata tier to scale with data volume, not file count.
This needs a separate design discussion — see ADR backlog.

Key concern: if small files (< 4KB) are inlined into deltas, the
Raft state machine replicates file content 3x and snapshots grow
proportionally. Current inline threshold is not implemented (Phase 7
gap), which means this is a design choice still open.

Options:
- a) Very low inline threshold (256B) — only metadata-like payloads
- b) Small-file chunk optimization — dedicated small-object pool
- c) Packed chunks — batch small files into larger extents
- d) Separate metadata/data Raft groups per shard

See capacity planning discussion for 100PB / 10B file scenario.
