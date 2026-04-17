# ADR-004: Schema Versioning and Rolling Upgrades

**Status**: Accepted
**Date**: 2026-04-17
**Context**: A-ADV-2 (upgrade and schema evolution)

## Decision

All persistent formats carry a version field. Rolling upgrades are supported
with N-1/N version compatibility.

### Delta envelope versioning
- `DeltaHeader.format_version: u16` — first field, fixed offset
- Readers that encounter unknown versions fail open (skip the delta,
  log warning) rather than crash
- Writers always produce the current version
- Compaction preserves original format version (does not upgrade)

### Chunk envelope versioning
- `EnvelopeMeta.format_version: u16`
- Algorithm ID already provides crypto-agility
- New envelope fields are additive (protobuf-style: unknown fields preserved)

### Wire protocol versioning (gRPC)
- Protobuf with reserved fields and additive evolution
- gRPC service versioning: `/kiseki.v1.LogService`, `/kiseki.v2.LogService`
- Native client negotiates version on connect

### View materialization
- Stream processors declare which delta format versions they support
- Upgrade sequence: deploy new stream processors first (can read old+new),
  then upgrade writers (produce new format)

## Rolling upgrade sequence

1. Deploy new `kiseki-server` binaries (can read old + new formats)
2. Rolling restart storage nodes (one at a time, Raft quorum maintained)
3. Deploy new `kiseki-control` (Go, independent restart)
4. Deploy new `kiseki-client-fuse` to compute nodes
5. After all nodes upgraded: optional compaction to upgrade old deltas

## Consequences

- All format changes must be backward-compatible for at least one version
- Breaking changes require a two-phase rollout (add new, migrate, remove old)
- Format version is the first field read on every deserialization path
