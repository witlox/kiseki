# @unit BDD Scenario Coverage Audit — 2026-04-25

## Purpose

For each of the 401 `@unit` BDD scenarios, determine whether:
1. A crate-level unit test already covers the same behavior
2. An `@integration` BDD scenario proves the behavior end-to-end

Goal: move @unit scenarios out of BDD into crate tests, keeping only
@integration scenarios in the acceptance suite. No gaps allowed.

## Summary

| Category | Count | Action required |
|----------|-------|-----------------|
| COVERED | 159 | Remove from BDD — crate tests exist |
| PARTIAL | 59 | Fix crate test stubs, then remove from BDD |
| GAP-UNIT | 142 | Add crate unit test, then remove from BDD |
| GAP-INTEGRATION | 18 | Add @integration scenario, then remove from BDD |
| GAP-BOTH | 23 | Add both crate test + @integration scenario |

## Per-feature breakdown

### Fully covered (can remove now)

- **block-storage.feature**: 6/6 COVERED — bitmap alloc, alignment, full, free, coalesce, split
- **erasure-coding.feature**: 11/11 COVERED — EC 4+2, 8+3, small chunk, normal/degraded/fail read, deterministic placement, overhead ratios, replication-3
- **small-file-placement.feature**: 15/16 COVERED (1 PARTIAL: redb layout)

### Mostly covered (few gaps)

- **chunk-storage.feature**: 13/19 COVERED, 6 GAP-UNIT (HMAC write, 5 advisory hint scenarios — hint infrastructure unimplemented)
- **key-management.feature**: 10/11 COVERED, 1 GAP-UNIT (audit lifecycle events — audit infra TODO)
- **device-management.feature**: 4/7 COVERED, 3 GAP-UNIT (remove-without-evacuate guard, no-sibling ENOSPC, audit trail)

### Mixed coverage

- **nfs3-rfc1813.feature**: 9/18 COVERED, 9 GAP-UNIT (NULL, WRITE FILE_SYNC, WRITE bad handle, CREATE, FSINFO, FSSTAT, LOOKUP NOENT — protocol-level XDR tests missing)
- **nfs4-rfc7862.feature**: 11/27 COVERED, 16 GAP-UNIT (EXCHANGE_ID, CREATE_SESSION, SEQUENCE, PUTROOTFH, GETATTR, WRITE, DESTROY_SESSION, OPEN read/create/NOENT, CLOSE — compound-level tests missing)
- **s3-api.feature**: 6/14 COVERED, 8 GAP-UNIT (empty body PUT, GET 404, invalid UUID, HEAD, DELETE, unknown bucket, prefix filtering, pagination)
- **native-client.feature**: 18/35 COVERED, 5 PARTIAL, 10 GAP-UNIT (native API, client-side encrypt, read-only ns, crash semantics, staging handoff, disconnect trigger chain, crypto-shred trigger chain, per-node capacity, backpressure processing, advisory disabled fallback), 2 GAP-INTEGRATION
- **view-materialization.feature**: 9/15 COVERED, 4 PARTIAL, 2 GAP-UNIT (prefetch warm-up, readahead suppression — view-layer advisory not implemented)
- **external-kms.feature**: 14/23 COVERED, 9 GAP-UNIT (cache TTL jitter, circuit breaker, concurrency limiter, provider timeout, KmsAuthConfig Debug, credential rotation — infrastructure unimplemented)
- **operational.feature**: 4/25 COVERED, 18 PARTIAL, 3 GAP-UNIT (dev-mode integrity, audit backpressure throttle, writable mmap ENOTSUP)
- **storage-admin.feature**: 4/26 COVERED, 17 PARTIAL, 5 GAP-UNIT (SRE role, inline threshold no-retroactive, rebalance target, inline threshold API, pool durability change)

### Significant gaps

- **authentication.feature**: 4/16 COVERED, 12 GAP-UNIT (mTLS handshake, cert expiry, tenant mismatch, IdP, NFS/S3 gateway auth, admin data-fabric rejection — tcp_tls.rs has zero tests)
- **protocol-gateway.feature**: 9/19 COVERED, 10 GAP-INTEGRATION (multipart, NFSv4 locking, conditional write, workflow_ref header, priority scheduling, backpressure telemetry, io_advise mapping, NFS workflow model, KMS unreachable, QoS headroom)
- **control-plane.feature**: 5/31 COVERED, 18 GAP-UNIT (IAM approve/deny/expiry, quota adjustment, flavor matching, compliance tag removal, retention, maintenance, advisory policy 8 scenarios, cache policy 5 scenarios), 8 GAP-BOTH
- **composition.feature**: 1/20 COVERED (EXDEV only), 11 GAP-UNIT (refcounts, inline, multipart, versioning, dedup wiring), 8 GAP-BOTH (compliance tags, failure injection, advisory hints, telemetry)
- **log.feature**: 0/12 COVERED, 5 GAP-INTEGRATION (compaction, GC, maintenance), 7 GAP-BOTH (merge ratio, HLC tie-break, admin compact audit, stalled consumer alert, advisory compaction pacing, telemetry scoping, advisory disabled)
- **workflow-advisory.feature**: 6/50 COVERED, 14 PARTIAL, 29 GAP-UNIT (profile validation, TTL expiry, hint outcome equivalence, draining FSM, StreamWarning, mTLS re-validation, pool handles, telemetry channels, audit events, phase-ring eviction, deadline hints, covert-channel hardening), 1 GAP-INTEGRATION

## Structural root causes

1. **Audit infrastructure**: 20+ scenarios across all features have `// TODO: wire audit infrastructure`. No crate wires `kiseki-audit` for event verification.

2. **Advisory hint subsystem**: ~40 scenarios describe hint behavior (affinity, dedup-intent, locality-class, retention-intent, backpressure, prefetch) that has no implementation in the target crates. BDD steps simulate via `last_error` string injection.

3. **CompositionStore isolation**: `CompositionStore` holds chunk IDs in memory only, has no reference to `ChunkStore`. The entire class of refcount-at-boundary behaviors cannot be verified by any composition-layer test.

4. **Protocol-level XDR/wire tests missing**: NFS3/NFS4/S3 crate tests exercise the ops layer (`NfsContext`, `gateway_write`) but not the actual RPC encoding/decoding (`handle_nfs3_first_message`, `op_exchange_id`, XDR byte parsing).

5. **Control plane modules with zero #[test]**: `iam.rs`, `retention.rs`, `advisory_policy.rs`, `flavor.rs`, `maintenance.rs`, `namespace.rs` — all have logic exercised only through BDD acceptance steps.

## Recommended execution order

### Phase A: Remove 159 COVERED scenarios (no code changes needed)

Delete from feature files. Crate tests already prove the behavior.
Affected: block-storage (6), erasure-coding (11), small-file-placement (15),
chunk-storage (13), key-management (10), device-management (4),
nfs3 (9), nfs4 (11), s3 (6), native-client (18), view (9),
external-kms (14), authentication (4), protocol-gateway (9),
operational (4), storage-admin (4), control-plane (5),
workflow-advisory (6), composition (1).

### Phase B: Fix 59 PARTIAL scenarios (fix stubs in crate tests)

For each: replace the `todo!()` sub-assertion with a real assertion in
the crate unit test, then remove from BDD.

### Phase C: Add 142 GAP-UNIT tests (add crate tests)

Prioritize by risk:
1. Control-plane modules with zero tests (iam, retention, advisory_policy, flavor, maintenance)
2. Composition-ChunkStore wiring (refcounts at boundary)
3. Protocol XDR encoding (NFS3 NULL/WRITE/CREATE/FSINFO, NFS4 compounds)
4. Authentication (tcp_tls.rs mTLS handshake)
5. Advisory hint infrastructure
6. Audit event emission

### Phase D: Add 18 GAP-INTEGRATION scenarios

Move to @integration tier with real backends.

### Phase E: Add 23 GAP-BOTH (unit test + @integration scenario)

Most are composition failure paths, log advisory interactions,
and control-plane cache policy.
