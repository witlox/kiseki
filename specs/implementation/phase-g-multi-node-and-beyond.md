# Phase G: Multi-Node Raft, EC, Device Management, Admin API, Protocol Ops

## Context

Phase F complete: demo milestone achieved (write, restart, data persists).
525 tests, 197 Rust + 35 Go + 281 BDD + 12 e2e. Single-node persistence
working. Architecture stable (ADR-022 through 026).

~210 red BDD scenarios remain as implementation backlog. This plan
addresses the 5 highest-priority work streams.

## Overview

| # | Stream | Red scenarios | Sessions | Dependencies |
|---|--------|-------------|----------|-------------|
| G.1 | Multi-node Raft | 18 | 2-3 | None (ADR-026 ready) |
| G.2 | EC erasure coding | 14 | 1-2 | G.1 (for multi-device) |
| G.3 | Device/pool management | 19 | 1-2 | G.2 (for pool health) |
| G.4 | Storage admin API | 46 | 2-3 | G.3 (for device ops) |
| G.5 | Protocol RFC completion | ~60 | 2-3 | Independent |

---

## G.1: Multi-Node Raft (18 red scenarios)

**Goal**: 3-node Raft replication, leader election, failover, snapshot transfer.

### G.1a: Real network transport

Replace `StubNetworkFactory` with TCP transport:

| File | Change |
|------|--------|
| `crates/kiseki-raft/src/tcp_transport.rs` | New: TCP+TLS RaftNetwork implementation |
| `crates/kiseki-raft/src/network.rs` | Keep stub for tests, add TCP variant |

openraft's `RaftNetwork` trait requires:
- `async fn append_entries(target, req) -> Result<resp>`
- `async fn install_snapshot(target, req) -> Result<resp>`
- `async fn vote(target, req) -> Result<resp>`

Implement via gRPC or raw TCP with bincode serialization.

### G.1b: Multi-node bootstrap

| File | Change |
|------|--------|
| `crates/kiseki-server/src/runtime.rs` | Accept node_id + peer list from config |
| `crates/kiseki-server/src/config.rs` | Add `KISEKI_NODE_ID`, `KISEKI_PEERS` env vars |

### G.1c: Leader election + failover test

| File | Test |
|------|------|
| `tests/e2e/test_raft_failover.py` | 3-node Docker compose, write, kill leader, verify writes resume |

### G.1d: Snapshot transfer

| File | Change |
|------|--------|
| `crates/kiseki-log/src/raft/state_machine.rs` | Wire `build_snapshot()` + `install_snapshot()` via redb |
| `crates/kiseki-raft/src/tcp_transport.rs` | Snapshot transfer over TCP |

**Exit**: 3 Docker containers form Raft cluster. Leader failover works.
Data survives node failure. 18 multi-node-raft.feature scenarios green.

---

## G.2: EC Erasure Coding (14 red scenarios)

**Goal**: Chunks split into data+parity fragments across JBOD devices.

### G.2a: EC codec (TDD)

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/ec.rs` | New: encode(data, k, m) → fragments, decode(fragments, k) → data |

Use `reed-solomon-erasure` crate (pure Rust, proven).

TDD tests: encode+decode roundtrip, degraded decode, failure on too many lost.

### G.2b: CRUSH-like placement

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/placement.rs` | New: hash-based fragment→device mapping |

Per ADR-026: `hash(chunk_id, frag_idx) % candidates.len()` with
distinct-device constraint.

### G.2c: Wire into ChunkStore

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/store.rs` | write_chunk() → EC encode → place fragments |
| `crates/kiseki-chunk/src/store.rs` | read_chunk() → read fragments → EC decode |

### G.2d: Repair

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/repair.rs` | New: identify affected chunks, reconstruct, re-place |

**Exit**: Chunks survive device failure via EC. 14 erasure-coding.feature
scenarios green. Repair verified.

---

## G.3: Device/Pool Management (19 red scenarios)

**Goal**: Capacity thresholds, device lifecycle, auto-evacuation.

### G.3a: PoolHealth state machine

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/pool.rs` | Implement per-device-class thresholds (ADR-024) |
| `crates/kiseki-chunk/src/pool.rs` | Health transitions: Healthy→Warning→Critical→Full |

### G.3b: Device state machine

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/device.rs` | New: DeviceInfo, DeviceState, health monitoring |

### G.3c: Auto-evacuation

| File | Change |
|------|--------|
| `crates/kiseki-chunk/src/evacuate.rs` | New: background chunk migration on degraded device |

### G.3d: System RAID monitoring

| File | Change |
|------|--------|
| `crates/kiseki-server/src/health.rs` | New: check /proc/mdstat on Linux, log warnings |

**Exit**: Pools enforce capacity thresholds. Devices auto-evacuate on
degradation. 19 device-management.feature scenarios green.

---

## G.4: Storage Admin API (46 red scenarios)

**Goal**: Full StorageAdminService gRPC per ADR-025.

### G.4a: Proto definition

| File | Change |
|------|--------|
| `specs/architecture/proto/kiseki/v1/admin.proto` | New: StorageAdminService with 20+ RPCs |

### G.4b: Rust gRPC server

| File | Change |
|------|--------|
| `crates/kiseki-server/src/admin_grpc.rs` | New: implement StorageAdminService |
| `crates/kiseki-server/src/runtime.rs` | Register on separate port (management) |

### G.4c: CLI mapping

| File | Change |
|------|--------|
| `control/cmd/kiseki-cli/` | Wire CLI subcommands to admin gRPC |

### G.4d: Observability streams

| File | Change |
|------|--------|
| `crates/kiseki-server/src/admin_grpc.rs` | DeviceHealth, IOStats streaming RPCs |

**Exit**: `kiseki pool list`, `kiseki device health --watch`, `kiseki tune set`
all work. 46 storage-admin.feature scenarios green.

---

## G.5: Protocol RFC Completion (~60 red scenarios)

**Goal**: Turn remaining RFC BDD scenarios green.

### G.5a: Step definitions for new RFC scenarios

| File | Change |
|------|--------|
| `crates/kiseki-acceptance/tests/steps/nfs_rfc.rs` | New: step defs for nfs3-rfc1813.feature |
| `crates/kiseki-acceptance/tests/steps/nfs4_rfc.rs` | New: step defs for nfs4-rfc7862.feature |
| `crates/kiseki-acceptance/tests/steps/s3_rfc.rs` | New: step defs for s3-api.feature |

### G.5b: Wire-format e2e tests

| File | Test |
|------|------|
| `tests/e2e/test_nfs_wire.py` | Raw TCP NFS3 NULL/GETATTR/READ via struct.pack |
| `tests/e2e/test_s3_list.py` | S3 LIST with prefix/pagination |

**Exit**: All implemented RFC operations have passing BDD scenarios.
Wire-format validated via Python e2e.

---

## Execution Order

```
G.1 Multi-node Raft (foundation)
    │
    ├──→ G.2 EC (needs multi-device from G.1)
    │       │
    │       └──→ G.3 Device management (needs EC from G.2)
    │               │
    │               └──→ G.4 Storage admin API (needs devices from G.3)
    │
    └──→ G.5 Protocol RFC (independent, can run in parallel)
```

G.1 is the critical path. G.5 can run alongside any other stream.

## Test Projections

| Stream | Scenarios green | New tests |
|--------|----------------|-----------|
| G.1 | 18 | +5 (TDD + e2e) |
| G.2 | 14 | +6 (TDD) |
| G.3 | 19 | +4 (TDD) |
| G.4 | 46 | +3 (integration) |
| G.5 | ~60 | +2 (e2e wire format) |
| **Total** | **~157** | **+20** |

After Phase G: ~438/459 BDD green (95%), ~545 total tests.
