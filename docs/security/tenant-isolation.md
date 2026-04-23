# Tenant Isolation

Tenant isolation is a foundational invariant of Kiseki. Tenants are
fully isolated with no cross-tenant data access, no delegation tokens,
and no cross-tenant key sharing (I-T1).

---

## Isolation model

Kiseki implements hierarchical tenancy with strict isolation boundaries:

```
Organization (billing, admin, master key authority)
  |
  +-- Project (optional: resource grouping, key delegation)
  |     |
  |     +-- Workload (runtime isolation unit)
  |     +-- Workload
  |
  +-- Workload (directly under org, if no projects)
```

### Isolation guarantees

| Property | Guarantee | Invariant |
|----------|-----------|-----------|
| Data access | No cross-tenant data access | I-T1 |
| Key material | Per-tenant encryption keys, never shared | I-T3, I-K3 |
| Resource consumption | Bounded by quotas at org and workload levels | I-T2 |
| Audit visibility | Tenant sees only their own events | I-A2 |
| Metrics | Tenant-anonymous for cluster admin | ADR-015 |
| Admin access | Zero-trust: cluster admin cannot access tenant data without approval | I-T4 |

---

## Per-tenant encryption keys

Each tenant has their own KEK (Key Encryption Key) managed by their
chosen KMS backend (ADR-028). The tenant KEK wraps access to system DEK
derivation parameters for that tenant's data.

### Key isolation

- **System DEKs** are derived per-chunk and are the same for identical
  chunks across tenants (enabling cross-tenant dedup by default).
- **Tenant KEKs** are unique per tenant. Even if two tenants store the
  same data, each tenant wraps access to the DEK derivation parameters
  independently.
- Tenant keys are not accessible to other tenants or to shared system
  processes (I-T3).

### Key storage isolation

When using the internal KMS provider (default), tenant KEKs are stored
in a separate Raft group from system master keys (I-K19). Compromise
of one group does not expose the other.

When using external KMS providers (Vault, KMIP, AWS KMS, PKCS#11),
tenant key material is managed entirely outside of Kiseki's storage,
under the tenant's own operational control.

---

## HMAC-keyed chunk IDs for opted-out tenants

By default, chunk IDs are derived from plaintext content:
`chunk_id = SHA-256(plaintext)`. This enables cross-tenant
deduplication: identical data stored by different tenants produces the
same chunk ID and shares storage.

Tenants that require stronger isolation can opt out of cross-tenant
dedup (I-X2, I-K10):

```
Default:     chunk_id = SHA-256(plaintext)
Opted-out:   chunk_id = HMAC-SHA256(plaintext, tenant_key)
```

### What opt-out provides

- **No cross-tenant dedup**: Identical data from different tenants
  produces different chunk IDs. Each tenant's data is stored
  independently.
- **Zero co-occurrence leak**: An observer cannot determine whether two
  tenants store the same data by comparing chunk IDs.
- **Storage overhead**: Duplicate data across tenants consumes
  additional storage.

### When to opt out

Opt-out is recommended for tenants with:

- Regulatory requirements prohibiting any form of cross-tenant data
  correlation (even at the metadata level).
- High-sensitivity data where the existence of shared content is itself
  sensitive information.
- Compliance regimes (HIPAA, ITAR) where data co-location with other
  tenants must be minimized.

---

## Audit log scoping

The audit log is append-only, immutable, and system-wide (I-A1). Audit
visibility is strictly scoped:

### Tenant audit export (I-A2)

Each tenant receives a filtered projection of the audit log:

- All events originating from the tenant's own operations.
- Relevant system events sufficient for a coherent, complete audit
  trail (e.g., a cluster admin modifying a pool that contains the
  tenant's data).
- Delivered on the tenant's VLAN.
- Sufficient for independent compliance demonstration (e.g., HIPAA
  Section 164.312 audit controls).

The tenant admin consumes this export. No events from other tenants
appear in the export.

### Cluster admin audit view (I-A3)

The cluster admin sees:

- System-level events (node joins, pool changes, key rotations).
- Tenant-anonymous or aggregated metrics.
- No tenant-attributable content.

Cluster admin modifications to pools containing tenant data are
audit-logged to the affected tenant's audit shard (I-T4c), so the
tenant can review.

### Advisory audit scoping (I-WA8)

Workflow advisory events (declare, end, phase-advance, hint
accept/reject, etc.) are written to the tenant's audit shard.

- Semantic phase tags and workflow IDs are tenant-scoped.
- Cluster-admin views see opaque hashes only (consistent with I-A3).
- High-volume events (hint-accepted, hint-throttled) may be batched or
  sampled, but at least one event per unique (workflow_id,
  rejection_reason) tuple is written per second.

---

## Cache isolation (ADR-031)

The client-side cache maintains strict per-tenant isolation:

### L1 (in-memory) isolation

- The L1 cache operates within a single client process.
- A client process is authenticated as a specific tenant via mTLS.
- L1 entries are decrypted plaintext chunks, keyed by chunk ID.
- On process termination, L1 entries are zeroized (I-CC2).

### L2 (on-disk) isolation

- Each client process creates its own L2 cache pool on local NVMe.
- Pool isolation is enforced by:
  - **Unique pool ID**: 128-bit CSPRNG value per process.
  - **flock**: Ownership proven by file lock on `pool.lock`.
  - **Per-process directory**: No cross-process sharing.
- Concurrent same-tenant processes have independent pools. There is no
  cross-process cache sharing.
- Orphaned pools (no live flock holder) are scavenged on startup or by
  `kiseki-cache-scrub`.
- On eviction or cache wipe, L2 entries are overwritten with zeros
  before unlink (I-CC2).

### Crypto-shred cache wipe (I-CC12)

When a crypto-shred event is detected for a tenant:

1. All cached plaintext for that tenant is wiped from L1 and L2.
2. L1 entries: `Zeroizing<Vec<u8>>` ensures memory-level erasure.
3. L2 entries: File contents overwritten with zeros before unlink.
4. Detection mechanisms:
   - Periodic key health check (default 30 seconds).
   - Advisory channel notification.
   - KMS error on next operation.

Maximum detection latency: `min(key_health_interval,
max_disconnect_seconds)`.

### Physical-level erasure note

Logical-level erasure (zeroize before deallocation) provides strong
protection against software-level attacks. For protection against
physical-level attacks on flash storage (e.g., reading NAND cells after
logical deletion), hardware encryption (OPAL/SED) on the compute node's
local NVMe is required. This is outside Kiseki's control but should be
part of the compute node security policy.

---

## Network isolation

### Data fabric

All data-fabric traffic is mTLS-encrypted. Tenant identity is extracted
from the client certificate and validated on every RPC.

### Management network

The management network (control plane, admin API) is separate from the
data fabric. Cluster admin access requires admin-OU certificates.

### Tenant VLAN

Tenant audit exports are delivered on the tenant's VLAN, providing
network-level isolation of audit data.

---

## Advisory isolation (I-WA1, I-WA2, I-WA5, I-WA6)

The workflow advisory subsystem enforces strict tenant isolation:

- **No existence oracles** (I-WA6): A client cannot determine the
  existence of resources it is not authorized to observe. Unauthorized
  and absent targets return identical responses (same error code,
  payload size, and latency distribution).
- **No content oracles** (I-WA11): Advisory fields never include
  cluster-internal identifiers (shard IDs, chunk IDs, node IDs, device
  IDs, rack labels).
- **Telemetry scoping** (I-WA5): Every telemetry value is computed
  over resources the caller is authorized to read. Aggregate metrics
  use k-anonymous bucketing (minimum k=5).
- **Covert-channel hardening** (I-WA15): Response timing and size do
  not vary with neighbor-workload state.

### Pool handle isolation (I-WA19)

Affinity pools are referenced via opaque pool handles, not
cluster-internal pool IDs:

- Handles are valid for one workflow's lifetime only.
- Never reused across workflows.
- Never equal or leak the cluster-internal pool identity.
- Multiple tenants can see the same opaque label attached to different
  internal pools; correlation across tenants is impossible because
  handles differ.

---

## Compliance support

Kiseki's tenant isolation model supports the following compliance
regimes:

| Regime | Relevant guarantees |
|--------|-------------------|
| HIPAA | Per-tenant encryption, audit export for Section 164.312, crypto-shred, bounded staleness (2s floor). |
| SOC 2 | Audit log immutability, access control separation, key management lifecycle. |
| GDPR | Crypto-shred as right-to-erasure mechanism, data isolation by design. |
| ITAR | HMAC-keyed chunk IDs (no cross-tenant correlation), dedicated tenant KMS. |

Compliance tags attach at any level of the tenant hierarchy
(organization, project, workload) and inherit downward. Tags may impose
additional constraints:

- Prohibit compression (HIPAA namespaces, I-K14).
- Set staleness floor (minimum 2 seconds for HIPAA).
- Require external KMS provider (no internal mode).
- Restrict pool placement (data residency).
