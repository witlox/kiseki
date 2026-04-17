# ADR-003: System DEK Derivation (Not Storage)

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-3 (system DEK count at scale), escalation point 3

## Decision

System DEKs are **derived locally on storage nodes** via HKDF, not stored
individually and not derived via RPC to the key manager.

```
system_dek = HKDF-SHA256(
    key = system_master_key[epoch],
    salt = chunk_id,
    info = "kiseki-chunk-dek-v1"
)
```

### Key distribution model (revised per ADV-ARCH-01)

The system key manager (kiseki-keyserver) stores and replicates master keys.
Storage nodes (kiseki-server) fetch the current master key at startup and
on epoch rotation. DEK derivation happens **locally on the storage node** —
the key manager never sees individual chunk_ids.

```
kiseki-keyserver:
  Stores: master_key per epoch
  Serves: master_key to authenticated kiseki-server processes
  Never sees: individual chunk_ids or per-chunk operations

kiseki-server:
  Caches: master_key (mlock'd, refreshed on rotation)
  Derives: per-chunk DEK = HKDF(master_key, chunk_id) — locally
  Never sends: chunk_ids to the key manager
```

This prevents the key manager from building an index of all chunk_ids
(ADV-ARCH-01: HKDF leak), which would reconstruct the per-tenant
refcount data we explicitly decided not to store (ADR-017).

## Rationale

- At petabyte scale with ~1MB average chunks: billions of chunks
- Storing billions of 32-byte DEKs = tens of GB in the key manager
- HKDF derivation is deterministic: same (master_key, chunk_id) → same DEK
- The key manager stores only master keys (one per epoch) — trivial storage
- HKDF is fast (~microseconds) and FIPS-approved
- Local derivation eliminates per-chunk RPC to key manager (performance)
- Key rotation: new epoch = new master key. Old master keys retained during
  migration window. Derivation still works for old-epoch chunks.
- Key manager never sees chunk-level operations (ADV-ARCH-01 fix)

## Consequences

- System key manager is simpler (stores epochs, not individual DEKs)
- Master key is cached in kiseki-server memory — this is the highest-value
  target on a storage node (ADV-ARCH-04, accepted risk with mitigations:
  mlock, MADV_DONTDUMP, seccomp, core dumps disabled)
- Master key compromise exposes ALL system DEKs for that epoch
- Chunk ID is used as salt — chunk ID must not change after creation
- Tenant KEK wraps the HKDF derivation parameters (epoch + chunk_id),
  not the DEK itself — unwrapping + HKDF derives the DEK
