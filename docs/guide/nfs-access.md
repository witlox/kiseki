# NFS Access

Kiseki exposes an NFS gateway on port 2049 (configurable via
`KISEKI_NFS_ADDR`) supporting both NFSv3 and NFSv4.2. The gateway
translates NFS operations into reads and writes against materialized
views and the composition log.

## Protocol Support

| Protocol | Status | Notes |
|----------|--------|-------|
| NFSv3 | Supported | Stateless, lower overhead |
| NFSv4.2 | Supported | Stateful, with lock support and extended attributes |

## Mounting

### Basic Mount

```bash
mount -t nfs <node>:/ /mnt/kiseki
```

With explicit version and options:

```bash
# NFSv4.2
mount -t nfs -o vers=4.2,proto=tcp <node>:/ /mnt/kiseki

# NFSv3
mount -t nfs -o vers=3,proto=tcp <node>:/ /mnt/kiseki
```

### Docker Compose (Development)

When using the development Docker Compose stack, the NFS port is
published to the host:

```bash
mount -t nfs -o vers=4.2,proto=tcp,port=2049 127.0.0.1:/ /mnt/kiseki
```

### fstab Entry

```
<node>:/ /mnt/kiseki nfs vers=4.2,proto=tcp,hard,intr 0 0
```

## Authentication

| Mode | Use case | Notes |
|------|----------|-------|
| AUTH_SYS | Development and testing | UID/GID-based, no Kerberos |
| Kerberos (RPCSEC_GSS) | Production | krb5, krb5i, or krb5p security flavors |

In development (Docker Compose), AUTH_SYS is used with no additional
configuration. For production deployments, Kerberos provides
authentication and optional integrity/privacy protection on the wire.

Kiseki always encrypts data at rest regardless of the NFS authentication
mode. The gateway performs tenant-layer encryption: clients send
plaintext over TLS to the gateway, and the gateway encrypts before
writing to the log and chunk store.

## Supported Operations

### Full Semantics

| Operation | Notes |
|-----------|-------|
| `open`, `close`, `read`, `write` | Standard file I/O |
| `create`, `unlink` | File creation and deletion |
| `mkdir`, `rmdir` | Directory creation and deletion |
| `rename` (within namespace) | Atomic within shard |
| `stat`, `fstat`, `lstat` | File metadata |
| `chmod`, `chown` | Permission changes (stored in delta attributes) |
| `readdir`, `readdirplus` | Directory listing from materialized view |
| `symlink`, `readlink` | Stored as inline data in delta |
| `truncate`, `ftruncate` | Composition resize |
| `fsync`, `fdatasync` | Flush to durable (delta committed to Raft quorum) |
| Extended attributes (xattr) | `getxattr`, `setxattr`, `listxattr`, `removexattr` |
| POSIX file locks (`fcntl`) | Per-gateway lock state |
| `O_APPEND` | Atomic append via delta |
| `O_CREAT`, `O_EXCL` | Atomic create-if-not-exists |

### Limited Semantics

| Operation | Limitation |
|-----------|-----------|
| `rename` (cross-namespace) | Returns `EXDEV` -- cannot rename across shards |
| Hard links | Within namespace only; cross-namespace returns `EXDEV` |
| Sparse files | Holes tracked in composition; zero-fill on read |
| `O_DIRECT` | Bypasses client cache but still traverses the gateway |
| `flock` (advisory) | Best-effort; not guaranteed across gateway failover |

### Not Supported

| Operation | Reason |
|-----------|--------|
| Writable shared `mmap` | Distributed shared writable mmap requires page-level coherence that is not tractable at HPC scale. Read-only mmap is supported. The gateway returns `ENOTSUP`. See ADR-013. |
| POSIX ACLs (POSIX.1e) | Unix permissions only (uid/gid/mode). POSIX ACLs add complexity without benefit for the target workloads. |

## Namespace Mapping

The NFS root (`/`) lists the tenant's namespaces as top-level
directories. Each namespace contains the compositions (files and
directories) belonging to that namespace. This is analogous to the S3
bucket mapping -- the same namespace appears as a bucket via S3 and as a
top-level directory via NFS.

```
/mnt/kiseki/
  training/          <- namespace "training"
    imagenet/
      train.tar
      val.tar
  checkpoints/       <- namespace "checkpoints"
    epoch-001.pt
```

## Performance Considerations

- **Readdir performance** -- directory listings are served from
  materialized views, not reconstructed from the log on each request.
  Views are updated incrementally by stream processors.

- **Write path** -- writes flow through the gateway to the composition
  context, which appends deltas to the shard log. An `fsync` ensures
  the delta is committed to a Raft quorum before returning.

- **Concurrent access** -- multiple NFS clients can read the same files
  concurrently. Write contention within a shard is serialized by the
  Raft leader.

- **Large files** -- large files are chunked using content-defined
  chunking (Rabin fingerprinting). Byte-range reads are served by
  fetching only the relevant chunks.

## Limitations Summary

1. **No writable shared mmap** -- applications that use writable shared
   memory-mapped files must use `write()` instead. Read-only mmap works
   and is useful for model loading.

2. **Cross-namespace rename returns EXDEV** -- renaming a file from one
   namespace to another requires a copy-and-delete at the application
   level, same as moving files across filesystem boundaries on a
   traditional system.

3. **No POSIX ACLs** -- only standard Unix permissions (mode bits).
   Fine-grained access control is handled by Kiseki's tenant IAM model,
   not filesystem-level ACLs.

4. **Lock state is per-gateway** -- POSIX file locks (`fcntl`) are
   maintained by the gateway instance. If a gateway fails over, lock
   state is lost. Advisory locks (`flock`) are best-effort.
