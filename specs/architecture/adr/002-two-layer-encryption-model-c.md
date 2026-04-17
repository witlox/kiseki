# ADR-002: Two-Layer Encryption Model (C)

**Status**: Accepted
**Date**: 2026-04-17
**Context**: Q-K-arch1, I-K1 through I-K14

## Decision

Single data encryption pass at the system layer. Tenant access via key wrapping.
No double encryption.

- System DEK encrypts chunk data (AES-256-GCM via FIPS module)
- Tenant KEK wraps access to system DEK derivation material
- System key manager derives per-chunk DEKs via HKDF (see ADR-003)

## Rationale

- Single encryption pass at HPC line rates (200+ Gbps per NIC)
- Double encryption doubles CPU cost for no additional security benefit
  given that both layers use authenticated encryption
- Key wrapping is O(32 bytes) per operation vs O(data_size) for encryption
- Cross-tenant dedup works: same plaintext → same chunk_id → one ciphertext,
  multiple tenant KEK wrappings

## Consequences

- Crypto-shred destroys tenant KEK → data unreadable but not physically deleted
- System key compromise exposes system-layer ciphertext; combined with tenant
  KEK = full access. System key manager must be highly protected (ADR-007).
- Envelope must carry both system and tenant wrapping metadata
