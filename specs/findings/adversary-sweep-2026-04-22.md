# Adversary Sweep Findings — 2026-04-22

**Scope**: Full codebase adversarial pass (8 chunks, all attack vectors)
**Fidelity baseline**: 24/24 DONE, 554/563 BDD, 361 unit+integ tests

## Findings

### ADV-S1: Raft TCP transport — unbounded message allocation
**Severity**: Critical
**Category**: Robustness > DoS
**Location**: `crates/kiseki-raft/src/tcp_transport.rs:89,199`
**Description**: Both `rpc_call()` (client) and `run_raft_rpc_server()` (server)
read a `u32` length prefix and allocate `vec![0u8; len]` without any cap.
A malicious peer can claim a 4GB message, causing OOM.
**Evidence**: `let resp_len = u32::from_be_bytes(len_buf) as usize;` then
`let mut resp_buf = vec![0u8; resp_len];` — no `MAX_RPC_SIZE` constant.
**Resolution**: Add `const MAX_RAFT_RPC_SIZE: usize = 128 * 1024 * 1024;`
(128MB, generous for snapshot transfer). Reject messages exceeding this.

### ADV-S2: Raft TCP transport — no authentication
**Severity**: Critical
**Category**: Security > Authentication
**Location**: `crates/kiseki-raft/src/tcp_transport.rs` (entire file)
**Description**: Raft inter-node communication uses plaintext TCP with no
authentication. Any network peer can inject `AppendEntries`, `Vote`, or
`FullSnapshot` RPCs, enabling leader hijack or state corruption.
**Evidence**: File header states "MVP: plaintext TCP. Production requires
mTLS (G-ADV-11)." No TLS imports or cert validation exist.
**Resolution**: Wrap `TcpStream` in `tokio_rustls::TlsStream` using the
cluster CA from `KISEKI_CA_PATH`. Verify peer cert OU matches cluster.
Known deferred item — tracked since Phase I2.

### ADV-S3: gRPC control service — no per-method authorization
**Severity**: Medium
**Category**: Security > Authorization
**Location**: `crates/kiseki-control/src/grpc/control_service.rs:84+`
**Description**: All 16 ControlService gRPC methods accept any mTLS-
authenticated peer without checking whether the caller is authorized for
the specific operation. A tenant admin could call `set_maintenance_mode`
or `register_peer` (cluster admin operations).
**Evidence**: No `Interceptor` impl, no role extraction from cert, no
per-method `require_admin()` check.
**Resolution**: Add tonic interceptor that extracts role from mTLS cert
(OU field or custom extension). Gate admin-only methods behind
`AdminRole::Admin` check from `storage_admin.rs`.

### ADV-S4: S3 gateway — no client authentication (by design)
**Severity**: High (acknowledged MVP limitation)
**Category**: Security > Authentication
**Location**: `crates/kiseki-gateway/src/s3_server.rs:27-31`
**Description**: S3 gateway accepts unsigned HTTP requests from any client.
Tenant ID is hardcoded at startup (bootstrap_tenant). No SigV4, no token,
no cert-based auth. Any network-reachable client can PUT/GET/DELETE.
**Evidence**: `struct S3State { tenant_id: OrgId }` — hardcoded, not
extracted from request. Comment: "No SigV4 auth."
**Resolution**: By design for MVP (single-tenant, dev/test). Production
requires SigV4 signature verification or mTLS client cert validation.
Not blocking — network isolation is the current mitigation.

### ADV-S5: NFS server — AUTH_NONE (by design)
**Severity**: High (acknowledged MVP limitation)
**Category**: Security > Authentication
**Location**: `crates/kiseki-gateway/src/nfs_server.rs:25-57`
**Description**: NFS server uses ONC RPC AUTH_NONE. Any NFS client on the
network can mount and access all files in the bootstrap namespace.
**Evidence**: Tenant and namespace hardcoded in `run_nfs_server()` call.
No Kerberos (AUTH_GSS) or RPCSEC_GSS integration.
**Resolution**: Standard for NFS deployments in trusted HPC networks.
Production would add AUTH_GSS/Kerberos (RFC 7530 §3). Not blocking.

### ADV-S6: Raft snapshot transfer — no size validation
**Severity**: Medium
**Category**: Robustness > DoS
**Location**: `crates/kiseki-raft/src/tcp_transport.rs:239-248`
**Description**: Snapshot installation receives the full snapshot data
via the same length-prefixed protocol as regular RPCs. A malicious peer
could send an enormous snapshot (multi-GB state machine) causing OOM.
Shares the same unbounded allocation issue as ADV-S1.
**Resolution**: Same fix as ADV-S1 — apply `MAX_RAFT_RPC_SIZE` to
snapshot transfer too. For very large snapshots, implement chunked
transfer with back-pressure.

## Verified Non-Issues (Previously Reported, Now Denied)

| Finding | Previous Status | Verification | Result |
|---------|----------------|--------------|--------|
| TenantStore TOCTOU (G-ADV-4) | Critical | Locks held correctly — org read lock spans entire project insert | **NOT VULNERABLE** |
| GC pool accounting drift (G-ADV-2) | Critical | `stored_bytes` includes EC overhead; subtract matches allocate | **NOT VULNERABLE** |
| ChunkStore thread safety (G-ADV-3) | Critical | `ChunkOps` takes `&mut self`; gateway wraps in `Mutex<Box<dyn ChunkOps>>` | **MITIGATED** |
| Debug log plaintext leak | Medium | All `eprintln!` prints only addresses, configs, warnings. No keys/plaintext | **NOT VULNERABLE** |
| Nonce reuse risk | Medium | Random 96-bit nonce + unique-per-chunk DEK. Collision ~2^-48 | **ACCEPTABLE** |

## Summary

| Severity | Count | IDs |
|----------|-------|-----|
| Critical | 2 | ADV-S1, ADV-S2 |
| High | 2 | ADV-S4, ADV-S5 (both acknowledged MVP) |
| Medium | 2 | ADV-S3, ADV-S6 |
| Low | 0 | — |
| **Total** | **6** | |

**Previously reported findings resolved**: 5 denied/mitigated on verification.

## Recommendation

**Two critical findings (ADV-S1, ADV-S2)** block production deployment:
1. **ADV-S1** (unbounded alloc) — quick fix, add size cap constant
2. **ADV-S2** (no Raft auth) — significant work, requires mTLS integration
   in TCP transport

Both are in `tcp_transport.rs` and can be addressed together. ADV-S1 is
a 5-line fix. ADV-S2 is a larger effort but well-scoped (wrap TcpStream
in TlsStream, reuse existing `TlsConfig` from kiseki-transport).

The high-severity findings (ADV-S4, ADV-S5) are acknowledged MVP
limitations with network isolation as the current mitigation. Not
blocking for HPC deployments within trusted fabric.
