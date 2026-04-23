# Encryption Model

Kiseki uses a two-layer encryption architecture (ADR-002, model C) that
separates data encryption from access control. One encryption pass
protects data; key wrapping controls who can read it.

---

## Two-layer architecture

```
┌─────────────────────────────────────────────────┐
│              Tenant Layer (access)              │
│                                                 │
│  Tenant KEK (controlled by tenant admin)        │
│  wraps the system DEK for tenant-scoped access  │
│                                                 │
│  Destroying the tenant KEK = crypto-shred       │
│  (all tenant data rendered unreadable)           │
├─────────────────────────────────────────────────┤
│              System Layer (data)                │
│                                                 │
│  System DEK encrypts chunk data (AES-256-GCM)   │
│  System KEK wraps system DEKs                    │
│  Always on -- no unencrypted chunks              │
└─────────────────────────────────────────────────┘
```

**System layer**: The system DEK encrypts every chunk using AES-256-GCM.
System DEKs are derived per-chunk using HKDF-SHA256 from a master key
(ADR-003). The system KEK wraps system DEKs and is managed by the cluster
admin via the system key manager.

**Tenant layer**: The tenant KEK wraps the system DEK for tenant-scoped
access control. There is no double encryption -- one data encryption pass,
with key wrapping for access control. The tenant admin controls the tenant
KEK via the tenant KMS.

---

## Envelope structure

Each chunk is stored as an envelope containing:

```
┌──────────────────────────────────────────┐
│  Envelope                                │
│                                          │
│  ┌──────────────────────────────────┐    │
│  │  Ciphertext (AES-256-GCM)       │    │
│  │  (encrypted chunk data)          │    │
│  └──────────────────────────────────┘    │
│                                          │
│  auth_tag (16 bytes, GCM tag)            │
│  nonce (12 bytes, unique per chunk)      │
│  system_key_epoch (current epoch)        │
│  tenant_key_epoch (current epoch)        │
│  chunk_id (content-addressed)            │
│  algorithm_id (for crypto-agility)       │
│                                          │
│  System wrapping metadata                │
│  Tenant wrapping metadata                │
└──────────────────────────────────────────┘
```

The envelope carries algorithm identifiers for crypto-agility (I-K7).
All metadata is authenticated -- unauthenticated encryption is never
acceptable.

---

## Key derivation

System DEKs are derived locally on each storage node using HKDF-SHA256
(ADR-003). No DEK-per-chunk RPC is required:

```
system_dek = HKDF-SHA256(
    ikm  = master_key[epoch],
    salt = chunk_id,
    info = "kiseki-chunk-dek-v1"
)
```

The master key is fetched from the system key manager at startup and on
rotation events. DEK derivation is deterministic -- the same chunk ID
and epoch always produce the same DEK.

---

## Key rotation

Key rotation is epoch-based (I-K6):

1. The admin triggers rotation (system or tenant level)
2. A new epoch is created with fresh key material
3. New data is encrypted with the current epoch's keys
4. Old data retains its epoch until background re-encryption migrates it
5. Two epochs coexist during the rotation window
6. Full re-encryption available as an explicit admin action for
   key-compromise incidents

---

## Crypto-shred

Destroying the tenant KEK renders all tenant data unreadable (I-K5):

```
1. Set retention hold (if compliance requires)
2. Destroy tenant KEK at tenant KMS
3. All wrapped system DEKs for this tenant become unwrappable
4. Chunk ciphertext remains on disk (system-encrypted) until GC
5. Physical GC runs separately when refcount = 0 AND no retention hold
```

The ordering contract (I-C2b): set hold before crypto-shred to prevent
race with GC.

Client-side detection: periodic key health check (default 30s) detects
`KEK_DESTROYED` and triggers immediate cache wipe (I-CC12). Maximum
detection latency: `min(key_health_interval, max_disconnect_seconds)`.

---

## Chunk ID derivation

| Mode | Algorithm | Cross-tenant dedup |
|---|---|---|
| Default | `SHA-256(plaintext)` | Yes |
| Opted-out | `HMAC-SHA256(plaintext, tenant_key)` | No (zero co-occurrence leak) |

When a tenant opts out of cross-tenant dedup (I-X2, I-K10), chunk IDs
are derived using HMAC with a tenant-specific key, making it impossible
to determine whether two tenants store the same data.

---

## Tenant KMS providers (ADR-028)

Five pluggable backends implement the `TenantKmsProvider` trait:

| Provider | Key model | Key location |
|---|---|---|
| Kiseki-Internal | Raft-replicated | On-cluster |
| HashiCorp Vault | Transit secrets engine | External |
| KMIP 2.1 | Standard key management protocol | External |
| AWS KMS | Cloud-managed keys | External |
| PKCS#11 | Hardware security modules | External |

Provider selection is per-tenant at onboarding. The trait fully
encapsulates protocol differences -- callers never branch on provider type
(I-K16). Wrap/unwrap operations include AAD (chunk_id) binding to prevent
envelope splicing (I-K17).

---

## FIPS compliance

Kiseki uses `aws-lc-rs` with the FIPS feature flag for FIPS 140-2/3
validated cryptographic operations. The `kiseki-crypto` crate provides:

- AES-256-GCM authenticated encryption
- HKDF-SHA256 key derivation
- SHA-256 hashing
- HMAC-SHA256 for opted-out chunk ID derivation
- `zeroize` integration for all key material in memory

---

## Delta encryption

Log delta payloads (filenames, attributes, inline data) are encrypted
with the system DEK, wrapped with the tenant KEK (I-K3). The delta
envelope has structurally separated:

- **System-visible header** (cleartext or system-encrypted): sequence
  number, shard ID, hashed_key, operation type, timestamp
- **Tenant-encrypted payload**: the actual mutation data

Compaction operates on headers only and never decrypts tenant-encrypted
payloads (I-O2).
