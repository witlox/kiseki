# MVP → Production Plan

**Date**: 2026-04-22 (updated: integrated all remaining gaps)
**Baseline**: MVP feature complete. 554/563 BDD, 368 tests, 30 ADRs,
63 invariants, both sweeps passed, all adversary findings fixed.

This plan covers every gap from "architecture proven" to "production
HPC storage system ready for deployment." Organized into 9 workstreams
that can be parallelized across teams.

Sources integrated:
- Original 6-workstream plan (crypto, storage, raft, transport, gateway, ops)
- `specs/failure-modes.md` (20 failure modes, P0-P3)
- `specs/assumptions.md` (50+ assumptions needing validation)
- `specs/features/*.feature` (unimplemented BDD scenarios)
- Hardware-specific items (CXL, GPU-direct, NUMA)
- Operational tooling (CLI, docs, upgrade, backup)

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
- Cache TTL enforcement (I-K9) — invalidate cached DEKs
- **Failure mode F-S1**: graceful drain of active reads during shred
- E2e test: write → shred → read fails
- **Depends on**: 1.1
- **Effort**: 1-2 sessions

### 1.4 External KMS providers (ADR-028)
- AWS KMS, Azure Key Vault, HashiCorp Vault adapters
- Circuit breaker for KMS latency (already typed)
- Tenant-brings-own-KMS flow: tenant registers provider,
  kiseki wraps/unwraps via external API
- Wire the 41 `external-kms.feature` scenarios
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
- **Assumption validation**: verify O_DIRECT alignment on target hardware
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
- **Failure mode F-D2**: rebalance during device evacuation
- **Depends on**: 2.1
- **Effort**: 2 sessions

### 2.5 Shard split execution
- Auto-split monitor detects I-L6 ceiling breach
- Compute midpoint of key range
- Create new shard, redistribute deltas
- Atomic cutover: old shard narrows range, new shard takes upper half
- Raft membership for new shard (new Raft group)
- **Failure mode F-L2**: write buffering during split must be durable
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

### 2.7 CXL / Persistent memory tier
- `NvmePersistentMemory` device class (ADR-024 defined, not implemented)
- DAX (Direct Access) mmap for ultra-low-latency metadata
- Use as write-ahead buffer before NVMe commit
- **Assumption**: CXL.mem Type 3 devices available on target platform
- **Depends on**: 2.2
- **Effort**: 3-5 sessions

### 2.8 GPU-direct storage
- Bypass CPU for AI training data loading (GPUDirect Storage / cuFile)
- Pre-registered GPU memory regions for DMA from NVMe
- Integration with chunk read path: decrypt → DMA to GPU buffer
- **Assumption**: NVIDIA GPUDirect Storage driver available
- **Depends on**: 2.2, 4.2 (for fabric-direct GPU path)
- **Effort**: 5-8 sessions

---

## Workstream 3: Raft & Consensus Hardening

### 3.1 Raft mTLS activation
- TLS infrastructure already wired (ADV-S2)
- Generate cluster CA + per-node certs in Docker compose
- `TcpNetworkFactory::with_tls(config)` activated in runtime
- Cert OU validation: only cluster members accepted
- **Depends on**: nothing (infrastructure done)
- **Effort**: 1-2 sessions

### 3.2 Dynamic membership changes
- `raft.add_learner()` → `raft.change_membership()`
- Control plane API: `AddShardMember`, `RemoveShardMember`
- Graceful decommission: add new → catch up → promote → demote old
- Needed for ADR-030 shard placement migration
- **Failure mode F-C1**: leader loss during membership change
- **Depends on**: 3.1
- **Effort**: 2-3 sessions

### 3.3 Persistent Raft log hardening
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

### 3.5 Clock skew and split-brain detection
- **Failure mode F-C3**: HLC clock skew exceeds tolerance
- Detect: compare HLC physical_ms drift across Raft peers
- Alert if skew > configurable threshold (default 500ms)
- Refuse writes if skew > hard limit (prevent causal violations)
- NTP/PTP quality monitoring per node (ClockQuality already modeled)
- **Depends on**: nothing
- **Effort**: 1-2 sessions

---

## Workstream 4: Network Transports

### 4.1 Transport abstraction refinement
- Current `Transport` trait: `connect() → Connection`
- Add: connection pooling, health tracking, reconnect with backoff
- Add: per-connection timeout, circuit breaker
- Add: transport metrics (latency histogram, error rate)
- NUMA-aware: pin transport threads to NUMA node of the NIC
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
- Slingshot adaptive routing configuration hints
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
- Shares implementation with 4.3 — difference is at NIC/switch config
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

### 4.6 NUMA-aware thread pinning
- Detect NUMA topology at boot (`/sys/devices/system/node/`)
- Pin I/O threads to NUMA node of associated NIC
- Pin Raft threads to same NUMA node as Raft NIC
- Pin chunk I/O threads to NUMA node of associated NVMe controller
- Avoid cross-NUMA memory access for hot data paths
- **Depends on**: 4.1
- **Effort**: 1-2 sessions

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
- Reconnect on gateway failure with exponential backoff
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
- Emergency: hard-limit breach → threshold floor via gRPC
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
- **Failure mode F-N1**: federation link failure → queue-and-retry
- **Depends on**: 3.2
- **Effort**: 5-8 sessions

### 6.5 Chaos testing framework
- Fault injection: network partition, slow disk, clock skew
- Jepsen-style linearizability verification
- Shard split under load
- Node failure during rebalance
- Multi-tenant contention
- Clock skew beyond HLC tolerance (F-C3)
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

## Workstream 7: Failure Mode Handling

Covers all 20 failure modes from `specs/failure-modes.md` not already
addressed in other workstreams.

### 7.1 Consensus failures (P0)
- **F-C1**: Leader loss → election timeout → new leader (already works)
- **F-C2**: Quorum loss → shard unavailable → writes rejected (already works)
- **F-C3**: Clock skew → handled in 3.5
- **Validation**: automated test for each, verify recovery time < SLO
- **Effort**: 1-2 sessions

### 7.2 Storage failures (P0-P1)
- **F-D1**: Device failure → EC repair from parity (needs 2.1)
- **F-D2**: Multiple device failure → partial data loss if > EC parity
- **F-D3**: Corrupted extent → CRC32 detection → read from replica
- **F-D4**: Full device → pool capacity threshold → redirect (needs 2.4)
- **Validation**: inject device failures, verify repair completes
- **Effort**: 2-3 sessions

### 7.3 Network failures (P1)
- **F-N1**: Federation link failure → queued replication (needs 6.4)
- **F-N2**: Client disconnect during write → partial write cleanup
- **F-N3**: Fabric transport failure → fallback to TCP (needs 4.5)
- **Validation**: inject network partitions, verify graceful degradation
- **Effort**: 1-2 sessions

### 7.4 Security failures (P1)
- **F-S1**: Crypto-shred during active reads → drain then shred (needs 1.3)
- **F-S2**: Certificate expiry → alert + auto-renewal integration
- **F-S3**: KMS unavailable → circuit breaker + cached keys (needs 1.4)
- **Validation**: inject cert expiry, KMS timeout, verify behavior
- **Effort**: 1-2 sessions

### 7.5 Operational failures (P2-P3)
- **F-O1**: Metadata disk full → emergency threshold floor (ADR-030, done)
- **F-O2**: Audit log backpressure → I-A5 safety valve (spec exists)
- **F-O3**: Stalled consumer blocks GC → alert + admin intervention
- **F-O4**: Schema version mismatch on upgrade → reject incompatible
- **Validation**: inject resource exhaustion, verify alerts fire
- **Effort**: 1-2 sessions

---

## Workstream 8: Tooling & Documentation

### 8.1 Admin CLI
- `kiseki-admin` binary for cluster management
- Subcommands: `status`, `pool`, `device`, `shard`, `tenant`, `maintenance`
- Connects via gRPC to ControlService
- Tabular output (human) + JSON output (scripting)
- **Depends on**: nothing
- **Effort**: 3-5 sessions

### 8.2 Upgrade / schema migration
- redb schema versioning (version table in each database)
- Proto backward compatibility (new fields are always optional)
- Rolling upgrade: mixed-version clusters during transition
- Rollback: downgrade path for one version back
- **Assumption validation**: verify redb handles schema evolution
- **Depends on**: nothing
- **Effort**: 2-3 sessions

### 8.3 Backup & disaster recovery
- Full cluster snapshot: pause writes → snapshot all shards → resume
- Point-in-time restore: replay from snapshot + Raft log
- Cross-site backup: async replication to cold-standby site
- Recovery time objective (RTO) / recovery point objective (RPO) SLOs
- **Depends on**: 6.4 (federation for cross-site)
- **Effort**: 5-8 sessions

### 8.4 Capacity planning tooling
- Predict when to add nodes based on growth rate
- Model: (current_usage, growth_rate, threshold) → days_until_full
- Alert: "pool X will reach Warning in N days at current rate"
- Recommendation engine: suggest node count / device class for workload
- **Depends on**: 6.2 (metrics)
- **Effort**: 2-3 sessions

### 8.5 Documentation
- Operator guide: deployment, configuration, monitoring, troubleshooting
- API reference: gRPC service definitions with examples
- Architecture overview: for new engineers joining the project
- Deployment runbook: Docker compose → Kubernetes → bare metal
- Performance tuning guide: transport selection, EC params, pool sizing
- **Depends on**: all workstreams (documents what's built)
- **Effort**: 5-8 sessions

---

## Workstream 9: Assumption Validation

Items from `specs/assumptions.md` that need hardware validation.

### 9.1 Hardware performance assumptions
- CXI fabric latency: assumed < 2µs for small messages
- NVMe write latency: assumed < 20µs for 4KB aligned writes
- HDD sequential throughput: assumed > 200 MB/s per drive
- EC encode overhead: assumed < 5% CPU for 4+2 RS coding
- **Method**: benchmark on target hardware, update SLOs
- **Effort**: 2-3 sessions

### 9.2 Scale assumptions
- 10B files / 100PB: metadata tier sizing validated (ADR-030 math done)
- Raft group count: assumed < 10K shards per cluster
- Concurrent clients: assumed < 100K FUSE mounts cluster-wide
- **Method**: load test with synthetic workload generators
- **Effort**: 2-3 sessions

### 9.3 Operational assumptions
- NTP/PTP clock quality: assumed < 1ms skew in HPC environments
- Network partition duration: assumed < 30s for fabric recovery
- Device replacement time: assumed < 1 hour for hot-swap
- **Method**: validate with site operations team
- **Effort**: 1-2 sessions

---

## Dependency Graph

```
WS1 (Crypto)     ─── independent ────────────────────────────────┐
WS2 (Storage)    ─── independent (2.7→2.2, 2.8→2.2+4.2) ────────┤
WS3 (Raft)       ─── 3.1 → 3.2 ─────────────────────────────────┤
WS4 (Transport)  ─── 4.1 → 4.2/4.3 → 4.4 → 4.5 ────────────────┤
WS5 (Gateway)    ─── 5.1 → 5.2/5.4; 5.3 → 5.4 ──────────────────┤
WS6 (Operations) ─── 6.1 → 6.2/6.3; 3.2 → 6.4; all → 6.5/6.6 ──┤
WS7 (Failures)   ─── depends on respective WS items ──────────────┤
WS8 (Tooling)    ─── 8.3 → 6.4; 8.4 → 6.2; 8.5 → all ───────────┤
WS9 (Validation) ─── requires hardware access ─────────────────────┘
```

WS1-5 parallelizable across teams. WS6-9 cross-cutting.

## Estimated Total Effort

| Workstream | Sessions | Critical Path |
|------------|----------|---------------|
| 1. Crypto / KMS | 8-12 | No |
| 2. Storage engine | 17-26 | Shard split (2.5), GPU-direct (2.8) |
| 3. Raft consensus | 7-11 | mTLS (3.1) → membership (3.2) |
| 4. Network transports | 16-25 | CXI (4.2) + IB (4.3) are big |
| 5. Gateway / client | 12-19 | SigV4 (5.1) + Kerberos (5.3) |
| 6. Operations | 20-30 | Federation (6.4) + chaos (6.5) |
| 7. Failure modes | 6-11 | Depends on WS1-6 items |
| 8. Tooling / docs | 17-27 | Backup (8.3) + docs (8.5) |
| 9. Assumption validation | 5-8 | Requires hardware |
| **Total** | **108-169** | |

Critical path to HPC production: **3.1 → 4.1 → 4.2 → 4.5 → 6.6 → 9.1**
(Raft mTLS → transport refine → Slingshot → selection → benchmark → validate).
