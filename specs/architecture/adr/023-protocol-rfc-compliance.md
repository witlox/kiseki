# ADR-023: Protocol RFC Compliance Scope

**Status**: Accepted.
**Date**: 2026-04-20.
**Deciders**: Architect + implementer.

## Context

Kiseki exposes three protocol interfaces: S3 HTTP, NFSv3, NFSv4.2.
ADR-013 (POSIX semantics) and ADR-014 (S3 API scope) define the
functional subset but don't reference specific RFC sections or define
wire-format compliance testing.

Now that wire protocol implementations exist, we need to codify
which RFC requirements are met and how compliance is verified.

## Decision

### Protocol scope

| Protocol | Standard | Implemented Subset | Total in Standard |
|----------|----------|-------------------|-------------------|
| NFSv3 | RFC 1813 | 7 of 22 procedures | 22 procedures |
| NFSv4.2 | RFC 7862 | 10 of ~60 operations | ~60 operations |
| S3 | AWS S3 API | 5 of 40+ operations | 40+ operations |

### NFSv3 (RFC 1813) — implemented procedures

| # | Procedure | Status | Notes |
|---|-----------|--------|-------|
| 0 | NULL | Implemented | Ping/health check |
| 1 | GETATTR | Implemented | File/directory attributes |
| 3 | LOOKUP | Implemented | Name → file handle resolution |
| 6 | READ | Implemented | Byte-range file read |
| 7 | WRITE | Implemented | File data write |
| 8 | CREATE | Implemented | Create new file + directory index entry |
| 16 | READDIR | Implemented | Directory listing with real filenames |

Not implemented: SETATTR, ACCESS, READLINK, SYMLINK, MKNOD, REMOVE,
RMDIR, RENAME, LINK, READDIRPLUS, FSSTAT, FSINFO, PATHCONF, COMMIT.

### NFSv4.2 (RFC 7862) — implemented COMPOUND operations

| Op | Name | Status | Notes |
|----|------|--------|-------|
| 9 | GETATTR | Implemented | Bitmap-selected attributes |
| 10 | GETFH | Implemented | Return current file handle |
| 15 | LOOKUP | Stub (delegates to directory index) | |
| 24 | PUTROOTFH | Implemented | Set root file handle |
| 25 | READ | Implemented | Via stateid + offset + count |
| 38 | WRITE | Implemented | Via stateid + offset + stable |
| 42 | EXCHANGE_ID | Implemented | Random client IDs (C-ADV-7) |
| 43 | CREATE_SESSION | Implemented | Random session IDs (C-ADV-2) |
| 44 | DESTROY_SESSION | Implemented | Session teardown |
| 53 | SEQUENCE | Implemented | Per-request sequencing |
| 63 | IO_ADVISE | Implemented | Accepted (advisory integration pending) |

### S3 API — implemented operations

| Operation | HTTP Method | Status |
|-----------|------------|--------|
| PutObject | PUT /:bucket/:key | Implemented |
| GetObject | GET /:bucket/:key | Implemented |
| HeadObject | HEAD /:bucket/:key | Implemented |
| DeleteObject | DELETE /:bucket/:key | Stub (returns 204) |
| ListObjectsV2 | GET /:bucket | Not yet |

### Compliance testing approach

1. **BDD feature files** map to RFC sections:
   - `specs/features/nfs3-rfc1813.feature` (14 scenarios)
   - `specs/features/nfs4-rfc7862.feature` (20 scenarios)
   - `specs/features/s3-api.feature` (10 scenarios)

2. **Wire format validation** via Python e2e tests:
   - NFS: raw TCP with `struct.pack` for ONC RPC framing
   - S3: `requests` library for HTTP

3. **Real client interop** (future):
   - NFS: `mount -t nfs -o nfsvers=3,tcp` in Docker
   - S3: `boto3` / `aws-cli`

## Consequences

- Clear documentation of what's implemented vs what's not
- BDD scenarios serve as living compliance spec
- Real client interop deferred until wire format proven via raw tests
- Expanding the subset (e.g., adding REMOVE, RENAME) requires:
  new BDD scenario → new step definition → implementation → test green

## References

- RFC 1813: NFS Version 3 Protocol Specification
- RFC 7862: NFS Version 4.2 Protocol
- RFC 5531: ONC RPC Version 2
- RFC 4506: XDR: External Data Representation Standard
- AWS S3 API Reference
- ADR-013: POSIX Semantics Scope
- ADR-014: S3 API Scope
