# ADR-022: Storage Backend — redb (Pure Rust)

**Status**: Accepted.
**Date**: 2026-04-20.
**Deciders**: Architect + implementer.

## Context

The system needs persistent storage for:
1. **Raft log entries** — append-heavy, sequential reads for replay
2. **State machine snapshots** — periodic full-state serialization
3. **Chunk metadata index** — key-value mapping (chunk_id → placement, refcount)
4. **View watermark checkpoints** — small, frequently updated

The spec references "RocksDB or equivalent" (build-phases.md Phase 3)
but does not commit to a specific engine. RocksDB is C++ and brings
~200MB build dependency via cmake/clang/librocksdb.

## Decision

Use **redb** v2 for all structured persistent storage.

### What redb handles

| Data | redb Table | Key | Value |
|------|-----------|-----|-------|
| Raft log entries | `raft_log` | `u64` (log index) | bincode-serialized entry |
| Raft vote/term | `raft_meta` | `&str` ("vote", "term") | `u64` |
| State machine snapshot | `sm_snapshot` | `"latest"` | bincode-serialized state |
| Chunk metadata | `chunk_meta` | `[u8; 32]` (chunk_id) | bincode ChunkMeta |
| View watermarks | `view_wm` | `[u8; 16]` (view_id) | `u64` (sequence) |

### What redb does NOT handle

**Chunk ciphertext data** is stored as files in pool directories:
```
$KISEKI_DATA_DIR/
  pools/
    fast-nvme/
      <chunk_id_hex>.enc    # 4KB-aligned encrypted blob
    bulk-nvme/
      <chunk_id_hex>.enc
  raft/
    db.redb                 # redb database file
```

This separation is intentional:
- redb is optimized for small key-value pairs (metadata)
- Chunk blobs are large (64KB-64MB), benefit from direct file I/O
- Future RDMA one-sided reads need file-level access (not DB pages)

## Rationale

| Criterion | redb | RocksDB | fjall | Custom files |
|-----------|------|---------|-------|-------------|
| Pure Rust | Yes | No (C++) | Yes | Yes |
| Build deps | None | cmake, clang, librocksdb | None | None |
| Binary size | ~50KB | ~5MB | ~100KB | 0 |
| ACID | Yes (COW) | Yes (WAL) | Yes (WAL) | Manual (fsync) |
| Crash recovery | Automatic | Automatic | Automatic | Manual replay |
| Compaction | None needed (B-tree) | Required (LSM) | Required (LSM) | None |
| Maturity | 1.0, used by Firefox | Very mature | Newer | N/A |
| Write amplification | Low (COW) | High (LSM) | High (LSM) | Low |

redb wins on simplicity, zero deps, and sufficient performance for
Raft log append + metadata lookup.

## Consequences

- No LSM-tree compaction complexity
- No C++ build toolchain required
- Chunk blobs as files: simple, inspectable, compatible with RDMA
- redb's COW B-tree has higher read amplification than LSM for
  range scans — acceptable for our workload (point lookups + append)
- If redb proves insufficient for high-throughput Raft log append,
  migrate to fjall (LSM, same API pattern)

## References

- redb: https://github.com/cberner/redb
- RFC 1813 §3: NFS3 procedure semantics
- build-phases.md Phase 3: "SSTable" storage (now redb B-tree)
