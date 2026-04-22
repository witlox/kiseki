# MVP → Production Plan

**Date**: 2026-04-22
**Baseline**: MVP feature complete. 554/563 BDD, 368 tests, 30 ADRs,
63 invariants, both sweeps passed, all adversary findings fixed.

This plan covers the gap from "architecture proven" to "production HPC
storage system." Organized into 6 workstreams that can be parallelized
across teams. Each workstream has phases ordered by dependency.

---

## Workstream 1: Cryptography & Key Management

### 1.1 Key rotation worker
- Background task monitors epoch TTL
- Triggers `RotateKey` via Raft consensus
- New writes use new epoch; old epochs remain for reads
- **Depends on**: nothing (self-contained)
- **Effort**: 1-2 sessions

### 1.2 Re-encryption on rotation
- Admin-triggered `ReencodePool` scans all chunks
- Reads with old DEK, re-encrypts with new DEK
- Batched, rate-limited, resumable after crash
- Updates chunk_meta epoch reference
- **Depends on**: 1.1
- **Effort**: 2-3 sessions

### 1.3 Crypto-shred propagation
- Delete tenant KEK → all tenant data unreadable
- Propagate to all nodes holding tenant chunks
- Verify: read after shred returns crypto error, not stale data
- Cache TTL enforcement (I-K9) — invalidate cached DEKs
- E2e test: write → shred → read fails
- **Depends on**: 1.1
- **Effort**: 1-2 sessions

### 1.4 External KMS providers (ADR-028)
- AWS KMS, Azure Key Vault, HashiCorp Vault adapters
- Circuit breaker for KMS latency (already typed)
- Tenant-brings-own-KMS flow: tenant registers provider,
  kiseki wraps/unwraps via external API
- **Depends on**: 1.1
- **Effort**: 3-5 sessions (per provider)

---

## Workstream 2: Storage Engine Hardening

### 2.1 Real multi-device EC striping
- Currently: EC encode produces fragments, stored in HashMap
- Production: fragments distributed across distinct physical devices
- Placement engine respects device failure domains (rack, chassis)
- Write fails if insufficient devices for EC params
- **Depends on**: nothing
- **Effort**: 2-3 sessions

### 2.2 Raw block device O_DIRECT implementation
- `RawBlockDevice` backend: open with O_DIRECT, aligned I/O
- Bypass OS page cache (kiseki manages its own cache)
- Superblock detection on real /dev/sdX, /dev/nvmeXn1
- Safety checks: refuse init if filesystem signatures detected
- **Depends on**: nothing
- **Effort**: 2-3 sessions

### 2.3 TRIM batching
- Batch freed extents into TRIM/DISCARD commands
- Periodic flush (not per-free) to avoid TRIM storms
- SSD wear optimization: coalesce adjacent TRIM ranges
- HDD: no-op (no TRIM support)
- **Depends on**: 2.2
- **Effort**: 1 session

### 2.4 Pool rebalancing
- When pool crosses Warning threshold, start migration
- Move chunks from hot pool to sibling pool (same device class)
- Respect I-C5 thresholds during rebalance
- Rate-limited to avoid saturating I/O
- **Depends on**: 2.1
- **Effort**: 2 sessions

### 2.5 Shard split execution
- Auto-split monitor detects I-L6 ceiling breach
- Compute midpoint of key range
- Create new shard, redistribute deltas
- Atomic cutover: old shard narrows range, new shard takes upper half
- Raft membership for new shard (new Raft group)
- **Depends on**: nothing (auto_split.rs has detection, needs execution)
- **Effort**: 3-4 sessions

### 2.6 Device scrub implementation
- `DeviceBackend::scrub()` verifies:
  - Bitmap primary/mirror consistency
  - CRC32 on sampled extents (configurable sample rate)
  - Orphan extent detection (bitmap allocated but no chunk_meta)
  - redb journal replay completeness
- Report emitted to audit log
- **Depends on**: 2.2
- **Effort**: 1-2 sessions

---

## Workstream 3: Raft & Consensus Hardening

### 3.1 Raft mTLS activation
- TLS infrastructure already wired (ADV-S2)
- Generate cluster CA + per-node certs in Docker compose
- `TcpNetworkFactory::with_tls(config)` activated in runtime
- `run_raft_rpc_server(addr, raft, Some(server_config))`
- Cert OU validation: only cluster members accepted
- **Depends on**: nothing (infrastructure done)
- **Effort**: 1-2 sessions

### 3.2 Dynamic membership changes
- `raft.add_learner()` → `raft.change_membership()`
- Control plane API: `AddShardMember`, `RemoveShardMember`
- Graceful decommission: add new member → catch up → promote → demote old
- Needed for ADR-030 shard placement migration
- **Depends on**: 3.1
- **Effort**: 2-3 sessions

### 3.3 Persistent Raft log for production
- Current: RedbRaftLogStore works but not battle-tested
- Production: test under concurrent load, verify crash recovery
- Benchmark: Raft commit latency with redb fsync
- Compare: redb vs sled vs rocksdb for Raft log
- **Depends on**: nothing
- **Effort**: 1-2 sessions

### 3.4 Snapshot transfer under load
- Current: snapshot works for small state machines
- Production: test with GB-scale state machines
- Chunked transfer if snapshot > MAX_RAFT_RPC_SIZE
- Progress reporting during long transfers
- **Depends on**: nothing
- **Effort**: 1-2 sessions

---

## Workstream 4: Network Transports

### 4.1 Transport abstraction refinement
- Current `Transport` trait: `connect() → Connection`
- Add: connection pooling, health tracking, reconnect
- Add: per-connection timeout, backoff on failure
- Add: transport metrics (latency histogram, error rate)
- **Depends on**: nothing
- **Effort**: 2 sessions

### 4.2 Slingshot / CXI transport (HPE Cray)
- libfabric backend via `libfabric-sys` FFI crate
- CXI provider: `fi_getinfo(FI_EP_RDM, "cxi")` for discovery
- Endpoint management: `fi_endpoint()`, `fi_listen()`, `fi_connect()`
- RDMA semantics: zero-copy send/recv for chunk data
- Pre-registered memory regions for bulk transfer
- Message framing: same length-prefixed protocol over fabric
- VNI (Virtual Network Interface) for tenant isolation
- Service ID based addressing (no IP, no TCP)
- Build: feature-gated `transport-cxi`, requires libfabric-dev + CXI headers
- Testing: requires Slingshot hardware or CXI simulator
- **Depends on**: 4.1
- **Effort**: 5-8 sessions

### 4.3 InfiniBand / RDMA verbs transport
- `rdma-core` / `ibverbs` FFI via `rdma-sys` crate
- Connection management: RC (Reliable Connected) QPs
- `ibv_post_send()` / `ibv_post_recv()` for RPC messages
- RDMA Read for chunk data (one-sided, no target CPU)
- Memory registration: `ibv_reg_mr()` for chunk buffers
- GID-based addressing (not IP): `ibv_query_gid()`
- Protection domain per tenant (QP isolation)
- Build: feature-gated `transport-ib`, requires rdma-core headers
- Testing: requires IB hardware or SoftROCE
- **Depends on**: 4.1
- **Effort**: 5-8 sessions

### 4.4 RoCEv2 transport (RDMA over Converged Ethernet)
- Same verbs API as InfiniBand (4.3) but over Ethernet
- GRH (Global Routing Header) required for RoCEv2
- ECN/PFC congestion control configuration
- DSCP marking for QoS priority
- MTU negotiation (usually 4096 for RoCE)
- Can share implementation with 4.3 — difference is at NIC/switch config level
- Build: same `transport-ib` feature as 4.3
- Testing: requires RoCEv2 NIC (Mellanox/NVIDIA ConnectX) or SoftROCE
- **Depends on**: 4.3 (shared verbs layer)
- **Effort**: 1-2 sessions (delta from 4.3)

### 4.5 Transport selection and failover
- Current: `TransportSelector` with static priority
- Production: auto-detect available transports at boot
  - Probe for CXI devices (`/sys/class/cxi/`)
  - Probe for IB devices (`/sys/class/infiniband/`)
  - Probe for RoCE (`/sys/class/infiniband/` + link_layer=Ethernet)
  - Fallback: TCP+TLS (always available)
- Runtime failover: if preferred transport fails, degrade gracefully
- Raft transport: use same selection (prefer fabric for low-latency consensus)
- **Depends on**: 4.2, 4.3, 4.4
- **Effort**: 2-3 sessions

---

## Workstream 5: Gateway & Client Production

### 5.1 S3 SigV4 authentication
- Parse `Authorization` header (AWS Signature Version 4)
- Validate signature against tenant secret key
- Extract tenant identity from access key → OrgId mapping
- Support presigned URLs (query string auth)
- **Depends on**: nothing
- **Effort**: 2-3 sessions

### 5.2 S3 bucket CRUD
- CreateBucket, DeleteBucket, HeadBucket, ListBuckets
- Bucket → namespace mapping via control plane
- Bucket policies (public/private, cross-tenant sharing)
- **Depends on**: 5.1
- **Effort**: 1-2 sessions

### 5.3 NFS Kerberos authentication (RPCSEC_GSS)
- ONC RPC AUTH_GSS integration
- Kerberos principal → tenant OrgId mapping
- Per-export access control (similar to /etc/exports)
- NFSv4 ACLs (beyond POSIX mode bits)
- **Depends on**: nothing
- **Effort**: 3-5 sessions

### 5.4 Multi-tenant gateway
- Per-request tenant extraction from cert/auth
- S3: from SigV4 access key
- NFS: from Kerberos principal
- gRPC: from mTLS cert OU
- Namespace isolation per tenant in gateway context
- **Depends on**: 5.1, 5.3
- **Effort**: 2-3 sessions

### 5.5 FUSE production hardening
- Nested directory support (subdirectories across shards)
- Write-at-offset (currently only offset=0)
- File locking (POSIX fcntl)
- Connection pooling to gateway
- Reconnect on gateway failure
- **Depends on**: nothing
- **Effort**: 2-3 sessions

### 5.6 Client bindings implementation
- PyO3: `kiseki_read()`, `kiseki_write()`, `kiseki_stat()` → Python
- C FFI: flesh out stubs with real gateway calls
- C++ wrapper: RAII Client class with real connection
- Build: maturin (Python), cbindgen (C header generation)
- **Depends on**: 5.5
- **Effort**: 2-3 sessions

### 5.7 ADR-030 dynamic threshold feedback loop
- Control plane aggregates `NodeMetadataCapacity` from all nodes
- Computes per-shard threshold from min voter budget
- Commits threshold updates via Raft `ShardConfig` change
- Threshold decrease: automatic on soft-limit breach
- Threshold increase: admin approval via maintenance mode
- Emergency: hard-limit breach → threshold floor via gRPC (not Raft)
- **Depends on**: nothing (infrastructure done, needs control plane loop)
- **Effort**: 2-3 sessions

---

## Workstream 6: Operational Readiness

### 6.1 Structured logging
- Replace all `eprintln!` with `tracing` crate
- Structured fields: shard_id, tenant_id, operation, latency_us
- Log levels: ERROR, WARN, INFO, DEBUG, TRACE
- JSON output for log aggregation (ELK, Loki)
- Tenant-scoped: no cross-tenant data in shared logs
- **Depends on**: nothing
- **Effort**: 2-3 sessions

### 6.2 Metrics + Prometheus
- Per-crate metrics: latency histograms, counters, gauges
- Shard-level: commit latency, delta count, byte size
- Chunk-level: write/read IOPS, EC encode time
- Gateway-level: request rate, error rate, p99 latency
- Pool-level: capacity used/total, device health
- Prometheus endpoint on separate port (e.g., 9090)
- Grafana dashboards (template)
- **Depends on**: 6.1
- **Effort**: 3-4 sessions

### 6.3 OpenTelemetry tracing
- Distributed traces across gateway → composition → log → chunk
- Span context propagation via gRPC metadata
- Trace sampling (configurable rate)
- Export to Jaeger/OTLP
- **Depends on**: 6.1
- **Effort**: 2-3 sessions

### 6.4 Federation async replication
- Peer-to-peer config sync (tenant metadata, namespaces)
- Async delta replication for multi-site (no cross-site Raft)
- Conflict resolution: last-writer-wins with HLC timestamps
- Data-cipher-only mode: replicate encrypted chunks without key access
- **Depends on**: 3.2
- **Effort**: 5-8 sessions

### 6.5 Chaos testing framework
- Fault injection: network partition, slow disk, clock skew
- Jepsen-style linearizability verification
- Shard split under load
- Node failure during rebalance
- Multi-tenant contention
- **Depends on**: all workstreams
- **Effort**: 5-8 sessions

### 6.6 Performance benchmarking
- IOR/MDTest for POSIX (NFS + FUSE)
- S3 benchmark (s3bench, warp)
- Raft commit latency profiling
- EC encode/decode throughput
- Fabric transport bandwidth (CXI, IB, RoCE vs TCP)
- Baseline: set SLOs for p50/p99/p999
- **Depends on**: 4.2-4.4
- **Effort**: 3-5 sessions

---

## Dependency Graph

```
Workstream 1 (Crypto)     ─── independent ───────────────────────┐
Workstream 2 (Storage)    ─── independent ───────────────────────┤
Workstream 3 (Raft)       ─── 3.1 before 3.2 ───────────────────┤
Workstream 4 (Transport)  ─── 4.1 → 4.2/4.3 → 4.4 → 4.5 ──────┤
Workstream 5 (Gateway)    ─── 5.1 → 5.2/5.4; 5.3 → 5.4 ────────┤
Workstream 6 (Operations) ─── 6.1 → 6.2/6.3; all → 6.5/6.6 ────┘
```

Workstreams 1-5 are parallelizable across teams. Workstream 6 is
cross-cutting and should start early (6.1 structured logging first).

## Estimated Total Effort

| Workstream | Sessions | Critical Path |
|------------|----------|---------------|
| 1. Crypto / KMS | 8-12 | No |
| 2. Storage engine | 10-15 | Shard split (2.5) |
| 3. Raft consensus | 5-9 | mTLS (3.1) → membership (3.2) |
| 4. Network transports | 15-23 | CXI (4.2) + IB (4.3) are big |
| 5. Gateway / client | 12-19 | SigV4 (5.1) + Kerberos (5.3) |
| 6. Operations | 20-30 | Federation (6.4) + chaos (6.5) |
| **Total** | **70-108** | |

Critical path to HPC production: **3.1 → 4.2 → 4.5 → 6.6** (Raft mTLS
→ Slingshot transport → transport selection → benchmarking). This is the
path that unlocks real hardware deployment.
