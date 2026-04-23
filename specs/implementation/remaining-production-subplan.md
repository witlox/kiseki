# Remaining Production Subplan

**Date**: 2026-04-23
**Covers**: All remaining MVP→Production items before adversarial + integrator pass

## Items covered

- **Slice A**: Consensus + Shard Lifecycle (3.2, 3.3, 3.4, 3.5, 2.5, 7.1)
- **Slice C**: Admin + Gateway (8.1, 5.2, 5.7, 8.2, 1.2)
- **WS 1.4**: External KMS providers (ADR-028)
- **WS 5.3**: NFS Kerberos authentication
- **WS 6.4**: Federation async replication
- **WS 6.5**: Chaos testing framework

## Completed (prior slices)

- WS 4 (transport): all done
- WS 6.1-6.3 (observability): all done
- WS 5.1, 5.4-5.6 (S3 auth, multi-tenant, FUSE, client bindings): done
- WS 1.1, 1.3 (key rotation, crypto-shred): done
- WS 2.1-2.6, 2.8 (storage engine, GPU-direct): done
- WS 3.1 (Raft mTLS): done
- WS 7.2, 7.3 (storage + network failures): done
- ADR-031 client cache: done

---

## Phase 1: Consensus Hardening (Slice A — WS 3.2-3.5)

### 1.1 Dynamic membership changes (3.2)

File: `kiseki-raft/src/membership.rs`

- `add_member(raft, node_id, addr)` → `raft.add_learner()` → catch up → `raft.change_membership()`
- `remove_member(raft, node_id)` → `raft.change_membership()` excluding node
- Graceful decommission: add new → catch up → promote → demote old
- Wire to control plane: `AddShardMember`, `RemoveShardMember` RPCs

### 1.2 Clock skew detection (3.5)

File: `kiseki-common/src/time.rs` (extend)

- `ClockSkewDetector`: compare HLC physical_ms across Raft peers
- Alert if skew > configurable threshold (default 500ms)
- Refuse writes if skew > hard limit (default 5s)
- Log quality degradation events

### 1.3 Shard split execution (2.5)

File: `kiseki-log/src/split.rs`

- `execute_split(shard, midpoint)`: compute key range midpoint,
  create new shard, redistribute deltas, atomic cutover
- New shard gets own Raft group via membership change
- Write buffering during split (brief latency bump per I-O1)
- Auto-split monitor in `auto_split.rs` calls execute when
  I-L6 ceiling breached

### 1.4 Persistent log hardening (3.3)

File: `kiseki-raft/src/redb_raft_log_store.rs` (extend)

- Crash recovery tests: write entries → crash → reopen → verify
- Concurrent load tests: parallel writes + reads
- Benchmark: Raft commit latency with redb fsync

### 1.5 Snapshot transfer under load (3.4)

File: `kiseki-raft/src/tcp_transport.rs` (extend)

- Test with GB-scale state machines
- Progress reporting during long transfers
- Chunked transfer if snapshot > `MAX_RAFT_RPC_SIZE`

### 1.6 Consensus failure validation (7.1)

- F-C1: Leader loss → election → new leader (integration test)
- F-C2: Quorum loss → shard unavailable (integration test)
- F-C3: Clock skew → handled by 1.2

**Effort**: 8-12 sessions

---

## Phase 2: Admin CLI + Gateway Completion (Slice C)

### 2.1 Admin CLI (8.1)

New crate: `kiseki-admin` (or binary in `kiseki-server`)

Subcommands:
- `kiseki-admin status` — cluster health, node list, shard map
- `kiseki-admin pool list|show|rebalance` — pool management
- `kiseki-admin device list|show|evacuate|remove` — device lifecycle
- `kiseki-admin shard list|show|split|migrate` — shard ops
- `kiseki-admin tenant list|show|quota` — tenant info
- `kiseki-admin maintenance on|off` — cluster maintenance mode

Connects via gRPC to `ControlService`. Tabular output (human) +
JSON output (`--json` flag).

### 2.2 S3 bucket CRUD (5.2)

File: `kiseki-gateway/src/s3_server.rs` (extend)

- `PUT /<bucket>` → `CreateBucket` (namespace creation via control plane)
- `DELETE /<bucket>` → `DeleteBucket`
- `HEAD /<bucket>` → `HeadBucket` (existence check)
- `GET /` → `ListBuckets` (tenant's namespaces)
- Bucket → namespace mapping via `TenantConfig`

### 2.3 ADR-030 threshold feedback loop (5.7)

File: `kiseki-control/src/threshold.rs`

- Control plane aggregates `NodeMetadataCapacity` from all nodes
- Computes per-shard threshold from min voter budget
- Commits threshold updates via Raft `ShardConfig` change
- Emergency: hard-limit breach → threshold floor via gRPC

### 2.4 Upgrade / schema migration (8.2)

File: `kiseki-server/src/migration.rs`

- redb schema version table in each database
- On startup: check version, run migration if needed
- Proto backward compatibility: new fields always optional
- Version mismatch between nodes → reject join

### 2.5 Re-encryption orchestration (1.2)

File: `kiseki-keymanager/src/reencrypt.rs`

- Wire rotation_monitor → rewrap_worker for background re-encryption
- Batched, rate-limited, resumable after crash
- Progress tracking via `RewrapProgress`
- Metric: `kiseki_rewrap_progress` gauge

**Effort**: 10-15 sessions

---

## Phase 3: External KMS Providers (WS 1.4)

File: `kiseki-keymanager/src/providers/`

### 3.1 Provider trait (already spec'd in ADR-028)

```rust
pub trait TenantKmsProvider: Send + Sync {
    async fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>>;
    async fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>>;
    async fn rotate(&self) -> Result<KmsEpochId>;
    async fn health_check(&self) -> Result<KmsHealth>;
}
```

### 3.2 Vault provider

- HashiCorp Vault Transit secrets engine
- `vault_client`: HTTP API via `reqwest` (feature-gated)
- Wrap: `POST /transit/encrypt/<key>`, Unwrap: `POST /transit/decrypt/<key>`
- Rotation: `POST /transit/keys/<key>/rotate`
- mTLS to Vault server

### 3.3 AWS KMS provider

- AWS KMS via `aws-sdk-kms` (feature-gated)
- Wrap: `Encrypt`, Unwrap: `Decrypt`
- Rotation: `CreateKey` with rotation schedule
- Region-aware endpoint

### 3.4 Internal provider (already exists)

- Wire existing `RaftKeyStore` as the internal provider backend

### 3.5 Provider validation on tenant activation (I-K18)

- Connectivity test, wrap/unwrap round-trip, cert chain validation

### 3.6 OIDC/JWT tenant IdP validation (I-Auth2)

File: `kiseki-control/src/idp.rs`

Per-tenant OIDC configuration in `TenantConfig`:
```rust
pub struct TenantIdpConfig {
    pub issuer_url: String,          // e.g. https://keycloak.site/realms/hpc
    pub client_id: String,           // Kiseki's client_id at the IdP
    pub jwks_uri: Option<String>,    // override; default: {issuer}/.well-known/jwks.json
    pub audience: Option<String>,    // expected JWT "aud" claim
    pub claim_mapping: ClaimMapping, // which JWT claims → OrgId, project, workload
}

pub struct ClaimMapping {
    pub org_claim: String,       // default: "org" or "tenant_id"
    pub project_claim: String,   // default: "project"
    pub workload_claim: String,  // default: "workload" or "sub"
}
```

Implementation:
- **JWT validation**: verify signature against JWKS (RS256/ES256),
  check `exp`, `iss`, `aud` claims. Use `jsonwebtoken` crate.
- **JWKS cache**: fetch JWKS from IdP, cache with TTL (default 1h),
  refresh on unknown `kid`.
- **Claim extraction**: map JWT claims to `(OrgId, ProjectId, WorkloadId)`
  via `ClaimMapping`.
- **Integration point**: second-stage auth after mTLS (I-Auth2).
  mTLS establishes "belongs to this cluster", JWT establishes
  "authorized by this tenant's admin for this workload."
- **Fallback**: if tenant has no IdP configured, mTLS alone is
  sufficient (existing behavior, I-Auth2 is optional).
- **LDAP/AD note**: not consumed directly. Sites using LDAP/AD
  federate through their OIDC provider (Keycloak LDAP federation,
  Azure AD, etc.). Kiseki speaks OIDC only. Document in deployment
  guide.

Tests:
- Valid JWT accepted, claims extracted correctly
- Expired JWT rejected
- Wrong issuer rejected
- Unknown kid triggers JWKS refresh
- No IdP config → mTLS-only (pass-through)

**Effort**: 10-14 sessions (3-4 per provider + 2-3 for OIDC)

---

## Phase 4: NFS Kerberos (WS 5.3)

File: `kiseki-gateway/src/nfs_auth.rs`

### 4.1 RPCSEC_GSS integration

- ONC RPC `AUTH_GSS` message parsing
- GSSAPI context establishment (via system libgssapi)
- Kerberos principal → tenant `OrgId` mapping
- Per-export access control

### 4.2 NFSv4 ACLs

- Beyond POSIX mode bits: NFSv4 ACL model
- ACL enforcement in `nfs4_server.rs`

**Effort**: 3-5 sessions

---

## Phase 5: Federation (WS 6.4)

File: `kiseki-control/src/federation.rs` (extend)

### 5.1 Peer-to-peer config sync

- Async replication of tenant metadata + namespaces
- Conflict resolution: last-writer-wins with HLC timestamps
- Per-federation-peer gRPC channel

### 5.2 Async delta replication

- Cross-site delta stream (not Raft — async)
- Ciphertext-only: no key material in replication stream
- Queue-and-retry on link failure (F-N1)

### 5.3 Data residency enforcement

- Control plane validates data residency before cross-site placement
- Compliance tags checked at replication boundary

**Effort**: 5-8 sessions

---

## Phase 6: Chaos Testing (WS 6.5)

File: `tests/chaos/` (new directory)

### 6.1 Fault injection framework

- Network partition simulation (iptables or transport-level drop)
- Slow disk injection (I/O delay on DeviceBackend)
- Clock skew injection (HLC offset)
- Process kill / restart

### 6.2 Linearizability verification

- Jepsen-style checker: concurrent writes + reads, verify linearizable
- Elle-based history recording

### 6.3 Stress scenarios

- Shard split under load
- Node failure during rebalance
- Multi-tenant contention
- Clock skew beyond HLC tolerance (F-C3)
- Crypto-shred during active reads

**Effort**: 5-8 sessions

---

## Phase Dependency Graph

```
Phase 1 (consensus) ────────────────────────┐
    │                                       │
Phase 2 (admin + gateway) ──────────────────┤
    │                                       │
Phase 3 (external KMS) ─── independent ─────┤
    │                                       │
Phase 4 (NFS Kerberos) ─── independent ─────┤
    │                                       │
Phase 5 (federation) ─── needs 2.3 ─────────┤
    │                                       │
Phase 6 (chaos) ─── needs all above ────────┘
```

Phases 1-4 parallelizable. Phase 5 depends on control plane (2.3).
Phase 6 is the capstone — tests everything.

## Estimated Total Effort

| Phase | Sessions | Notes |
|-------|----------|-------|
| 1: Consensus hardening | 8-12 | Membership, clock skew, shard split |
| 2: Admin + gateway | 10-15 | CLI, S3 CRUD, threshold, migration |
| 3: External KMS + OIDC | 10-14 | Vault + AWS + OIDC/JWT + validation |
| 4: NFS Kerberos | 3-5 | RPCSEC_GSS + ACLs |
| 5: Federation | 5-8 | Async replication + residency |
| 6: Chaos testing | 5-8 | Fault injection + Jepsen-style |
| **Total** | **41-62** | |

## Post-implementation

After all phases complete:

1. **Adversarial pass**: full codebase adversarial sweep on all code
   written today that didn't go through the diamond loop. Focus on:
   - Transport layer (Phases A-G from fabric subplan)
   - Observability (structured logging, metrics, OTel)
   - Client cache engine (ADR-031 implementation)
   - Storage engine (journal, TRIM, EC striping, scrub)
   - All new modules across 8+ crates

2. **Integrator pass**: cross-cutting integration verification.
   - All crate boundaries correct (no forbidden deps)
   - Proto coverage (every .proto in build.rs)
   - Architecture check (ADR-027 boundary)
   - BDD scenario coverage (target: all scenarios green)
   - E2E with docker-compose (Jaeger, multi-node, S3+NFS)
   - Spec consistency (invariants, ubiquitous language, failure modes)
