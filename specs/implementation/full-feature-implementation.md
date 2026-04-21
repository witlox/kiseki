# Full Feature Implementation Plan (Code First, BDD After)

## Context

BDD step fixing repeatedly failed because domain code is missing.
Instead of papering over gaps with fake assertions, build ALL missing
features first, then wire BDD after. Adversarial review before BDD pass.

**Reference**: `specs/implementation/100pct-completion-plan.md` (BDD plan,
resume after this plan completes).

## Approach

- Build real domain code with TDD (unit tests per module)
- No BDD step changes until all features built
- Each phase: implement → unit test → commit
- Adversarial review after all phases complete
- Then return to BDD plan to wire steps to real code

---

## F1: NFS3 Complete (11 missing procedures)

**File**: `crates/kiseki-gateway/src/nfs3_server.rs`

Add: SETATTR, MKDIR, RMDIR, LINK, SYMLINK, READLINK,
READDIRPLUS, ACCESS, COMMIT, PATHCONF, MKNOD.

Fix: READDIR (cookie pagination), FSINFO/FSSTAT (real pool data).

Each handler: decode XDR → NfsContext → encode XDR reply.
NfsContext needs: `mkdir()`, `rmdir()`, `setattr()`, `symlink()`,
`readlink()`, `access()`, `commit()`.

**Exit**: All 22 NFS3 procedures return real data + unit tests.

## F2: NFS4 Complete (27 missing operations)

**File**: `crates/kiseki-gateway/src/nfs4_server.rs`

Priority: READDIR (real), SETATTR, RENAME, ACCESS,
SAVEFH/RESTOREFH, RECLAIM_COMPLETE.

Fix: LOCK (conflicts), SEQUENCE (slots), IO_ADVISE (advisory),
OPEN (share/deny).

**Exit**: All RFC 7862 ops per ADR-023 scope + unit tests.

## F3: S3 Complete

**File**: `crates/kiseki-gateway/src/s3_server.rs` + `s3.rs`

Implement: DELETE (real), LIST prefix filtering, LIST pagination,
multipart upload (init/part/complete/abort), conditional requests
(If-None-Match, If-Match).

**Exit**: All S3 routes return real data + unit tests.

## F4: Key Rotation + Re-wrapping Worker

**New**: `crates/kiseki-keymanager/src/rewrap_worker.rs`

Implement: background chunk enumeration, DEK re-wrap from old to
new epoch, progress tracking, cancellation, full re-encryption engine.

**Exit**: `rotate()` triggers background re-wrap + unit tests.

## F5: Log Auto-Split + Compaction Worker

**New**: `crates/kiseki-log/src/auto_split.rs`, `compaction_worker.rs`

Implement: threshold monitoring → split trigger, key range division,
delta redistribution, configurable compaction rate.

**Exit**: Shards auto-split + background compaction + unit tests.

## F6: View Stream Processor + Versioning

**New**: `crates/kiseki-view/src/versioning.rs`
**Extend**: `stream_processor.rs`, `view.rs`

Implement: object version history, historical reads, staleness SLO
enforcement, stream processor materialization, rebuild from log.

**Exit**: Views materialize from deltas, versioning works + unit tests.

## F7: Client Discovery + FUSE Pipeline

**Extend**: `crates/kiseki-client/src/discovery.rs`, `fuse_fs.rs`
**New**: `transport_select.rs`, `batching.rs`, `prefetch.rs`

Implement: seed discovery, transport fallback, write coalescing,
readahead detection, FUSE wired through gateway with encryption.

**Exit**: Client discovers, selects transport, FUSE roundtrip + unit tests.

## F8: StorageAdminService

**New**: `crates/kiseki-control/src/storage_admin.rs`,
`grpc/storage_admin_service.rs`, `admin.proto`

Implement: 20+ RPCs — pool CRUD, device lifecycle, shard management,
tuning, observability streaming, authorization (admin/SRE roles).

**Exit**: All admin RPCs callable + unit tests.

## F9: Auth — SPIFFE + IdP + Revocation

**New**: `crates/kiseki-transport/src/spiffe.rs`, `revocation.rs`,
`crates/kiseki-control/src/idp.rs`

Implement: SPIFFE SVID URI parsing, OIDC JWT validation, CRL
fetch+cache+verify.

**Exit**: Identity extraction works for all cert types + unit tests.

## F10: Operational — Integrity + Versioning + Compression

**New**: `crates/kiseki-server/src/integrity.rs`,
`crates/kiseki-common/src/versioning.rs`
**Wire**: `kiseki-crypto/compress.rs` into gateway pipeline

Implement: ptrace detection, core dump blocking, format_version on
DeltaHeader, version negotiation, compression tenant opt-in.

**Exit**: Monitor runs, versions checked, compression end-to-end + unit tests.

---

## Execution order

```
F1 (NFS3) ─────────┐
F2 (NFS4) ─────────┤
F3 (S3) ───────────┤
F4 (key rotation) ─┤
F5 (log split) ────┤──→ ADVERSARIAL REVIEW ──→ BDD WIRING
F6 (view+version) ─┤
F7 (client) ───────┤
F8 (admin API) ────┤
F9 (auth) ─────────┤
F10 (operational) ──┘
```

## Verification per phase

1. `cargo test -p <crate>` — new unit tests pass
2. `cargo clippy -p <crate>` — no warnings
3. `cargo test --workspace` — no regressions
4. Commit with description of what was built

## After all F-phases

1. Adversarial review of all new code (gate-2)
2. Fix blocking findings
3. Return to BDD plan — wire steps to real code
4. Target: 456/456 with real assertions

## Estimated effort

| Phase | Sessions |
|-------|----------|
| F1 NFS3 | 1 |
| F2 NFS4 | 2 |
| F3 S3 | 1 |
| F4 Key rotation | 1 |
| F5 Log split | 1 |
| F6 View+version | 1 |
| F7 Client | 2 |
| F8 Admin API | 2 |
| F9 Auth | 1 |
| F10 Operational | 1 |
| Adversarial | 1 |
| **Total** | **~14** |
