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
