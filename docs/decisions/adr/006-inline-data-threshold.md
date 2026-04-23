# ADR-006: Inline Data Threshold

**Status**: Accepted
**Date**: 2026-04-17
**Context**: Escalation point 6, analyst session

## Decision

Delta payloads may carry inline data up to **4096 bytes** (4KB).

Data below this threshold is encrypted and stored directly in the delta
payload. No separate chunk write occurs.

## Rationale

- Small files (symlinks, xattrs, tiny configs): avoid chunk overhead
- DeltaFS validated this pattern at scale (inode metadata with inline data)
- 4KB aligns with filesystem block size and NVMe sector size
- Raft replication cost per delta increases slightly but acceptably
  (4KB payload vs ~200 byte metadata-only delta)
- Standard practice: ext4, Btrfs, XFS all support inline data

## Threshold selection

| Threshold | Raft cost | Use cases captured | Chunk overhead saved |
|---|---|---|---|
| 1KB | Minimal | Symlinks, xattrs | Low |
| **4KB** | Acceptable | Small files, metadata, configs | Moderate |
| 8KB | Noticeable | More files inline | Higher but Raft fan-out increases |
| 64KB | Significant | Too much data in the log | Raft becomes bottleneck |

4KB is the sweet spot: captures the majority of metadata-only operations
without overloading Raft replication.

## Consequences

- Configurable per cluster (system-level setting, not per-tenant)
- Compaction must handle deltas with inline data (encrypted payload may
  be larger than metadata-only deltas)
