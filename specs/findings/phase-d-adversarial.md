# Phase D Adversarial Review

**Date**: 2026-04-20. **Reviewer**: Adversary role.

## CRITICAL (4)

### D-ADV-1: gRPC auth interceptor defined but not wired
- **Location**: `crates/kiseki-log/src/grpc.rs:189-195`, `crates/kiseki-server/src/runtime.rs:167`
- **Issue**: `auth_interceptor()` is a no-op pass-through, never attached to `LogServiceServer`
- **Impact**: All LogService RPCs bypass tenant identity validation (I-Auth1, I-T1)
- **Resolution**: Wire via `tonic::service::interceptor()`, implement OrgId extraction from peer certs

### D-ADV-2: S3 TLS flag silently ignored
- **Location**: `crates/kiseki-gateway/src/s3_server.rs:149-153`
- **Issue**: `use_tls=true` logs a warning and falls back to plaintext
- **Impact**: False security — admin believes TLS is active, data flows unencrypted
- **Resolution**: Implement `tokio_rustls::TlsAcceptor` wrapping, panic if TLS requested but unavailable

### D-ADV-3: ViewStore unreachable from protocol gateways
- **Location**: `crates/kiseki-server/src/runtime.rs:83-162`
- **Issue**: `view_store` moved into stream processor closure, not shared with gateways
- **Impact**: Staleness enforcement (I-K9, I-V3) not applied to reads
- **Resolution**: Wrap in `Arc<Mutex<ViewStore>>`, pass to gateway for read validation

### D-ADV-4: NFS LOOKUP always returns NOENT
- **Location**: `crates/kiseki-gateway/src/nfs_ops.rs:228-231`
- **Issue**: `lookup_by_name()` returns `None` unconditionally (stub)
- **Impact**: NFS directory traversal broken — clients cannot find files by name
- **Resolution**: Implement directory index (name → composition_id), populate on CREATE

## HIGH (5)

### D-ADV-5: NFS CREATE ignores filename
- **Location**: `crates/kiseki-gateway/src/nfs3_server.rs:267-268`
- **Issue**: `_dir_fh` and `_name` discarded, writes empty Vec
- **Resolution**: Store filename in directory index, pass data from CREATE

### D-ADV-6: NFS READDIR returns UUID filenames
- **Location**: `crates/kiseki-gateway/src/nfs_ops.rs:248-251`
- **Issue**: Entries named by `composition_id.0.to_string()`, not semantic names
- **Resolution**: Use directory entry table with human-readable names

### D-ADV-7: TLS 1.3 enforced in Go but not Rust
- **Location**: Go `main.go:102` vs Rust `runtime.rs:29-33`
- **Issue**: Go enforces `tls.VersionTLS13`, Rust uses tonic default (TLS 1.2+)
- **Resolution**: Set min TLS version on Rust `ServerTlsConfig`

### D-ADV-8: Go BDD assertions structural not behavioral
- **Location**: `control/tests/acceptance/steps_advisory_policy.go` (70% of then steps)
- **Issue**: Steps check struct fields exist, don't invoke domain validation logic
- **Resolution**: Each then step must call real domain functions

### D-ADV-9: Federation steps are flag-checks only
- **Location**: `control/tests/acceptance/steps_federation.go`
- **Issue**: No actual peer sync, replication, or residency blocking tested
- **Resolution**: Wire FederationReg with actual exchange logic

## MEDIUM (1)

### D-ADV-10: Advisory policy steps don't invoke control plane RPCs
- **Location**: `control/tests/acceptance/steps_advisory_policy.go`
- **Issue**: All steps manipulate local ControlWorld, no gRPC calls
- **Resolution**: Stand up gRPC server in test, issue real RPCs

## LOW (1)

### D-ADV-11: Stream processor recreated every 100ms poll
- **Location**: `crates/kiseki-server/src/runtime.rs:145-162`
- **Issue**: New TrackedStreamProcessor created each iteration
- **Status**: Acceptable — watermark state lives in ViewStore, processor is stateless
