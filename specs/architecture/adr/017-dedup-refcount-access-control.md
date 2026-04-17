# ADR-017: Dedup Refcount Metadata Access Control

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-2 (cross-tenant dedup refcount metadata)

## Decision

Chunk refcount metadata stores **total refcount only**, without per-tenant
attribution. Tenant-to-chunk mapping is derived from composition metadata
(which is tenant-encrypted).

### Mechanism

```
ChunkMeta:
  chunk_id: abc123
  total_refcount: 3      ← visible to system
  per_tenant_refs: N/A   ← NOT stored

Tenant attribution is in the composition deltas:
  org-pharma/composition-X references chunk abc123   ← encrypted in delta payload
  org-biotech/composition-Y references chunk abc123  ← encrypted in delta payload
```

### Access control

- Cluster admin can see: chunk_id, total_refcount, pool, EC status
- Cluster admin CANNOT see: which tenants reference which chunks
  (this information is in tenant-encrypted delta payloads)
- System dedup process: compares chunk_ids (in the clear for dedup),
  but does not record which tenant triggered the dedup match

### Residual risk

- Total refcount > 1 reveals that SOME dedup occurred, but not who
- Timing side channel: a dedup hit is faster than a full write. An
  observer who can measure write latency precisely could infer dedup.
  Mitigation: add random delay to normalize write timing (optional,
  configurable per tenant).

## Consequences

- No per-tenant refcount tracking in chunk metadata
- Refcount decrement on crypto-shred: the crypto-shred process walks
  the tenant's compositions (decrypted with tenant key during shred)
  to identify which chunks to decrement
- This is slower than a per-tenant refcount lookup but only happens
  during crypto-shred (rare operation)
