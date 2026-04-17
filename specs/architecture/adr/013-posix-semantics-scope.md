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
| mmap (shared, writable) | Distributed shared writable mmap requires page-level coherence — not tractable for a distributed system at HPC scale. Read-only mmap is supported. |
| ACLs (POSIX.1e) | Unix permissions only (uid/gid/mode). POSIX ACLs add complexity without significant benefit for the target workload. Revisit if needed. |
| chroot, pivot_root | Filesystem-level operations, not meaningful for FUSE mount |

## Consequences

- mmap restriction documented prominently (HPC users expect it)
- Read-only mmap works (useful for model loading)
- Writable mmap requires application changes (use write() instead)
- No POSIX ACLs simplifies the permission model
