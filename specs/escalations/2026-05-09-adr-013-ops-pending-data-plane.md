# Escalation: ADR-013 ops awaiting data-plane support after libfuse swap

**Date:** 2026-05-09
**From:** implementer (libfuse-swap Phase 2)
**To:** architect
**Plan:** `specs/implementation/libfuse-swap.md` §"ADR-013 parity check"
**ADR:** ADR-013 §"Supported (full semantics)"
**Severity:** Pre-existing data-plane gap surfaced by the swap; not a regression.

## Finding

The libfuse swap exposes the kiseki_fuse Filesystem trait surface for
every ADR-013 §"Supported (full)" op. Today the `KisekiFuse` data
plane (`crates/kiseki-client/src/fuse_fs.rs`) backs a subset; the rest
default-impl to `ENOSYS` or `EOPNOTSUPP`. This was true under fuser
too — the swap did not regress anything.

Per the Phase 2 step 10 amendment ("file as a separate spec-only
invariant and escalate to architect via specs/escalations/ rather than
silently regressing ADR-013"), this escalation enumerates the gap.

## Coverage matrix (post-Phase-2 commit)

| ADR-013 op | Trait method | Backed by `KisekiFuse` | Reply today | Data-plane work needed |
|---|---|---|---|---|
| open / release | `open`, `release` | n/a (fh = 0) | OK | none |
| read / write | `read`, `write` | yes | OK | none |
| create / unlink | `create`, `unlink` | yes (`create_in`/`unlink_in`) | OK | none |
| mkdir / rmdir | `mkdir`, `rmdir` | yes (`mkdir_in`/`rmdir_in`) | OK (rmdir now wired — was missing in fuser version) | none |
| rename | `rename` | yes (`rename_in`) | OK | none |
| stat / fstat / lstat | `getattr`, `lookup` | yes | OK | none |
| readdir / readdirplus | `readdir` | yes (`readdir_in`) | OK | readdirplus is the optimized variant; data plane returns one stat per entry today |
| symlink / readlink | `symlink`, `readlink` | yes (`symlink_in`/`readlink`) | OK (NEW vs fuser version) | none |
| fsync / fdatasync | `fsync` | yes (3-phase + `gateway.fsync_pending`) | OK | none |
| O_APPEND, O_CREAT, O_EXCL | (handled in write/create flags) | yes | OK | none |
| **chmod** | `setattr` mode-only | yes (`setattr(ino, Some(mode))`) | OK (NEW vs fuser version) | none |
| **chown** | `setattr` uid/gid | no | EOPNOTSUPP | extend `KisekiFuse::setattr` to accept uid/gid; persist on FileAttr (currently uid=0, gid=0 are wall-clock placeholders, not stored) |
| **truncate / ftruncate** | `setattr` size | no | EOPNOTSUPP | extend data plane to support arbitrary size mutation; today `KisekiFuse::write` is append-shaped from a buffer, not a sparse truncating mutator |
| **utimensat** | `setattr` atime/mtime/ctime | no | EOPNOTSUPP | persist per-inode mtime in `FileAttr` (today wall-clock placeholder per Bug 7 fix); add atime/ctime if needed |
| **getxattr / setxattr / listxattr / removexattr** | xattr quartet | no | ENOSYS (default impl) | add xattr storage to composition store + persistence |
| **getlk / setlk** | POSIX file locks | no | ENOSYS (default impl) | add a POSIX lock manager (per-namespace?) — non-trivial coordination work in distributed setting |
| **statfs** | `statfs` | no | ENOSYS (default impl) | hook `KisekiFuse` to a per-namespace usage rollup; gateway side already has the data |

## What this means

The libfuse-swap plan's Acceptance criterion 10 ("ADR-013 parity verified") is **partial**: every ADR-013 op has a covering trait method, but the data-plane backing for several ops is still ENOSYS / EOPNOTSUPP. The swap delivers the *plumbing*; the data-plane work is independent.

## Recommended sequencing

These are independent data-plane features, each landing in its own PR:

1. **chmod is fully wired now** — the swap's mode-only setattr path closes the chmod gap that fuser also had.
2. **chown** — small (extend FileAttr to persist uid/gid; relax the placeholder).
3. **statfs** — small (gateway-side rollup already exists; just plumb it).
4. **truncate** — medium (write-path refactor: today's `flush_take_buffer` is append-shaped).
5. **utimensat** — medium (per-inode mtime/atime persistence; touches every op that bumps mtime).
6. **xattr quartet** — large (new persistent map per inode; FUSE-API parity is easy, the storage layer + tenant-encryption are not).
7. **getlk/setlk** — largest (distributed POSIX lock manager — research-grade question; non-blocking in HPC where locks are advisory or per-rank).

## Recommendation

Mark this escalation **acknowledged** (not blocking the libfuse swap from being declared complete). The swap is independent of the data-plane gaps; closing them is its own roadmap. The plan's Acceptance criterion 10 stays "partial" until items 2-7 land.

If the architect wants criterion 10 to gate the swap as "full," items 2-7 become Phase-2 sub-work, which would extend Phase 2 by weeks or months.

## Cross-references

- `specs/architecture/adr/013-posix-semantics-scope.md` — the operation matrix.
- `specs/implementation/libfuse-swap.md` §"ADR-013 parity check" + step 10 amendment.
- `crates/kiseki-client/src/fuse_fs.rs` — KisekiFuse data plane (1256 lines).
- `crates/kiseki-client/src/fuse_daemon.rs` — libfuse trait impl (the new file post-swap).
