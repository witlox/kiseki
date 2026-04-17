# ADR-003: System DEK Derivation (Not Storage)

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-3 (system DEK count at scale), escalation point 3

## Decision

System DEKs are **derived at runtime** via HKDF, not stored individually.

```
system_dek = HKDF-SHA256(
    key = system_master_key[epoch],
    salt = chunk_id,
    info = "kiseki-chunk-dek-v1"
)
```

## Rationale

- At petabyte scale with ~1MB average chunks: billions of chunks
- Storing billions of 32-byte DEKs = tens of GB in the key manager
- HKDF derivation is deterministic: same (master_key, chunk_id) → same DEK
- The key manager stores only master keys (one per epoch) — trivial storage
- HKDF is fast (~microseconds) and FIPS-approved
- Key rotation: new epoch = new master key. Old master keys retained during
  migration window. Derivation still works for old-epoch chunks.

## Consequences

- System key manager is simpler (stores epochs, not individual DEKs)
- Master key compromise exposes ALL system DEKs for that epoch (same as
  storing them, but with less attack surface since there's one key to protect)
- Chunk ID is used as salt — chunk ID must not change after creation
- Tenant KEK wraps the HKDF derivation parameters (epoch + chunk_id),
  not the DEK itself — unwrapping + HKDF derives the DEK
