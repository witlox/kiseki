# pNFS DS WRITE Implementation Plan

**Status:** Draft (architect-blessed via `specs/escalations/2026-05-10-pnfs-ds-write-design.md` Option C; ADR-038 rev 3 §D5 amendment in place).
**Created:** 2026-05-10
**Tracks:** ADR-038 §D5 + §D5.1 (chunk-staging DS WRITE) and the two coupled perf items.
**Owner role:** implementer.

## Goal

Wire pNFS DS WRITE end-to-end so the WRITE-mode layout path becomes usable on the local 3-node compose and the GCP perf cluster. Today the path is gated off (`nfs4_server.rs:1402-1405` routes RW-mode LAYOUTGET to MDS unconditionally; compose sets `KISEKI_DISABLE_PNFS_LAYOUT=1` for read-mode too because of the per-file DS-session tax). Three coupled items, in this order:

1. **§D5 chunk-staging WRITE buffer** — make WRITE actually work via the v4-inline pattern (per-stateid buffer + COMMIT flush).
2. **DS session cache** — fix the per-file DS-session-establishment tax independently; even with WRITE wired, the kernel's `EXCHANGE_ID + CREATE_SESSION + RECLAIM_COMPLETE` per OPEN+LAYOUTGET caps short-file throughput.
3. **Re-enable WRITE-mode layouts** + remove the MDS fallback gate; flip compose default `KISEKI_DISABLE_PNFS_LAYOUT=0`; verify against the 2026-05-09 NFSv4.1 perf snapshot.

## Why this binding (not the other shapes)

Per the escalation:

| Shape | Verdict |
|---|---|
| **A** — `GatewayOps::write_at(composition_id, offset, data)` (RMW) | Rejected. Layout invalidates per-WRITE; kernel doesn't recover gracefully; full re-encrypt for any non-full-file write. |
| **B** — Mutable compositions | Rejected. Architectural earthquake (touches ADR-005 / 011 / 040 / advisory / audit); breaks crypto-shred semantics. |
| **C** — Chunk-staging buffer per stateid | **Selected.** Mirrors `nfs_ops::WriteBuffers` v4-inline path. Preserves immutability + content-addressing. ADR-038 §D5 amendment is local. |

## Scope and non-scope

**In scope:**
- New `DsWriteBuffers` (mirrors `WriteBuffers` in `nfs_ops.rs`), keyed on `StateId`, with per-stateid byte cap (`KISEKI_PNFS_DS_BUFFER_CAP_BYTES`, default 256 MiB).
- DS handler for `op::WRITE` in `pnfs_ds_server.rs`: append to buffer; cap-overflow returns `NFS4ERR_NOSPC`.
- DS handler for `op::COMMIT` (already in `ALLOWED_DS_OPS`): drain buffer → `GatewayOps::write` → reply with the composition's verifier.
- DS handler for `op::DESTROY_SESSION` / session expiry: drop buffers for the session's stateids (no implicit flush — kernel must COMMIT before destroying for durability).
- `op::WRITE` added to `ALLOWED_DS_OPS`.
- DS-side session cache keyed on `(client_id, ds_addr)` so `EXCHANGE_ID + CREATE_SESSION + RECLAIM_COMPLETE` doesn't repeat per file.
- Remove the conditional MDS fallback in `nfs4_server.rs:1402-1405` once Phases 1+2 land cleanly.
- `KISEKI_DISABLE_PNFS_LAYOUT` default flips to `0` in `docker-compose.3node.yml`; BDD pNFS scenarios continue to use the existing override.
- BDD: new scenarios for DS WRITE round-trip, NOSPC overflow, COMMIT semantics; refresh existing `pnfs-rfc8435.feature` scenarios that assumed read-only DS.
- Perf: re-run 3-node `tests/e2e/test_perf_baseline.py` NFSv4.1 read+write with layouts ON; capture in `specs/performance/`.

**Out of scope:**
- LAYOUTCOMMIT semantics beyond the basic flush (the kernel client uses LAYOUTCOMMIT primarily for size/mtime updates after a stripe write; we treat it as a fancier COMMIT for now).
- DS-side stateid generation independent of MDS (MDS-authoritative per ADR-038 §D3 stays).
- Multi-DS-mirror writes (mirror count = 1 per ADR-038 §D6 stays).
- ADR-013 ops awaiting data-plane (`specs/escalations/2026-05-09-adr-013-ops-pending-data-plane.md`) — separate plan.

## Phase 1 — chunk-staging buffer + DS WRITE handler

1. **Add `DsWriteBuffers`** in `crates/kiseki-gateway/src/pnfs_ds_server.rs` (or a sibling `pnfs_write_buffer.rs` if it grows). Shape:
   ```rust
   pub(crate) struct DsWriteBuffers {
       buffers: Mutex<HashMap<StateId, BufferEntry>>,
       cap_bytes: u64,  // from KISEKI_PNFS_DS_BUFFER_CAP_BYTES
   }
   struct BufferEntry {
       composition_id: CompositionId,    // from MDS via stateid lookup
       data: BTreeMap<u64, Vec<u8>>,     // offset → bytes; merged on flush
       total_bytes: u64,
       last_write: Instant,              // for idle-eviction telemetry
   }
   ```
   Internal lock pattern matches `nfs_ops::WriteBuffers` (same `lock_or_die` style).

2. **`buffer_write(stateid, offset, data) -> Result<u32, NfsError>`**: validates total_bytes + data.len() ≤ cap; returns `NFS4ERR_NOSPC` on overflow; merges into the BTreeMap so overlapping writes resolve last-write-wins (POSIX `pwrite` semantics).

3. **`flush_writes(stateid) -> Result<CompositionId, GatewayError>`**: walks the BTreeMap into a single `Vec<u8>` (sparse holes = zero per `truncate(2)` semantics — matches inline path behavior), calls `gateway.write(WriteRequest { name: None, ... })`, removes the buffer entry, returns the new composition_id. Updates the fh→composition map via the existing path.

4. **DS `op_write` handler** in `pnfs_ds_server.rs`:
   - Decode `(stateid, offset, stable, data)` from the COMPOUND.
   - `state.buffers.buffer_write(stateid, offset, &data)` → on success reply `WRITE4OK { count, FILE_SYNC, verifier }`.
   - Per RFC 8881 §18.32, `stable=DATA_SYNC`/`UNSTABLE` is honored: kernel can defer COMMIT.
   - Per ADR-038 §D5.1, NOSPC is sticky for that stateid until COMMIT clears the buffer.

5. **DS `op_commit` handler** (replaces the existing stub at the position in `ALLOWED_DS_OPS`):
   - Decode `(offset, count)` per RFC 8881 §18.3 (offset/count are advisory; we flush the entire buffer on any COMMIT).
   - Call `state.buffers.flush_writes(stateid)`.
   - Reply `COMMIT4OK { verifier }` using the new composition's verifier.

6. **`op::WRITE` added to `ALLOWED_DS_OPS`** and the comment at `:53-56` updated to point at this plan + ADR-038 rev 3 §D5.

7. **Telemetry counters**:
   - `kiseki_pnfs_ds_write_bytes_total{state=accepted|rejected_nospc}` — write-buffer accept rate.
   - `kiseki_pnfs_ds_commit_total{state=ok|err}`.
   - `kiseki_pnfs_ds_buffer_bytes` — gauge of total in-flight buffer bytes (Prometheus surface for sizing alerts).

8. **Unit tests** (in-process, mirroring `pnfs_ds_server.rs` existing test style):
   - WRITE then COMMIT → composition_id flows back, data round-trips via gateway.read.
   - WRITE past cap → NFS4ERR_NOSPC; subsequent COMMIT clears buffer; write succeeds again.
   - WRITE with overlapping offsets → last-write-wins; COMMIT produces merged composition.
   - DESTROY_SESSION drops buffers (no flush); subsequent READ via MDS still sees the pre-write composition.

## Phase 2 — DS session cache

Independent of Phase 1; can land in parallel.

9. **Identify the session-establishment site** in `pnfs_ds_server.rs` (`op_exchange_id` + `op_create_session`). Today each (client_id, ds_addr) pair re-establishes per OPEN+LAYOUTGET because the DS doesn't remember session_ids across LAYOUTRETURN+LAYOUTGET cycles.

10. **Add `DsSessionCache`** keyed on `(ClientId, DsAddr)` with TTL (default 5 min, configurable via `KISEKI_PNFS_DS_SESSION_TTL_SECS`):
    ```rust
    pub(crate) struct DsSessionCache {
        sessions: Mutex<LruCache<(ClientId, DsAddr), CachedSession>>,
        ttl: Duration,
    }
    struct CachedSession {
        session_id: SessionId,
        established_at: Instant,
    }
    ```

11. **`op_create_session`** consults the cache first; if present and not expired, returns the cached session_id. If absent, runs the existing establishment path and inserts.

12. **`op_destroy_session`** removes the cache entry (kernel-driven teardown is honored).

13. **TTL eviction** runs lazily on each `op_create_session` call (no background thread needed at this scale).

14. **Unit tests**:
    - Two `op_exchange_id + op_create_session` round-trips for the same `(client_id, ds_addr)` return the same session_id.
    - `op_destroy_session` invalidates; next `op_create_session` mints a fresh session_id.
    - TTL expiry mints a fresh session_id even without explicit destroy.

15. **Telemetry**:
    - `kiseki_pnfs_ds_session_cache_hits_total`.
    - `kiseki_pnfs_ds_session_cache_misses_total`.

## Phase 3 — re-enable WRITE-mode layouts

After Phases 1+2 land cleanly:

16. **Remove the MDS fallback** at `nfs4_server.rs:1402-1405` (the comment block "Even when pNFS is not disabled at the env level, write-mode layouts force the kernel onto the broken DS-WRITE path; flip them to MDS the same way"). Replace with a doc comment pointing at this plan + the rev 3 §D5 amendment.

17. **`docker-compose.3node.yml` env var flip**: drop `KISEKI_DISABLE_PNFS_LAYOUT: "1"` (the comment block stays as historical context with a "see commit X for the unblock"). Pull request that ships Phase 3 also updates the compose; landing it before Phases 1+2 would re-introduce the 0.5 MB/s read regression.

18. **BDD scenarios** in `specs/features/pnfs-rfc8435.feature` (or a new `pnfs-rfc8435-write.feature`):
    - Scenario: Linux 6.x kernel client mounts NFSv4.1, opens a file in RW mode, writes 8 MiB, commits, reads back via the same mount. Asserts: bytes match, no NFS4ERR_NOTSUPP / NFS4ERR_LAYOUTUNAVAILABLE on the client side.
    - Scenario: write 512 MiB through a single stateid → NFS4ERR_NOSPC (cap is 256 MiB), client recovers via COMMIT-then-rewrite.
    - Scenario: kernel client tears down session mid-write (DESTROY_SESSION), reopens, sees pre-write composition (no implicit flush), can re-issue the write.

19. **Perf re-run**: `tests/e2e/test_perf_baseline.py::test_perf_nfs41_seq_read` + `test_perf_nfs41_seq_write` with `KISEKI_DISABLE_PNFS_LAYOUT=0`. Compare to the 2026-05-09 baseline (read 923 MB/s via MDS; write 1644 MB/s via MDS). Acceptance: read ≥ 923 MB/s and write ≥ 800 MB/s (write expected to drop slightly because of LAYOUTCOMMIT round trips; 800 MB/s preserves "useful for HPC" without claiming pNFS is faster than MDS for short files).

## Acceptance criteria

This plan is "done" when:

1. `cargo nextest run -p kiseki-gateway` passes the new DS WRITE / COMMIT / NOSPC unit tests.
2. `cargo nextest run -p kiseki-gateway` passes the new DS session cache unit tests.
3. BDD scenarios in `pnfs-rfc8435-write.feature` pass.
4. `tests/e2e/test_perf_baseline.py::test_perf_nfs41_seq_read` ≥ 923 MB/s with `KISEKI_DISABLE_PNFS_LAYOUT=0`.
5. `tests/e2e/test_perf_baseline.py::test_perf_nfs41_seq_write` ≥ 800 MB/s with `KISEKI_DISABLE_PNFS_LAYOUT=0`.
6. `pnfs_ds_server.rs:53-56` comment + `ALLOWED_DS_OPS` no longer exclude WRITE.
7. `nfs4_server.rs:1402-1405` MDS-fallback block removed (or rewritten to point at this plan as historical context).
8. `docker-compose.3node.yml` `KISEKI_DISABLE_PNFS_LAYOUT` env var removed.
9. ADR-038 rev 3 §D5 + §D5.1 reflected in code comments at `pnfs_ds_server.rs` (`DsWriteBuffers` doc) and `pnfs.rs` (layout-mode handler).

## Effort estimate

- Phase 1 (DS WRITE buffer + handlers + tests): **2 days**.
- Phase 2 (DS session cache): **1 day**.
- Phase 3 (BDD + flag flip + perf re-run + writeup): **1 day**.

**Total:** **~4 days** of focused work for one engineer. Wall-clock estimate: **calendar 1 week** under typical interleaving.

## Risk register

| Risk | Mitigation |
|---|---|
| Buffer cap (256 MiB/stateid) too tight under HPC patterns | Configurable via `KISEKI_PNFS_DS_BUFFER_CAP_BYTES`; default ships generous; alert on `kiseki_pnfs_ds_write_bytes_total{state=rejected_nospc}` non-zero. |
| Per-stateid buffer overlapping-write merge has off-by-one in the BTreeMap range walk | Mirror the v4 inline `nfs_ops::WriteBuffers` impl (same shape, well-tested). Add proptest fuzz on the merge function. |
| DS session cache TTL too short → kernel sees expired sessions and re-establishes anyway | Default 5 min ≥ kernel pNFS client's typical layout lease (30 s); ample margin. Configurable. |
| Removing MDS fallback at `:1402-1405` breaks any existing scenario that depended on RW LAYOUTGET going to MDS | BDD scenarios cover; if anything red, gate Phase 3 on a failing-test triage rather than landing fallback removal blind. |
| Layout invalidation cascades despite the buffer-then-COMMIT design (kernel re-LAYOUTGETs aggressively) | Capture LAYOUTGET frequency in a counter during Phase 3 perf run; if > 1 per 10 MiB written, the design assumption is wrong and we revisit. |
| Per-storage-node aggregate buffer memory exceeds host RAM under high client concurrency | Bound by `max_clients × N_stateids_per_client × 256 MiB`; default DS LRU is 16k stateids per ADR-038 §D3, so a 16-host cluster with 16 pNFS clients is bounded at ~16 × 16k × 256 MiB = a lot. Operator sizing doc gets a paragraph; alerts on `kiseki_pnfs_ds_buffer_bytes`.

## Cross-references

- `specs/architecture/adr/038-pnfs-layout-and-ds-subprotocol.md` rev 3 (§D5 + §D5.1).
- `specs/escalations/2026-05-10-pnfs-ds-write-design.md` (Option C resolution).
- `crates/kiseki-gateway/src/pnfs_ds_server.rs:53-56` (the comment that triggered the escalation).
- `crates/kiseki-gateway/src/nfs_ops.rs:415,429` (`buffer_write` + `flush_writes` — the inline-write pattern Phase 1 mirrors).
- `crates/kiseki-gateway/src/nfs4_server.rs:1356-1357` + `:1398-1405` (env-var gate + WRITE-mode fallback that Phases 2+3 unwind).
- `docker-compose.3node.yml` (`KISEKI_DISABLE_PNFS_LAYOUT=1` at three nodes — added 2026-05-09 commit `da45687`; removed in Phase 3).
- `tests/e2e/test_perf_baseline.py::test_perf_nfs41_seq_{read,write}` (Phase 3 perf gate).
- `specs/performance/2026-05-09-libfuse-swap.md` (current NFSv4.1 baseline numbers Phase 3 must match).
- `specs/escalations/2026-05-09-adr-013-ops-pending-data-plane.md` (separate; not unblocked by this plan).
- ADR-013 §"O_APPEND" (delta-shaped composition update precedent).
- ADR-040, ADR-005 (composition immutability + content-addressing constraints preserved by Option C).
