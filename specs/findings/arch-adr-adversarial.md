# Architecture Adversarial Review: ADR-022 (redb) + ADR-023 (RFC)

**Date**: 2026-04-20. **Reviewer**: Adversary (architecture mode).

## CRITICAL (6)

### ARCH-C1: redb append throughput unvalidated for Raft workload
- COW B-tree has higher write amplification than WAL for sequential append
- Need benchmarks at 1TB+ log size before committing
- **Resolution**: Benchmark redb vs fjall vs custom WAL; document SLO

### ARCH-C2: Chunk-as-files risks inode exhaustion at scale
- 100TB cluster ≈ 1.6B chunks ≈ inode table overflow on ext4/xfs
- **Resolution**: Consider pool files (one large file per device, offsets in redb) or document inode provisioning requirements

### ARCH-C3: EC fragment placement across devices undefined
- No mapping pool → physical devices, no reverse index for repair
- **Resolution**: Extend AffinityPool with device list, implement placement algorithm

### ARCH-C4: NFSv3 missing REMOVE/RENAME/FSSTAT/FSINFO
- Real `mount -t nfs` will fail on first `rm` or `df`
- **Resolution**: Implement REMOVE + RENAME (HIGH priority), FSSTAT/FSINFO (MEDIUM)

### ARCH-C5: NFSv4.2 missing OPEN/CLOSE/LOCK
- Stateful file access broken — READ/WRITE without prior OPEN violates RFC
- **Resolution**: Implement stateful OPEN with stateid, validate on I/O

### ARCH-C6: S3 ListObjectsV2 missing — contradicts ADR-014
- ADR-014 says "Supported (full)", ADR-023 says "Not yet"
- `aws s3 ls`, `boto3.list_objects_v2()` both fail
- **Resolution**: Reconcile ADRs, implement ListObjectsV2

## HIGH (5)

### ARCH-H1: redb file growth/compaction semantics unclear
- COW creates dead pages — does redb reclaim them?
- **Resolution**: Document redb space management, test at 1M+ entries

### ARCH-H2: Reader/writer lock contention (Mutex vs RwLock)
- Stream processor blocks on Raft writes via shared Mutex
- **Resolution**: Use RwLock or decouple state snapshots

### ARCH-H3: Raft snapshot/restore mechanism undefined
- New replicas must replay entire log (hours at scale)
- **Resolution**: Implement install_snapshot via redb serialization

### ARCH-H4: Wire-format testing insufficient — no fuzzing
- BDD tests check semantics not wire bytes
- **Resolution**: Add fuzz tests for XDR/ONC RPC malformed inputs

### ARCH-H5: redb + NVMe direct I/O status unknown
- Page cache may double memory footprint at scale
- **Resolution**: Verify redb page alignment, measure RSS vs file size

## MEDIUM (3)

### ARCH-M1: No migration path redb → fjall
### ARCH-M2: LOOKUP consistency model unclear
### ARCH-M3: Chunk metadata recovery procedure undefined
