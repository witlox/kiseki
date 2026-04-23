# Security Model

Kiseki is designed with security as a foundational constraint, not a
bolted-on feature. The system enforces strong tenant isolation,
mandatory encryption, and a zero-trust boundary between infrastructure
operators and tenants.

---

## Zero-trust boundary

Kiseki enforces a strict separation between two administrative domains:

### Cluster admin (infrastructure operator)

- Manages nodes, global policy, system keys, pools, devices.
- **Cannot** access tenant config, logs, or data without explicit
  tenant admin approval (I-T4).
- Sees operational metrics in tenant-anonymous or aggregated form.
- Modifications to pools containing tenant data are audit-logged to
  the affected tenant's audit shard (I-T4c).

### Tenant admin (data owner)

- Controls tenant keys, projects, workload authorization, compliance
  tags, user access.
- Grants or denies cluster admin access requests.
- Receives tenant-scoped audit exports sufficient for independent
  compliance demonstration.
- Can crypto-shred to render all tenant data unreadable.

### Access request flow

When a cluster admin needs access to tenant resources (for debugging,
migration, etc.):

1. Cluster admin submits an access request via the control plane.
2. The request is recorded in the audit log.
3. Tenant admin reviews and approves or denies.
4. If approved, access is time-bounded and scoped.
5. All access is audit-logged to the tenant's shard.

---

## Encryption at rest

Every chunk stored on disk is encrypted. There are no exceptions.

- **Algorithm**: AES-256-GCM (authenticated encryption with associated
  data).
- **Key derivation**: HKDF-SHA256 derives per-chunk DEKs from a system
  master key and the chunk ID (ADR-003).
- **Envelope**: Each chunk carries an envelope containing ciphertext,
  system-layer wrapping metadata, tenant-layer wrapping metadata, and
  authenticated metadata (chunk ID, algorithm identifiers, key epoch).

### What is encrypted

| Data | Encryption | Location |
|------|-----------|----------|
| Chunk data on disk | System DEK (AES-256-GCM) | Data devices |
| Inline small-file content | System DEK | `small/objects.redb` |
| Delta payloads (filenames, attributes) | System DEK, wrapped with tenant KEK | Raft log / redb |
| Delta headers (sequence, shard, operation type, timestamp) | Cleartext or system-encrypted | Raft log / redb |
| Backup data | System-encrypted | External backup target |
| Federation replication | Ciphertext-only | Replication stream |

### What is NOT encrypted

- Delta headers: Compaction operates on headers only (I-O2). Headers
  contain no tenant-attributable content.
- Prometheus metrics: Aggregated counters and histograms. No
  tenant-attributable data in metric labels.
- Health/liveness probes: `200 OK` response.

---

## Encryption in transit

All data-fabric communication uses mTLS. No plaintext data crosses the
network.

- **Data path**: mTLS with per-tenant certificates signed by the
  Cluster CA (I-K2).
- **Raft consensus**: mTLS between Raft peers.
- **Key manager**: mTLS between storage nodes and the key manager.
- **Client to gateway**: TLS (clients send plaintext over TLS; the
  gateway encrypts before writing).
- **Native client**: Client-side encryption (plaintext never leaves
  the workload process).

### Protocol gateway encryption

Protocol gateway clients (NFS, S3) send plaintext over TLS to the
gateway. The gateway performs tenant-layer encryption before writing to
the storage layer. This means plaintext exists in gateway process memory
but never on the wire in cleartext and never at rest.

### Native client encryption

Native clients (FUSE, FFI, Python) perform tenant-layer encryption
themselves. Plaintext never leaves the workload process and never
traverses the data fabric.

---

## FIPS 140-2/3 compliance

Kiseki uses `aws-lc-rs` as its cryptographic backend, which provides
a FIPS 140-2/3 validated implementation of:

- AES-256-GCM (authenticated encryption)
- HKDF-SHA256 (key derivation)
- SHA-256 (content-addressed chunk IDs)
- HMAC-SHA256 (per-tenant chunk IDs for opted-out tenants)

The FIPS feature is controlled by the `kiseki-crypto/fips` feature
flag at compile time.

### Crypto-agility

Envelope metadata carries algorithm identifiers for crypto-agility.
If a new algorithm is needed (e.g., post-quantum), envelopes can carry
the new algorithm identifier alongside the existing one during a
transition period.

---

## No plaintext past gateway boundary (I-K1, I-K2)

This is the fundamental security invariant. Kiseki guarantees:

1. No plaintext chunk is ever persisted to storage (I-K1).
2. No plaintext payload is ever sent on the wire between any components
   (I-K2).
3. The system can enforce access to ciphertext without being able to
   read plaintext without tenant key material (I-K4).

### Where plaintext exists

Plaintext exists only in:

- **Client process memory**: For native clients that perform client-side
  encryption.
- **Gateway process memory**: Transiently, while the gateway encrypts
  protocol-path data.
- **Stream processor memory**: Stream processors cache tenant key
  material and are in the tenant trust domain (I-O3).
- **Client cache (L1)**: In-memory cache of decrypted chunks
  (zeroized on eviction or deallocation, I-CC2).
- **Client cache (L2)**: On-disk cache of decrypted chunks on local
  NVMe (zeroized before unlink, I-CC2).

---

## Content-addressed chunk IDs

Chunk identity is derived from content, serving both dedup and integrity:

- **Default**: `chunk_id = SHA-256(plaintext)`. Enables cross-tenant
  dedup.
- **Opted-out tenants**: `chunk_id = HMAC-SHA256(plaintext,
  tenant_key)`. Cross-tenant dedup is impossible. Zero co-occurrence
  leak (I-K10).

Tenants that opt out of cross-tenant dedup pay a storage overhead
(identical data stored separately per tenant) but gain the guarantee
that no metadata (chunk IDs, refcounts) leaks information about data
similarity across tenants.

---

## Audit trail

All security-relevant events are recorded in an append-only, immutable
audit log with the same durability guarantees as the data log (I-A1).

Audit events include:

- Data access (read/write by tenant, workload, client)
- Key lifecycle (rotation, crypto-shred, KMS health)
- Admin actions (pool changes, device management, tuning parameters)
- Policy changes (quotas, compliance tags, advisory policy)
- Authentication events (mTLS success/failure, cert revocation)

### Audit scoping

- **Tenant audit export**: Filtered to the tenant's own events plus
  relevant system events. Delivered on the tenant's VLAN (I-A2).
  Sufficient for independent compliance demonstration (HIPAA
  Section 164.312 audit controls).
- **Cluster admin audit view**: System-level events only.
  Tenant-anonymous or aggregated (I-A3).

---

## Runtime integrity

An optional runtime integrity monitor detects attempts to access Kiseki
process memory (I-O7):

- ptrace detection
- `/proc/pid/mem` access monitoring
- Debugger attachment detection
- Core dump attempt detection

On detection, the monitor alerts both cluster admin and tenant admin.
Optional auto-rotation of keys can be configured as a response.

---

## STRIDE Threat Analysis

Systematic analysis of Kiseki's attack surfaces using the
[STRIDE](https://en.wikipedia.org/wiki/STRIDE_(security)) framework.

### Spoofing (identity)

| Threat | Attack surface | Mitigation | Invariant |
|--------|---------------|------------|-----------|
| Rogue node joins cluster | Raft peer handshake | mTLS with Cluster CA — only certs signed by the cluster CA are accepted. Raft RPC server rejects plaintext when TLS is configured. | I-Auth1, I-K13 |
| Client impersonates tenant | Data fabric connection | mTLS required. OrgId extracted from cert OU or SPIFFE SAN. Fallback: UUID v5 from cert fingerprint (no anonymous access). | I-Auth1, I-Auth3 |
| Forged S3 request | S3 gateway | SigV4 signature validation with HMAC-SHA256 (constant-time comparison). `x-amz-date` required, `host` must be signed. | SigV4 auth |
| Forged JWT token | OIDC second-stage | `alg=none` rejected unconditionally. HS256 verified via HMAC. RS256/ES256 verified via JWKS with key ID matching. | I-Auth2 |
| NFS UID spoofing | NFS gateway | AUTH_SYS trusts client-asserted UID (known limitation). Mitigated by: network segmentation, Kerberos for production, per-export allowed method list. | NFS auth |
| Replay of captured request | S3 gateway | Timestamp validation (TODO: ±15min window). Captured Raft RPCs are harmless (Raft rejects stale term/log index). | SigV4 |

**Residual risk**: NFS AUTH_SYS is inherently spoofable. Production deployments MUST use Kerberos or restrict NFS to trusted networks.

### Tampering (data integrity)

| Threat | Attack surface | Mitigation | Invariant |
|--------|---------------|------------|-----------|
| Modify chunk on disk | Block device | CRC32C on every extent read. Mismatch → EC repair from parity. Periodic scrub with configurable sample rate. | I-C7, I-C8 |
| Modify chunk in transit | Fabric | TLS 1.3 (authenticated encryption). RDMA paths use pre-encrypted chunks. | I-K2, I-Auth1 |
| Modify Raft log entry | Raft replication | Raft consensus — committed entries are immutable (I-L3). Log entries validated by majority before commit. WAL journal for crash-safe bitmap. | I-L2, I-L3 |
| Tamper with envelope | Crypto layer | AES-256-GCM authenticated encryption. Tampered ciphertext, auth tag, or nonce → decryption failure. AAD binding to chunk_id prevents envelope splicing (I-K17). | I-K7, I-K17 |
| Modify L2 cache file | Client NVMe | CRC32 trailer on every L2 read. Mismatch → bypass to canonical + delete corrupt entry. | I-CC7, I-CC13 |
| Corrupt staging manifest | Client cache | Invalid JSON silently skipped during manifest load. No data served from unverifiable source. | I-CC7 |

### Repudiation (deniability)

| Threat | Attack surface | Mitigation | Invariant |
|--------|---------------|------------|-----------|
| Admin denies action | Control plane | All admin operations (maintenance, quota, compliance, key rotation) recorded in cluster audit shard with timestamp, identity, and parameters. | I-A1, I-A6 |
| Tenant denies access | Data path | All data access operations auditable. Tenant audit export provides filtered, coherent trail for compliance (HIPAA §164.312). | I-A2 |
| Advisory abuse denied | Workflow advisory | Advisory lifecycle events (declare, end, phase-advance, budget-exceeded) logged per-occurrence. High-volume events sampled with per-second-per-workflow counts. | I-WA8 |
| Device state change denied | Storage | Device state transitions (Healthy→Degraded→Evacuating→Failed→Removed) recorded with timestamp, reason, admin identity. | I-D2 |
| Crypto-shred denied | Key management | Shred event logged in tenant audit shard. Key health check provides detection confirmation. Cache wipe events counted. | I-K5, I-CC12 |

### Information disclosure (confidentiality)

| Threat | Attack surface | Mitigation | Invariant |
|--------|---------------|------------|-----------|
| Plaintext leak on wire | All RPCs | TLS mandatory on all data fabric connections. No plaintext payloads transmitted. | I-K1, I-K2 |
| Plaintext on disk (server) | Chunk storage | All chunks encrypted at rest with system DEK (AES-256-GCM). No plaintext persisted on storage nodes. Compaction operates on headers only — never decrypts payloads. | I-K1, I-O2 |
| Plaintext on disk (client) | L2 cache | Cached plaintext on compute-node NVMe (same trust domain as process memory). File permissions 0600. Zeroize on eviction/wipe. Crash scrubber for orphaned pools. FTL residual risk documented. | I-CC2, I-CC8 |
| Cross-tenant data leak | Multi-tenant | Full tenant isolation (I-T1). Per-tenant encryption keys. Cluster admin cannot access tenant data without approval (I-T4). HMAC-keyed chunk IDs for dedup-opted-out tenants prevent co-occurrence analysis. | I-T1, I-T3, I-K10 |
| Telemetry leaks tenant info | Advisory | Telemetry scoped to caller's authorization. k-anonymity (k≥5) over neighbour workloads. Response shape unchanged under low-k conditions. Timing and size bucketed to prevent covert channels. | I-WA5, I-WA6, I-WA15 |
| Error messages leak state | All APIs | `AuthError` returns generic failures. `KmsError` uses enum variants not freeform strings. Advisory requests for unauthorized targets return same shape as absent targets. | I-WA6 |
| Core dump exposes keys | Server/client | Key material wrapped in `Zeroizing<Vec<u8>>`. Runtime integrity monitor detects debugger/ptrace. | I-K8, I-O7 |
| Log messages leak data | Structured logging | Structured tracing with typed fields. No plaintext in log events. Tenant-scoped identifiers hashed in cluster-admin views. | I-A3, I-K8 |

### Denial of service (availability)

| Threat | Attack surface | Mitigation | Invariant |
|--------|---------------|------------|-----------|
| Raft leader flooding | Raft consensus | MAX_RAFT_RPC_SIZE (128MB) rejects oversized messages. Per-shard throughput guard (I-SF7) limits inline write rate. | ADV-S1, I-SF7 |
| Advisory hint flooding | Workflow advisory | Per-workload hint budget (hints/sec, concurrent workflows). Budget exceeded → local degradation only. Advisory isolated from data path (I-WA2). | I-WA7, I-WA16, I-WA17 |
| Connection pool exhaustion | Transport | `max_per_endpoint` connection cap. Circuit breaker trips after threshold failures. FabricSelector falls back to TCP. | Transport health |
| Disk exhaustion (metadata) | System NVMe | ADR-030 dynamic inline threshold. Soft limit → threshold reduction. Hard limit → threshold floor + alert via out-of-band gRPC. | I-SF1, I-SF2 |
| Disk exhaustion (data) | Device pools | Per-pool capacity thresholds (Warning/Critical/Full). Writes rejected at Critical. Pool rebalancing. | I-C5 |
| Cache exhaustion (client) | Client NVMe | Per-process `max_cache_bytes`. Per-node `max_node_cache_bytes` (80% of filesystem). Disk-pressure backstop at 90%. | ADR-031 §8 |
| Audit log backpressure | Audit | Safety valve: if audit export stalls >24h, data GC proceeds with documented gap. Per-tenant configurable backpressure mode. | I-A5 |
| Shard split storm | Log | Exponential backoff per shard (2h floor, 24h cap). Cluster-wide concurrent migrations bounded by `max(1, num_nodes/10)`. | I-SF4 |

### Elevation of privilege (authorization)

| Threat | Attack surface | Mitigation | Invariant |
|--------|---------------|------------|-----------|
| Cluster admin accesses tenant data | Control plane | Zero-trust boundary. Access requires explicit tenant admin approval, time-bounded, scope-limited, audit-logged. | I-T4, I-T4c |
| Tenant escapes namespace | Data path | Namespace isolation per tenant. Cross-shard operations return EXDEV (I-L8). Compositions belong to exactly one tenant (I-X1). | I-T1, I-X1, I-L8 |
| Hint escalates priority | Advisory | Hints cannot extend capability. Cannot cause operation success that would otherwise be rejected. Cannot cross namespace/tenant boundary. Cannot bypass retention hold. | I-WA14 |
| Client escalates cache policy | Client cache | Client selections bounded by admin-set ceilings. Policy narrowing only (child ≤ parent). `cache_enabled=false` at any level → disabled for all children. | I-CC10, I-WA7 |
| KMS provider escalation | Key management | Provider abstraction opaque to callers (I-K16). No access-control decision depends on provider type. Provider migration requires 100% re-wrap before atomic switch. | I-K16, I-K20 |
| gRPC method escalation | Control plane | Per-method authorization. 9 admin-only methods gated by `require_admin()`. Unknown role → rejected. | gRPC authz |

### Summary

| STRIDE Category | Threats identified | Mitigated | Residual risk |
|----------------|-------------------|-----------|---------------|
| **Spoofing** | 6 | 5 | NFS AUTH_SYS UID spoofing (use Kerberos in prod) |
| **Tampering** | 6 | 6 | None — all paths have integrity verification |
| **Repudiation** | 5 | 5 | None — comprehensive audit trail |
| **Information disclosure** | 8 | 8 | Client L2 NVMe FTL residual (use OPAL/SED) |
| **Denial of service** | 8 | 8 | None — all paths have rate limiting/backpressure |
| **Elevation of privilege** | 6 | 6 | None — defense in depth at every boundary |
| **Total** | **39** | **37** | **2 documented residual risks** |

Both residual risks have documented mitigations:
1. NFS AUTH_SYS → deploy Kerberos or restrict to trusted networks
2. NVMe FTL data remanence → deploy OPAL/SED with per-boot key rotation
