# ADR-005: Erasure Coding and Chunk Durability

**Status**: Accepted
**Date**: 2026-04-17
**Context**: I-C4, escalation point 10

## Decision

EC parameters are per affinity pool, configured by cluster admin.

### Default profiles

| Pool type | Strategy | Rationale |
|---|---|---|
| fast-nvme (metadata, hot data) | EC 4+2 | Balance of space efficiency and rebuild speed |
| bulk-nvme (cold data, checkpoints) | EC 8+3 | Higher space efficiency for bulk data |
| meta-nvme (log SSTables, key manager) | Replication-3 | Lowest latency for consensus-critical data |

### Phase 16a default — Replication-3 below 6 nodes

EC X+Y requires ≥X+Y distinct failure domains (I-D4). A 3-node
cluster cannot satisfy EC 4+2 (needs 6 distinct nodes) or EC 8+3
(needs 11). Phase 16a ships **Replication-3 as the only durability
strategy** until the per-cluster-size defaults table lands in 16b:

| Cluster size | 16a default | 16b candidate |
|---|---|---|
| 1 node | local-only (no replication) | unchanged |
| 2 nodes | Replication-2 (no quorum — read-only HA only) | unchanged |
| 3-5 nodes | Replication-3 | EC 2+1 (storage-efficient) or stay Rep-3 |
| ≥6 nodes | Replication-3 | EC 4+2 (matches the original default) |

Replication-3 pays a 3× storage tax vs EC 4+2's 1.5×, but for
small/medium clusters that's the price of correctness. See
`specs/implementation/phase-16-cross-node-chunks.md` for the
ClusteredChunkStore + ClusterChunkService design.

### Chunk-RDMA alignment (C-ADV-3)

Content-defined chunking produces variable-size chunks. For RDMA:
- Chunks are stored with 4KB-aligned padding on disk
- RDMA scatter-gather lists map logical chunk boundaries to aligned physical blocks
- One-sided RDMA transfers use pre-registered memory regions at 4KB alignment
- Padding overhead is bounded: max 4KB per chunk, typically <1% for chunks >256KB

## Consequences

- Pool-level EC config means all chunks in a pool share the same protection level
- Changing EC parameters requires re-encoding existing chunks (background process)
- RDMA alignment adds trivial storage overhead but enables zero-copy transfers
