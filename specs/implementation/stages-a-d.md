# Stages A–D: Secure → Durable → Usable → Complete

**Status**: Planned. **Created**: 2026-04-19.
**Prerequisite**: Infrastructure integration (WI-1 through WI-4) complete.

Stages are strictly sequential. Each sub-step gets an adversarial
review before advancing.

```
Stage A (secure)  → Stage B (durable) → Stage C (usable) → Stage D (complete)
```

---

## Stage A: Make it secure

Close the High/Medium findings from WI-4 and Phase 2. After this
stage, no unauthenticated path exists.

| Step | What | Crates | Adv gate |
|------|------|--------|----------|
| A.1 | Connection + handshake timeouts | kiseki-transport | a1-gate |
| A.2 | X.509 OU/SAN parsing (x509-parser) | kiseki-transport | a2-gate |
| A.3 | CRL checking | kiseki-transport | a3-gate |
| A.4 | Wire mTLS into gRPC listeners | kiseki-server, kiseki-transport | a4-gate |
| A.5 | Graceful shutdown (SIGTERM drain) | kiseki-server | a5-gate |

**Exit**: Both gRPC listeners require mTLS. Real OrgId from X.509 OU
or SPIFFE SAN. CRL checking. Timeouts. Graceful shutdown.

**Risks**: Tonic `ServerTlsConfig` vs raw rustls version compatibility.
`x509-parser` must not pull non-FIPS crypto.

---

## Stage B: Make it durable

Replace local command logs with openraft consensus. Close CRITICAL
invariants I-L2 and I-K12.

| Step | What | Crates | Adv gate |
|------|------|--------|----------|
| B.1 | openraft type scaffolding + Raft transport proto | new kiseki-raft, kiseki-proto | b1-gate |
| B.2 | kiseki-keymanager openraft integration | kiseki-keymanager, kiseki-server | b2-gate |
| B.3 | kiseki-log openraft (per-shard groups) | kiseki-log, kiseki-server | b3-gate |
| B.4 | kiseki-audit openraft (append-only) | kiseki-audit, kiseki-server | b4-gate |
| B.5 | Raft transport service + cluster bootstrap | kiseki-server, kiseki-proto | b5-gate |

**Exit**: I-L2 and I-K12 closed. 3-node replication tested. Leader
failover works. Key material encrypted in Raft logs.

**Risks**: openraft 0.10 alpha API instability. Per-shard Raft group
resource management. Snapshot encryption for key material.

---

## Stage C: Make it usable

Wire data-path gRPC services so clients can write/read over the
network. Close the wi4-gate Medium finding (contexts not injected).

| Step | What | Crates | Adv gate |
|------|------|--------|----------|
| C.1 | LogService gRPC | kiseki-log, kiseki-proto, kiseki-server | c1-gate |
| C.2 | ChunkService gRPC | kiseki-chunk, kiseki-proto, kiseki-server | c2-gate |
| C.3 | CompositionService gRPC | kiseki-composition, kiseki-proto, kiseki-server | c3-gate |
| C.4 | ViewService gRPC | kiseki-view, kiseki-proto, kiseki-server | c4-gate |
| C.5 | End-to-end data-path integration test | kiseki-server tests | c5-gate |

**Exit**: Four data-path gRPC services registered. Integration test:
write delta via gRPC → read back → verify audit event.

**Risks**: Proto type mapping fidelity (WI-3 lesson). Service
definitions must be added to .proto files. Large payload streaming
deferred (unary RPCs with documented size limit).

---

## Stage D: Make it complete

Protocol gateways, FUSE mount, Go gRPC servers, Python e2e.

| Step | What | Crates/packages | Adv gate |
|------|------|-----------------|----------|
| D.1 | S3 gateway implementation | kiseki-gateway | d1-gate |
| D.2 | NFS gateway implementation | kiseki-gateway | d2-gate |
| D.3 | FUSE mount in kiseki-client | kiseki-client | d3-gate |
| D.4 | Go ControlService + AuditExportService gRPC | control/pkg/*, control/cmd/* | d4-gate |
| D.5 | Python end-to-end test suite | tests/e2e/ | d5-gate |

**Exit**: S3 + NFS gateways serve core subsets. FUSE client mounts
with client-side encryption. Go gRPC servers handle all control-plane
RPCs. Python e2e passes against a 3-node cluster with cross-protocol
consistency test (write via FUSE, read via S3).

**Risks**: NFS crate maturity (Rust NFSv4.1 ecosystem is thin). FUSE
on macOS requires macFUSE/FUSE-T. Python e2e cluster management
complexity.

---

## Tracking

| Step | Status | Commit | Adv gate | Notes |
|------|--------|--------|----------|-------|
| A.1 | done | — | passed | Connect 5s + handshake 10s, configurable, 2 tests |
| A.2 | done | — | passed | x509-parser OU/SAN extraction, SPIFFE fallback, 1 test |
| A.3 | pending | — | — | CRL |
| A.4 | pending | — | — | mTLS on gRPC |
| A.5 | pending | — | — | Graceful shutdown |
| B.1 | pending | — | — | openraft scaffold |
| B.2 | pending | — | — | Keymanager Raft |
| B.3 | pending | — | — | Log Raft (per-shard) |
| B.4 | pending | — | — | Audit Raft |
| B.5 | pending | — | — | Cluster bootstrap |
| C.1 | pending | — | — | LogService gRPC |
| C.2 | pending | — | — | ChunkService gRPC |
| C.3 | pending | — | — | CompositionService gRPC |
| C.4 | pending | — | — | ViewService gRPC |
| C.5 | pending | — | — | E2E integration test |
| D.1 | pending | — | — | S3 gateway |
| D.2 | pending | — | — | NFS gateway |
| D.3 | pending | — | — | FUSE mount |
| D.4 | pending | — | — | Go gRPC servers |
| D.5 | pending | — | — | Python e2e suite |
