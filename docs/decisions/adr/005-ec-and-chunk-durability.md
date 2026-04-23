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
