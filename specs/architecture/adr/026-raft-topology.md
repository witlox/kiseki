# ADR-026: Raft Topology — Per-Shard on Fabric (Strategy A)

**Status**: Accepted.
**Date**: 2026-04-20.
**Deciders**: Architect + domain expert.

## Context

Kiseki needs multi-node Raft for durability (I-L2) and failover.
The cluster operates on a shared Slingshot fabric (200 Gbps per node)
where control messages (Raft) and data (chunk I/O) share bandwidth.

Three strategies were evaluated:
- **A**: Raft per shard, all traffic on fabric
- **B**: Raft for metadata only, primary-copy for data (Ceph-like)
- **C**: Multi-Raft with batched transport (TiKV-like)

## Decision

**Strategy A: Raft per shard, on the data fabric.**

Start with A, add C's batching optimization when monitoring shows
it's needed (>1000 connections per node).

### Why this works

Raft traffic is negligible compared to data fabric capacity:

| Scale | Shards | Groups/node | Heartbeat/node | Replication/node | % of 200 Gbps |
|-------|--------|-------------|----------------|-----------------|----------------|
| 10 nodes | 100 | 30 | 78 KB/s | 3 MB/s | <0.001% |
| 100 nodes | 1000 | 30 | 78 KB/s | 3 MB/s | <0.001% |
| 1000 nodes | 10,000 | 30 | 78 KB/s | 3 MB/s | <0.001% |

Groups-per-node stays constant at ~30 because shard count scales
with node count (each node hosts ~30 shard replicas regardless of
cluster size).

### Key insight: Raft only for metadata

**Chunk data does NOT go through Raft.** The write path:

```
Large write:
  Client → Gateway → encrypt → chunk to NVMe (EC direct) → delta to Raft (1KB metadata)

Small write (<4KB):
  Client → Gateway → encrypt → inline in delta → Raft only
```

Raft replicates delta metadata (~1KB per operation). Chunk ciphertext
(64KB-64MB) is written directly to NVMe devices via EC. This means:
- Write throughput limited by NVMe/network, NOT by Raft
- Raft consensus adds ~30-60µs (RDMA) or ~75-250µs (TCP) per metadata op
- 50-100k metadata ops/sec per shard, shards in parallel

**Phase 16a refinement.** The cross-node chunk fabric introduced in
Phase 16a adds a second Raft-replicated metadata table —
`cluster_chunk_state`, keyed by `(tenant_id, chunk_id)` and carrying
the per-chunk refcount + placement list (~80 bytes per entry). This
is *still* metadata, not bytes: the chunk ciphertext travels over the
dedicated `ClusterChunkService` gRPC fabric (`PutFragment` / etc.)
and never rides Raft. The "Raft only for metadata" principle holds —
`cluster_chunk_state` is in scope for Raft because it's a few dozen
bytes per chunk, sized to the same order as the existing delta
metadata. See `specs/implementation/phase-16-cross-node-chunks.md`
D-4 for the atomic `ChunkAndDelta` proposal that keeps the
cluster_chunk_state and the composition delta consistent across
replicas.

### Projected performance vs competition

| Metric | Kiseki (projected) | Lustre | Ceph | GPFS |
|--------|-------------------|--------|------|------|
| Write GB/s /node | 25-40 | 5-12 | 1-3 | 5-15 |
| Read GB/s /node | 40-50 | 10-20 | 3-8 | 10-30 |
| Write latency | 30-250µs | 100-500µs | 500-2000µs | 100-300µs |
| Metadata IOPS /node | 1.5-3M | 50-100k | 10-50k | 200k |

### Raft group configuration

| Raft group | Members | Where |
|-----------|---------|-------|
| Key manager | 3-5 | Dedicated keyserver nodes |
| Log shard (per shard) | 3 | Spread across storage nodes |
| Audit shard (per tenant) | 3 | Spread across storage nodes |

Placement rule: no two members of the same group on the same node
(or same rack if rack-aware placement is configured).

### Transport

| Phase | Transport | Optimization |
|-------|-----------|-------------|
| Phase 1 (now) | TCP + TLS | Direct connections, one per Raft peer |
| Phase 2 (10+ nodes) | TCP + TLS + connection pooling | Reuse connections across groups |
| Phase 3 (100+ nodes) | Batched transport (Strategy C) | Coalesce heartbeats per node pair |
| Future | Slingshot CXI / RDMA | Sub-10µs Raft RTT |

### Election storm mitigation

Correlated failure (rack power loss) causes simultaneous elections
for all Raft groups on affected nodes (~30 groups per node × N nodes).

Mitigations:
1. **Randomized election timeouts**: openraft already does this (150-300ms jitter)
2. **Staggered group startup**: on node restart, groups start elections
   over a 5-second window (not all at once)
3. **Leader sticky**: prefer re-electing the same leader if it recovers
   within the election timeout (avoids unnecessary leader changes)

### Network requirements

| Network | Purpose | Kiseki traffic |
|---------|---------|---------------|
| Data fabric (Slingshot/ethernet) | Chunk I/O + Raft | 99.99% data, 0.01% Raft |
| Management network (if available) | ControlService, monitoring | Optional: route Raft here to fully isolate |

**Management network is NOT required.** Raft on the fabric is fine
because the overhead is <0.001% of capacity. If a management network
exists (common in HPC), Raft CAN be routed there for belt-and-suspenders
isolation, but it's not necessary.

## Consequences

- Simplest implementation: use openraft's built-in TCP transport
- No separate management network required (but can use one)
- Scales to ~10k shards / 1000 nodes without transport optimization
- Add batching (Strategy C) as a pure transport optimization later
- Election storms during correlated failure are bounded by randomized timeouts
- Raft adds ~30-250µs to metadata write latency (acceptable for HPC)

## Migration path

If Strategy A proves insufficient at extreme scale:
1. Add batched transport (C) — pure transport change, no protocol change
2. If even C is insufficient, partition shards into metadata-Raft and
   data-EC groups (B) — larger refactor but data model already supports it

## References

- ADR-005: EC and chunk durability
- ADR-022: Storage backend (redb)
- ADR-024: Device management
- TiKV Multi-Raft: https://tikv.org/deep-dive/scalability/multi-raft/
- openraft: https://datafuselabs.github.io/openraft/
- Slingshot fabric: ~5-10µs RTT, 200 Gbps per endpoint
