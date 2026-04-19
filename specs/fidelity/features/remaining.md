# Fidelity: Remaining Crates (Summary)

## kiseki-keymanager (key-management.feature, manager surface)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 5/17 | THOROUGH | initial epoch, fetch, rotation, multi-epoch, unavailable |
| 12/17 | NONE | KMS connectivity, crypto-shred lifecycle, audit events, federation |

**Confidence: MEDIUM** — epoch management is well-tested; orchestration deferred.

## kiseki-transport (authentication.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 3/16 | THOROUGH | mTLS handshake, wrong CA rejected, config validation |
| 13/16 | NONE | cert revocation, SPIFFE, gateway auth, advisory re-validation |

**Confidence: LOW** — mTLS works but OrgId extraction is a placeholder, no revocation.

## kiseki-chunk (chunk-storage.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 6/25 | THOROUGH | write/read, dedup, GC, refcount, retention hold, underflow |
| 19/25 | NONE | EC, repair, placement, integrity check, pool management |

**Confidence: LOW** — core dedup/GC/hold semantics work, but no EC or placement.

## kiseki-audit (operational.feature, audit subset)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 5/33 | MODERATE | append, per-tenant sharding, query filter, export, empty |
| 28/33 | NONE | safety valve, runtime integrity, clock drift, federation |

**Confidence: LOW** — append-only semantics work, but operational scenarios are broad.

## kiseki-composition (composition.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 7/21 | THOROUGH | create, delete, rename, EXDEV, read-only, multipart, versioning |
| 14/21 | NONE | refcount integration, abort cleanup, listing, directory ops |

**Confidence: MEDIUM** — CRUD + multipart + EXDEV are solid. No chunk refcount integration.

## kiseki-view (view-materialization.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 7/23 | THOROUGH | create, watermark, discard, MVCC pin, expiry, release, staleness |
| 16/23 | NONE | stream processor, rebuild, multi-shard, advisory integration |

**Confidence: LOW** — view lifecycle is correct but no actual materialization (stream processor).

## kiseki-gateway (protocol-gateway.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 0/23 | — | No tests, trait only |

**Confidence: LOW** — trait boundary exists but zero behavioral verification.

## kiseki-client (native-client.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 4/26 | MODERATE | cache hit/miss, invalidation, eviction, TTL expiry |
| 22/26 | NONE | FUSE, discovery, transport, encryption, prefetch |

**Confidence: LOW** — cache works, everything else is a stub.

## kiseki-advisory (workflow-advisory.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 7/51 | THOROUGH | budget enforcement, workflow declare, phase monotonicity, lookup |
| 44/51 | NONE | gRPC service, telemetry, k-anonymity, covert-channel hardening |

**Confidence: LOW** — domain logic tested, but advisory runtime infrastructure absent.

## Go control plane (control-plane.feature)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 14/32 | MODERATE | tenant CRUD, quota validation, compliance tags, IAM access, policy staleness, advisory budget |
| 18/32 | NONE | gRPC server, namespace/shard creation, flavor matching, federation, discovery |

**Confidence: MEDIUM** — domain types and validation tested, no server or gRPC.

## kiseki-common + kiseki-proto (no feature file)

| Covered | Depth | Scenarios |
|---------|-------|-----------|
| 19/— | THOROUGH | HLC properties, boundary tests, proto roundtrip |

**Confidence: HIGH** — foundational types thoroughly tested with property tests.
