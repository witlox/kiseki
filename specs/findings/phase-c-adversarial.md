# Phase C Adversarial Review (Protocol Interfaces)

**Date**: 2026-04-20. **Reviewer**: Adversary role.

## CRITICAL (4)

### C-ADV-1: Unbounded memory allocation in NFS Record Marking decoder
- **Location**: `crates/kiseki-gateway/src/nfs_xdr.rs:218-221`
- **Issue**: `read_rm_message()` trusts wire-supplied length (up to 2GB), calls `buf.resize()` without bounds
- **Impact**: OOM DoS — attacker sends 0x7FFF_FFFF frame header, server allocates 2GB
- **Resolution**: Cap frame size (e.g., 16MB max)

### C-ADV-2: Predictable NFSv4.2 session IDs enable hijacking
- **Location**: `crates/kiseki-gateway/src/nfs4_server.rs:85-88`
- **Issue**: Session ID = `client_id.to_be_bytes() || 1u64.to_be_bytes()` — fully predictable
- **Impact**: Session hijacking, arbitrary file read/write
- **Resolution**: Use cryptographic random bytes for session IDs

### C-ADV-3: NFSv4.2 COMPOUND ops unbounded (DoS)
- **Location**: `crates/kiseki-gateway/src/nfs4_server.rs:169,175`
- **Issue**: `num_ops` read from wire, loop runs `0..num_ops` with no cap
- **Impact**: CPU exhaustion with `num_ops=0xFFFF_FFFF`
- **Resolution**: Cap at 32 ops per COMPOUND (RFC default)

### C-ADV-4: NFSv4.2 does not validate session ownership
- **Location**: `crates/kiseki-gateway/src/nfs4_server.rs:313-340`
- **Issue**: `op_sequence()` checks session exists but not that caller owns it
- **Impact**: Cross-client session poisoning
- **Resolution**: Bind session to connection/peer identity

## HIGH (4)

### C-ADV-5: XDR read_opaque() no maximum length
- **Location**: `crates/kiseki-gateway/src/nfs_xdr.rs:112-122`
- **Issue**: Allocates Vec based on wire-supplied u32 length
- **Resolution**: Add configurable max opaque size

### C-ADV-6: S3 PUT body size unbounded
- **Location**: `crates/kiseki-gateway/src/s3_server.rs:45-64`
- **Issue**: No explicit body size limit (relies on axum default 2MB)
- **Resolution**: Add explicit `axum::extract::DefaultBodyLimit`

### C-ADV-7: NFSv4.2 client ID is predictable counter
- **Location**: `crates/kiseki-gateway/src/nfs4_server.rs:78-82`
- **Issue**: `next_client_id` increments from 1 — enables session prediction
- **Resolution**: Use random u64 for client IDs

### C-ADV-8: FUSE inode table unbounded
- **Location**: `crates/kiseki-client/src/fuse_fs.rs:64-66`
- **Issue**: HashMap grows without limit on create()
- **Resolution**: Add max inode count, return ENOSPC when full

## MEDIUM (4)

### C-ADV-9: FUSE unlink() doesn't delete backend data
- **Location**: `crates/kiseki-client/src/fuse_fs.rs:158-162`
- **Issue**: Removes inode mapping only, composition persists in storage
- **Resolution**: Call CompositionOps::delete on unlink

### C-ADV-10: S3 tenant ID hardcoded (bootstrap)
- **Location**: `crates/kiseki-gateway/src/s3_server.rs:26-27`
- **Issue**: All S3 requests use same tenant_id — I-T1 bypassed
- **Status**: Known, documented as dev-mode. Tracked in D-ADV findings.

### C-ADV-11: NFSv4.2 EXCHANGE_ID leaks server identity
- **Location**: `crates/kiseki-gateway/src/nfs4_server.rs:248-252`
- **Issue**: Returns "kiseki" + "kiseki.local" in server_owner
- **Resolution**: Use generic or configurable server identity

### C-ADV-12: S3 DELETE always returns 204 (unimplemented)
- **Location**: `crates/kiseki-gateway/src/s3_server.rs:122-128`
- **Status**: Known MVP gap, documented

## LOW (2)

### C-ADV-13: Arc<G> blanket impl sound but implicit
- **Location**: `crates/kiseki-gateway/src/ops.rs:65-72`
- **Status**: Safe — Rust type system handles Send/Sync

### C-ADV-14: NFS READDIR returns global handle registry
- **Location**: `crates/kiseki-gateway/src/nfs_ops.rs:234-257`
- **Status**: Currently returns only registered handles (safe due to per-context scope)
