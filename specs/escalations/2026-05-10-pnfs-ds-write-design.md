# Escalation: pNFS DS WRITE design — `GatewayOps::write_at` shape

**Date:** 2026-05-10
**From:** implementer
**To:** architect
**Status:** **RESOLVED — Option C accepted 2026-05-10.** ADR-038 amended in place to rev 3 (§D5 rewritten + new §D5.1 buffer-cap section). Implementation plan: `specs/implementation/pnfs-ds-write.md`.
**Severity:** Design gap, not a regression. Blocks the WRITE-mode pNFS layout path; today the path is already gated off by `nfs4_server.rs:1402-1405` (write-mode LAYOUTGET routes to MDS regardless of `KISEKI_DISABLE_PNFS_LAYOUT`).
**Recommended outcome:** amend **ADR-038 §D5** in place. No new ADR.

## Finding

`pnfs_ds_server.rs:53-56` excludes `op::WRITE` from `ALLOWED_DS_OPS` with the comment:

> NOTE: WRITE is intentionally absent in Phase 15a — `GatewayOps::write` creates a fresh composition, which doesn't match the pNFS write-to-an-existing-stripe semantics. WRITE is wired in a follow-up phase along with the architect-blessed `GatewayOps::write_at`. See Phase 15b notes.

ADR-038 §D5 claims:

> DS writes (LAYOUTIOMODE4_RW): plaintext from client → `GatewayOps::write` → encrypt → chunk store. This is the same path as the current MDS WRITE op; the DS handler is a thin XDR-decode wrapper around the existing `GatewayOps::write` call.

That's wrong on its face. `GatewayOps::write` (`crates/kiseki-gateway/src/ops.rs:97`) takes a `WriteRequest` containing `tenant_id + namespace_id + data + Option<name> + Option<conditional>` and returns a `WriteResponse { composition_id, bytes_written }`. It creates a **new** composition for the entire `data` payload. pNFS WRITE semantics need:

- An existing **composition_id** (the file the layout addresses),
- A **stateid** to scope the write,
- A byte **offset** into that composition,
- A slice of bytes to overlay starting at offset.

There is no `composition_id` or `offset` parameter in `WriteRequest`. ADR-038 §D5's "thin XDR-decode wrapper" doesn't typecheck.

## Constraints (non-negotiable)

- **Compositions are content-addressed and immutable** (ADR-040 + ADR-005). `composition_id = HMAC(chunk_ids ++ DEK_id)`. Mutating a composition in place changes its id, which breaks pNFS layout caching, Raft replay, audit, advisory checks, and every cache by composition_id.
- **Stripes share the composition's DEK** (ADR-038 §D5 — accurate part). No new key material per stripe.
- **Existing v4 inline WRITE** uses `nfs_ops::buffer_write` + `flush_writes` (`crates/kiseki-gateway/src/nfs_ops.rs:415,429`): per-fh in-memory buffer accumulates writes; CLOSE/COMMIT calls `GatewayOps::write` once on the accumulated bytes; produces a new composition; updates fh→composition_id mapping. The kernel-visible semantics are mutate-by-replacement.
- **ADR-013 §"O_APPEND: Atomic append via delta"** already establishes the precedent that POSIX writes-to-existing-files become delta-shaped composition updates, not in-place mutations.

## Three shape options

### Option A — `GatewayOps::write_at` as read-modify-write (RMW)

```rust
async fn write_at(
    &self,
    composition_id: CompositionId,  // existing composition (the layout target)
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    offset: u64,
    data: &[u8],
) -> Result<WriteResponse, GatewayError>;  // returns new composition_id
```

Read the entire existing composition into a Vec, splice `data` at `offset`, call existing write path to produce a new composition. **`composition_id` changes on every write.**

**Pros:**
- Smallest diff. New trait method; mostly composes existing primitives.
- Preserves content-addressing.
- Composition immutability holds.

**Cons:**
- **Read-modify-write tax dominates** for any non-full-file write. A 4 KiB write at offset 1 GiB requires reading + re-encrypting + re-chunking 1 GiB. Worse than the current MDS path.
- **Layout invalidation cascade:** every WRITE produces a new `composition_id`, so the pNFS layout's `nfl_uri` (which encodes the composition_id per ADR-038 §D6) is stale immediately after the WRITE. Forces LAYOUTRECALL after every WRITE → defeats the entire pNFS-bypass-MDS premise.
- Doesn't match the v4 inline-write pattern; introduces a second write path.

**Verdict:** matches the comment in `pnfs_ds_server.rs` literally but is the wrong shape. Not recommended.

### Option B — Mutable compositions

Add a "live" mode to compositions where chunks can be added/replaced without recomputing the composition_id. Touches:

- ADR-040 (composition store: needs a "live → frozen" transition).
- ADR-005 (chunk durability: in-flight chunks need a stable identity before the composition is frozen).
- Raft replay (intermediate states must be replayable; today only frozen compositions go to the log).
- Advisory subsystem (workflow_ref tracking expects composition_id stability).
- Audit log (every state transition is currently rooted on composition_id).

**Pros:**
- True pNFS WRITE semantics: write-at-offset stays at the same `composition_id`.
- Layouts remain valid across writes (the kernel client likes this).

**Cons:**
- Architectural earthquake. ~5 ADRs implicated. Multi-week investigation just to scope.
- Breaks the content-addressing invariant that anchors crypto-shred (ADR-011) — a "live" composition with mutable chunks can't be reliably crypto-shredded since the chunk set is open-ended.
- Defeats ADR-040's "compositions are append-only deltas" rule that the persistent metadata store assumes.

**Verdict:** structural change for a feature with no urgent business need (NFSv4.1 reads via MDS already work post-`da45687`). Not recommended.

### Option C — Chunk-staging buffer (RECOMMENDED)

Mirror the v4 inline WRITE pattern. DS WRITE accumulates per-`stateid` plaintext into an in-memory buffer; on COMMIT (or session close, or LAYOUTCOMMIT), the buffer flushes via existing `GatewayOps::write` to produce a new composition. The DS-side state is per-stateid buffers; the layout the client receives points at the existing composition until COMMIT, after which the kernel re-LAYOUTGETs and gets the updated composition.

**Implementation surface:**

- New `DsWriteBuffers` (mirrors `nfs_ops::WriteBuffers`) keyed on stateid.
- `op_write` (DS) → `buffers.append(stateid, offset, data)`.
- `op_commit` (DS, already in `ALLOWED_DS_OPS`) → drain buffer → `GatewayOps::write` → respond with new `composition_id` via the existing fh→composition_id mapping path.
- No new `GatewayOps::write_at` method needed. Comment in `pnfs_ds_server.rs:53-56` was wrong about the trait shape; the actual fix is buffer-then-flush.

**Pros:**
- Reuses the existing write path (same crypto, same Raft replication, same audit).
- Preserves composition immutability + content-addressing + ADR-040 + ADR-005.
- Semantically identical to v4 inline WRITE — the DS just becomes a per-storage-node fan-out of the same pattern.
- ADR-038 amendment is small: §D5 changes "GatewayOps::write" → "DS-side buffer accumulates writes; COMMIT flushes via GatewayOps::write, mirroring nfs_ops::flush_writes."

**Cons:**
- Per-file LAYOUTCOMMIT latency dominates (kernel issues frequent COMMITs to bound dirty data; each one is a Raft round trip).
- Layout still invalidates on COMMIT (new composition_id) — same fundamental tension as Option A. **But:** kernel pNFS client expects this and handles re-LAYOUTGET cleanly, vs. Option A's per-WRITE invalidation which it does not handle gracefully.
- Buffer memory pressure scales with in-flight writers × file size; needs a per-stateid buffer cap with overflow → return `NFS4ERR_NOSPC` or force-flush.

**Verdict:** **Recommended.** Smallest deviation from existing architecture, matches the v4 path, no new trait method, ADR-038 amendment is in-place.

## What's NOT in scope

This escalation covers only the trait shape + composition-update semantics. It does **not** cover:

- **Per-file DS-session tax** (`nfs4_server.rs:1376-1393`): kernel does `EXCHANGE_ID + CREATE_SESSION + RECLAIM_COMPLETE` per OPEN+LAYOUTGET, torn down on CLOSE. Independent issue. Even with WRITE wired via Option C, this caps short-file throughput. Fix is a session cache keyed on `(client_id, ds_addr)`. Implementer-shaped work; ~3 days; doesn't need architect blessing.
- **WRITE-mode layout fallback** (`nfs4_server.rs:1402-1405`): currently routes WRITE-mode `LAYOUTGET` requests to MDS even when reads use the DS. Once Option C lands, this fallback can be removed conditionally.

If the architect picks Option C, a follow-up implementation plan covers all three (DS WRITE wiring + session cache + remove WRITE-mode fallback) as a coherent perf unlock.

## Proposed ADR-038 §D5 amendment

Replace the existing §D5 paragraph

> DS writes (LAYOUTIOMODE4_RW): plaintext from client → `GatewayOps::write` → encrypt → chunk store. This is the same path as the current MDS WRITE op; the DS handler is a thin XDR-decode wrapper around the existing `GatewayOps::write` call.

with:

> DS writes (LAYOUTIOMODE4_RW): plaintext from client accumulates in a per-`stateid` in-memory buffer on the DS (mirroring `nfs_ops::WriteBuffers` used by the v4 inline WRITE path). On `COMMIT` (or session close, or LAYOUTCOMMIT), the DS drains the buffer and calls existing `GatewayOps::write` to produce a new composition. The DS handler is a thin XDR-decode wrapper around `buffer_write` + `flush_writes`; the underlying crypto + Raft + audit paths are unchanged from the inline-write case. Composition immutability and content-addressing are preserved (ADR-040 + ADR-005); the kernel pNFS client re-LAYOUTGETs after each LAYOUTCOMMIT to pick up the new `composition_id`.

Add a new sub-section §D5.1:

> **§D5.1. Per-stateid buffer cap.** DS-side write buffers are bounded per stateid (default 256 MiB). Overflow returns `NFS4ERR_NOSPC` to force the kernel client to issue COMMIT-then-OPEN-then-WRITE rather than buffer indefinitely. Cap configurable via `KISEKI_PNFS_DS_BUFFER_CAP_BYTES`.

Cross-link to ADR-013 §"O_APPEND: Atomic append via delta" as the established precedent for delta-shaped composition updates.

## Decision needed

1. Is Option C the right shape? (My recommendation; smallest diff, matches existing architecture.)
2. If yes, accept the ADR-038 §D5 amendment above? Or want me to land a fuller draft as a separate amendment proposal?
3. If no, which of A/B do you want, or what's the alternative?

## Cross-references

- `specs/architecture/adr/038-pnfs-layout-and-ds-subprotocol.md` §D5 (target of amendment).
- `specs/architecture/adr/013-posix-semantics-scope.md` §"O_APPEND" (precedent).
- `specs/architecture/adr/040-persistent-metadata-stores.md` (composition-immutability constraint).
- `specs/architecture/adr/005-ec-and-chunk-durability.md` (content-addressing constraint).
- `crates/kiseki-gateway/src/pnfs_ds_server.rs:53-56` (the comment that triggered this).
- `crates/kiseki-gateway/src/nfs_ops.rs:415,429` (the v4 inline-WRITE pattern Option C mirrors).
- `crates/kiseki-gateway/src/ops.rs:35-97` (current `WriteRequest` + `GatewayOps::write` shape).
- `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md` (precedent for option-listing escalations to architect).
