# Key Management

Kiseki uses a two-layer encryption model where system-level encryption
protects data at rest and tenant-level key wrapping controls access.
This page covers operational aspects of key management: rotation,
re-encryption, crypto-shred, and external KMS integration.

---

## Encryption model

Kiseki implements Model (C) from ADR-002: single data encryption pass
at the system layer, with tenant access via key wrapping. No double
encryption.

```
Plaintext chunk
  |
  v
System DEK (AES-256-GCM)  -->  Ciphertext (stored on disk)
  |
  v
System KEK (wraps DEK derivation material)
  |
  v
Tenant KEK (wraps system DEK derivation parameters per tenant)
```

### System keys

- **System DEK**: Per-chunk symmetric key derived locally on each
  storage node via HKDF-SHA256 (ADR-003). Never stored, never
  transmitted. Derivation: `HKDF(master_key[epoch], chunk_id,
  "kiseki-chunk-dek-v1")`.
- **System master key**: Per-epoch master key stored in the system key
  manager (kiseki-keyserver). Storage nodes fetch it at startup and on
  epoch rotation, then derive per-chunk DEKs locally. The key manager
  never sees individual chunk IDs.
- **System KEK**: Wraps system master keys. Managed by the cluster
  admin.

### Tenant keys

- **Tenant KEK**: Key wrapping key managed by the tenant's chosen KMS
  backend. Wraps access to system DEK derivation parameters (epoch +
  chunk_id). Destroying the tenant KEK = crypto-shred (data becomes
  unreadable).
- **No Tenant DEK**: Model (C) does not double-encrypt. The tenant
  layer is key-wrapping, not data-encryption.

### Invariants

- I-K1: No plaintext chunk is ever persisted to storage.
- I-K2: No plaintext payload is ever sent on the wire.
- I-K7: Authenticated encryption (AES-256-GCM) everywhere.
- I-K8: Keys are never logged, printed, transmitted in the clear, or
  stored in configuration files.

---

## System key manager

The system key manager (`kiseki-keyserver`) is a dedicated HA service
backed by its own Raft consensus group.

### Deployment

Deploy on 3-5 dedicated nodes, separate from storage nodes. The system
key manager must be at least as available as the log (I-K12) because
its unavailability blocks all chunk writes cluster-wide.

### Key distribution

```
kiseki-keyserver:
  Stores: master_key per epoch (Raft-replicated)
  Serves: master_key to authenticated kiseki-server processes (mTLS)
  Never sees: individual chunk_ids or per-chunk operations

kiseki-server:
  Caches: master_key (mlock'd, MADV_DONTDUMP, seccomp)
  Derives: per-chunk DEK = HKDF(master_key, chunk_id) -- locally
  Never sends: chunk_ids to the key manager
```

This design prevents the key manager from building an index of all
chunk IDs, which would leak per-tenant access patterns.

---

## Key rotation

### System key rotation

System key rotation creates a new epoch with a new master key. The
rotation process:

1. Cluster admin initiates rotation via `RotateSystemKey()`.
2. The key manager generates a new master key and assigns a new epoch.
3. Storage nodes are notified and fetch the new master key.
4. New writes use the new epoch. Old data retains its epoch.
5. Two epochs coexist during the rotation window (I-K6).

Old master keys are retained until all data encrypted under them has
been re-encrypted or deleted. Full re-encryption is available as an
explicit admin action.

### Tenant key rotation

Tenant key rotation creates a new epoch for the tenant's KEK:

1. Tenant admin initiates rotation via `RotateTenantKey(tenant)`.
2. The tenant KMS generates or rotates the key (provider-specific).
3. New envelope wrappings use the new epoch.
4. Old wrapped material remains valid until background re-wrapping
   completes.

### Background re-encryption

A background monitor detects envelopes wrapped under old epochs and
schedules re-wrapping. The rewrap worker:

1. Reads envelopes with old-epoch tenant wrapping.
2. Unwraps with old KEK.
3. Re-wraps with current KEK.
4. Writes the updated envelope.

For providers that support server-side rewrap (e.g., Vault Transit),
the rewrap operation never exposes plaintext derivation material to the
storage node.

---

## Crypto-shred

Crypto-shred is the authoritative deletion mechanism in Kiseki.
Destroying the tenant KEK renders all tenant data unreadable.

### Process

1. Tenant admin initiates via `CryptoShred(tenant)`.
2. The tenant KMS destroys the KEK (provider-specific: Vault key
   deletion, AWS KMS key scheduling, PKCS#11 key destruction).
3. All cached key material for the tenant is invalidated across the
   cluster.
4. Native clients detect the shred via key health checks (default every
   30 seconds) and wipe their caches (I-CC12).

### What happens after crypto-shred

- **Data is semantically deleted**: No component can decrypt the tenant's
  data because the KEK is destroyed.
- **Ciphertext remains on disk**: Physical GC runs separately when chunk
  refcount = 0 AND no retention hold is active (I-C2b).
- **Audit trail preserved**: Crypto-shred events are recorded in the
  audit log.

### Ordering requirement

If retention holds are needed, they must be set before crypto-shred:

```
Set retention hold -> Crypto-shred -> Hold expires -> GC eligible
```

This prevents a race between crypto-shred and GC (I-C2b).

### Detection latency

Crypto-shred detection is bounded by:
`min(key_health_interval, max_disconnect_seconds)`.

Default key health check interval: 30 seconds. Configurable per tenant
within [5s, 300s], default 60s (I-K15).

---

## External KMS providers (ADR-028)

Kiseki supports five tenant KMS backends via the `TenantKmsProvider`
trait. The provider is selected per-tenant at onboarding.

### Provider comparison

| # | Backend | Transport | Material model | Key material location |
|---|---------|-----------|----------------|----------------------|
| 1 | Kiseki Internal | In-process | Local | Separate Raft group in Kiseki |
| 2 | HashiCorp Vault | HTTPS | Local (cached) | Vault Transit engine |
| 3 | KMIP 2.1 | mTLS (TTLV) | Remote or local | KMIP server / HSM |
| 4 | AWS KMS | HTTPS | Remote only | AWS KMS |
| 5 | PKCS#11 v3.0 | Local (FFI) | Remote only (HSM) | Hardware Security Module |

### Provider invariants

- I-K16: Provider abstraction is opaque to callers. No correctness
  decision depends on which backend is selected.
- I-K17: Wrap/unwrap operations include AAD (chunk_id) binding. A
  wrapped blob cannot be spliced from one envelope to another.
- I-K18: Provider is validated on configuration: connectivity test,
  wrap/unwrap round-trip, certificate chain. Validation failure prevents
  tenant activation.
- I-K19: Internal provider stores tenant KEKs in a separate Raft group
  from system master keys.
- I-K20: Provider migration (e.g., Internal to Vault) requires
  re-wrapping all existing envelopes. Migration is background, audited,
  and preserves data availability throughout.

### Provider 1: Kiseki Internal (default)

Zero-configuration default. Kiseki manages tenant KEKs internally in a
Raft group separate from system master keys. Suitable for
single-operator deployments.

**Security trade-off**: Internal mode does not provide the full
two-layer security guarantee. Compromise of both the system key store
and the tenant key store yields full access. Compliance-sensitive
tenants should use an external provider.

### Provider 2: HashiCorp Vault

Uses Vault's Transit secrets engine for encryption-as-a-service:

| Kiseki operation | Vault API |
|-----------------|-----------|
| `wrap` | `POST /transit/encrypt/:name` (with `context` = AAD) |
| `unwrap` | `POST /transit/decrypt/:name` (with `context` = AAD) |
| `rotate` | `POST /transit/keys/:name/rotate` |
| `rewrap` | `POST /transit/rewrap/:name` (server-side, no plaintext exposure) |
| `destroy` | `DELETE /transit/keys/:name` |

### Provider 3: KMIP 2.1

Standards-based integration with enterprise KMS and HSM appliances.
Uses mTLS over TTLV binary protocol.

### Provider 4: AWS KMS

Cloud-native KMS integration. Key material never leaves AWS. All
wrap/unwrap operations are remote HTTPS calls. Suitable for hybrid
cloud deployments.

### Provider 5: PKCS#11 v3.0

Direct HSM integration via the PKCS#11 C API (FFI). Key material
stays in the HSM. Highest security level, requires HSM hardware on
or accessible from storage nodes.

---

## OIDC integration

Tenant identity providers can be integrated for second-stage
authentication (I-Auth2). This is optional and orthogonal to the KMS
provider choice.

When configured, workload-level identity is validated against the
tenant admin's authorization via OIDC/JWT tokens, providing "authorized
by my tenant admin" on top of the mTLS-based "belongs to this cluster"
identity.

Keycloak is included in the development Docker Compose stack for OIDC
testing.

---

## Operational checklist

### Key rotation schedule

| Key type | Recommended interval | Enforcement |
|----------|---------------------|-------------|
| System master key | Quarterly | Manual (cluster admin) |
| Tenant KEK | Per tenant policy | Manual or automated via KMS |
| TLS certificates | Annual | Cluster CA renewal |

### Monitoring key health

```bash
# Check key manager health
grpcurl keyserver1:9400 kiseki.v1.KeyManagerService/KeyManagerHealth

# Check tenant KMS connectivity
grpcurl node1:9100 kiseki.v1.KeyManagerService/CheckKmsHealth

# Monitor key rotation metrics
curl -s http://node1:9090/metrics | grep kiseki_key_rotation_total

# Monitor crypto-shred events
curl -s http://node1:9090/metrics | grep kiseki_crypto_shred_total
```

### Key material security

- Master keys are mlock'd in memory on storage nodes (prevent swapping).
- Core dumps are disabled (`LimitCORE=0` in systemd, `MADV_DONTDUMP`).
- seccomp filters restrict system calls on key-handling threads.
- Runtime integrity monitor detects ptrace, `/proc/pid/mem` access, and
  debugger attachment (I-O7).
- Keys are zeroized on deallocation (`Zeroizing<Vec<u8>>`).
