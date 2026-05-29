# ADR-013: POSIX Semantics Scope

**Status**: Accepted
**Date**: 2026-04-17
**Context**: A-ADV-4 (POSIX semantics depth)

## Decision

POSIX support via FUSE with explicit compatibility matrix.

### Supported (full semantics)

| Operation | Notes |
|---|---|
| open, close, read, write | Standard file I/O |
| create, unlink, mkdir, rmdir | Directory operations |
| rename (within namespace) | Atomic within shard |
| stat, fstat, lstat | File metadata |
| chmod, chown | Permission changes (stored in delta attributes) |
| readdir, readdirplus | Directory listing from view |
| symlink, readlink | Stored as inline data in delta |
| truncate, ftruncate | Composition resize |
| fsync, fdatasync | Flush to durable (delta committed) |
| extended attributes (xattr) | getxattr, setxattr, listxattr, removexattr |
| POSIX file locks (fcntl) | Per-gateway lock state |
| O_APPEND | Atomic append via delta |
| O_CREAT, O_EXCL | Atomic create-if-not-exists |

### Supported (limited semantics)

| Operation | Limitation |
|---|---|
| rename (cross-namespace) | Returns EXDEV (ADR: I-L8) |
| hard links | Within namespace only; cross-namespace returns EXDEV |
| sparse files | Holes tracked in composition; zero-fill on read |
| O_DIRECT | Bypasses client cache but still goes through FUSE |
| flock (advisory) | Best-effort; not guaranteed across gateway failover |

### Not supported

| Operation | Reason |
|---|---|
| mmap (shared, writable) | Distributed shared writable mmap requires page-level coherence — not tractable for a distributed system at HPC scale. Read-only mmap is supported. **The FUSE client returns ENOTSUP with a log message: "writable shared mmap not supported; use write() instead."** |
| ACLs (POSIX.1e) | Unix permissions only (uid/gid/mode). POSIX ACLs add complexity without significant benefit for the target workload. Revisit if needed. |
| chroot, pivot_root | Filesystem-level operations, not meaningful for FUSE mount |

## Consequences

- mmap restriction documented prominently (HPC users expect it)
- Read-only mmap works (useful for model loading)
- Writable mmap requires application changes (use write() instead)
- No POSIX ACLs simplifies the permission model

## Write-ack consistency (ADR-047 amendment, 2026-05-29)

POSIX / NFS / FUSE are **synchronous-apply** surfaces: `close()` / `fsync()` /
the streaming CommitStream block until the write's intent is *applied*
(visible) on the owning shard — preserving **close-to-open** consistency (a
subsequent `open()` on any node sees the closed file's data). The ADR-047
decoupled-write-ack relaxation (ack on quorum-durable intent, ordering
applied asynchronously) does **NOT** apply to these surfaces; they keep the
strict path. They still benefit from #137 (parallel chunk store) + EC/Raft
parallelization — just not the async ack. The relaxation is granted only to
surfaces whose own contract tolerates bounded-stale visibility
(S3/object/native — ADR-014). Rationale + the per-surface split: ADR-047 §F-3.
