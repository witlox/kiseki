# ADR-039: Flexible Files Layout — Mirror-List Encoding (one segment, N mirrors)

**Status**: Accepted (validated end-to-end against Linux 6.x pNFS client)
**Date**: 2026-04-29
**Deciders**: Integrator role (kernel-side wire investigation drove the decision)
**Context**: Phase 15c.5 → 15c.10 — pNFS sustained reads from a real Linux client failed end-to-end despite ADR-038's design being structurally correct on the surface

## Revision history

- rev 1 (2026-04-29): supersedes part of ADR-038 §D1's stripe-list
  semantics; codifies the flex-files mirror-list shape that
  Linux 6.x flex-files driver actually expects.

## Problem

ADR-038 §D1 picked Flexible Files Layout (RFC 8435) and described
"per-stripe `nfsv4_1_file_layout_ds_addr4`-style mirror list with a
single mirror." The implementation interpreted that as **N segments
× 1 mirror** — one `layout4` segment per file-position stripe, each
segment carrying one `ff_mirror4` with a single DS:

```
LAYOUTGET response:
  layout4 segments: [
    { offset: 0M, length: 1M, ff_layout4 { mirrors: [DS_node1] } },
    { offset: 1M, length: 1M, ff_layout4 { mirrors: [DS_node2] } },
    { offset: 2M, length: 1M, ff_layout4 { mirrors: [DS_node3] } },
    { offset: 3M, length: 1M, ff_layout4 { mirrors: [DS_node1] } },
    ... 64 stripes total at default cap ...
  ]
```

This passed unit/integration tests on the wire-format level and
worked for Phase 15c.4's 1 MiB single-stripe sandbox. **It failed
catastrophically for any sustained sequential read.** A 30-minute
tcpdump + tshark investigation against the docker-compose 3-node
cluster showed:

1. Kernel issues `LAYOUTGET` for offset=0, gets the 64-segment
   response.
2. Kernel resolves *exactly one* `deviceid` via `GETDEVICEINFO`
   (the first segment's).
3. Kernel reads stripe 0 successfully via DS-direct.
4. Kernel needs stripe 1 (different `deviceid` per round-robin)
   but **never resolves the second device**.
5. Loops on `LAYOUTGET` → `LAYOUTRETURN` until userspace times out
   (180s+ for fio, EIO for dd).

The wire-correct shape per RFC 8435 §13.2 is **striping inside one
segment** via `ffl_mirrors<>` and `ffl_stripe_unit`, NOT one
segment per file-position stripe:

> The `ffl_stripe_unit` is the same as the corresponding NFSv4.1
> file mapping field; it controls the alignment of stripes…
> Within a single `ff_layout4`, `ffl_mirrors[i]` covers every
> `i`-th `stripe_unit`-sized chunk of the segment.

Linux's flex-files driver expects this shape. Per-segment mirrors
work only when the kernel can resolve and bind every segment's DS
ahead of dispatch — which it doesn't do efficiently (one
`GETDEVICEINFO` per layout, then dispatch within bound mirrors).

## Decision

### D1. LAYOUTGET emits ONE `layout4` segment with N mirrors

```
LAYOUTGET response:
  layout4 segments: [
    {
      offset: <kernel-requested>,
      length: <bounded by max_stripes_per_layout × stripe_size>,
      ff_layout4 {
        ffl_stripe_unit: <stripe_size_bytes>,    // = 1 MiB by default
        ffl_mirrors: [
          ff_mirror4 { ffm_data_servers: [{ deviceid: node1, fh: ..., stateid: 0 }] },
          ff_mirror4 { ffm_data_servers: [{ deviceid: node2, fh: ..., stateid: 0 }] },
          ff_mirror4 { ffm_data_servers: [{ deviceid: node3, fh: ..., stateid: 0 }] },
        ],
        ffl_flags: FF_FLAGS_NO_LAYOUTCOMMIT,
      }
    }
  ]
```

The kernel computes `mirror_idx = (file_offset / stripe_unit) %
num_mirrors` for each byte and dispatches the read to that
mirror's DS at the absolute composition offset (RFC 8435 §13.2).

### D2. The DS-side file handle is whole-segment (`stripe_index = 0`)

Pre-15c.9 the FH encoded `(composition_id, stripe_index)` and the
DS computed `abs_offset = stripe_index * stripe_size + kernel_offset`.
With one segment and per-mirror dispatch, the kernel sends *file
offsets* directly. The FH carries `stripe_index = 0` and the DS
reads at `abs_offset = kernel_offset`, bounded only by the
kernel's `count` (already `≤ rsize`).

The legacy per-stripe path (`stripe_index > 0`) is preserved in
`pnfs_ds_server::op_read_ds` for backward compatibility with
cached layouts during a server upgrade — but the LAYOUTGET
encoder no longer emits them.

### D3. Replication-3 makes mirror_idx purely load-balancing

In a striped multi-mirror flex-files layout the kernel routes
"every Nth stripe" to mirror N. Per-mirror data could differ
(true striping). In kiseki's Replication-3 deployment, *every
node holds every byte* — chunk replication happens below the
gateway via Raft. So:

- Mirror 0's DS can serve *any* offset.
- Mirror 1's DS can serve *any* offset.
- Mirror 2's DS can serve *any* offset.

The `mirror_idx` is purely a *load-balancing hint*. The kernel's
choice spreads sustained-read traffic across cluster nodes; if a
mirror's DS is unreachable, the kernel can fall back to another
(RFC 8435 §6).

For a future EC deployment where each mirror holds only its
shard, the DS would need to translate `mirror_idx` → "which
shards do I have, and is this offset one of them?" That's a
follow-up tracked on the EC track.

### D4. Layout coverage check on cache hits

The MDS layout cache is keyed by `composition_id`. Pre-15c.9 the
cache returned the cached layout regardless of the kernel's
requested `(offset, length)`, which broke the kernel's state
machine when an earlier LAYOUTGET seeded the cache at offset=0 and
a later one asked for offset=N. ADR-039 codifies the contract:
**a cache hit returns the cached layout only if it covers the
requested range** (`first.offset ≤ requested.offset` AND
`last.offset + last.length ≥ requested.offset + requested.length`).
On a coverage miss, the cache entry is replaced.

## Alternatives considered

### A1. Single mirror per composition (hash-pinned)

Pick one node per composition (deterministic hash of
composition_id), emit only that mirror. The kernel binds one DS,
reads from it for the whole file; no mirror cycling, no per-stripe
GETDEVICEINFO churn.

**Tested 2026-04-29 in Phase 15c.10**: broke pNFS dispatch
entirely. The kernel cycled on `LAYOUTGET` / `LAYOUTRETURN` when
our cache returned the same pinned mirror after a kernel-side
return — Linux apparently expects mirror diversity in the layout
even when only one is needed for a given read.

**Verdict**: rejected.

### A2. `return_on_close = false`

Tell the kernel "don't auto-return the layout on close." Reduces
LAYOUTGET re-acquisition between fio iterations.

**Tested 2026-04-29 in Phase 15c.10**: broke pNFS dispatch — the
kernel got into a different deadlock when it never returned the
layout, and `dd` hung in close().

**Verdict**: rejected.

### A3. Reduce `max_stripes_per_layout` to 1 (single-stripe per LAYOUTGET)

Force the kernel to issue a fresh LAYOUTGET on every stripe
boundary, hoping that simplifies dispatch.

**Tested 2026-04-29 in Phase 15c.10**: same hang as the original
multi-segment case. The kernel still expects RFC 8435 §13.2
striping inside the segment regardless of how many segments are
returned.

**Verdict**: rejected.

### A4. NFSv4.1 File Layout (RFC 5661 §13) instead of Flex Files

Drop FFL entirely, use the older File Layout. ADR-038 §D1 already
considered this and rejected it because of the per-DS state
complexity. Phase 15c.10 didn't reopen the question — the bug
was in the FFL encoding, not FFL itself.

**Verdict**: not reconsidered; ADR-038 §D1 stands.

## Consequences

### Pros

- **NFSv4.1 sustained-read works end-to-end against Linux 6.x.**
  `test_pnfs_plaintext_fallback` (1 MiB) still green; an 8 MiB
  read completes in 5.5 sec wall (was 180s+ timeout).
- **Wire-level throughput is real.** tcpdump shows 8 × 1 MiB DS
  reads in ~20 ms (≈400 MB/s). The slow fio numbers reported in
  earlier perf-test runs are benchmark artifacts — wrapping over
  a small file in page cache — not protocol overhead.
- **Cluster-wide load balancing for free.** The kernel's
  `mirror_idx` spreads sustained reads across all 3 nodes
  without any server-side coordination.
- **Smaller LAYOUTGET response.** ~830 bytes for a 3-node
  cluster vs ~12 KiB for the pre-15c.9 64-segment encoding.

### Cons

- **No per-stripe FH authorization.** Every mirror carries a
  whole-segment FH. A leaked FH gives access to the entire
  composition's offset range, not just one stripe's. ADR-038
  §D4.3's per-stripe MAC was the security boundary; with
  whole-segment FHs the boundary is per-composition + per-key-
  rotation. This is acceptable because (a) keys rotate on a
  bounded schedule and (b) the FH still carries an HMAC + expiry,
  so a leaked FH expires.
- **Layout invalidation on cache miss is more visible.** With
  one segment per layout, a coverage-mismatched LAYOUTGET
  invalidates the entire cached layout. With many small
  segments, the kernel could potentially keep the segments that
  do cover its range. Doesn't matter in practice — the kernel
  rarely keeps partial layouts.
- **EC follow-up is harder.** When EC fragments span multiple
  nodes (k+m striping per chunk), the mirror_idx mapping needs
  to align with EC shard placement so the kernel reads each
  shard from the right node. Tracked separately as a Phase 16+
  consideration.

## Test gating

- **Unit (this commit)**: 4 tests in `pnfs::mds_layout_tests`:
  `layout_covers_full_requested_range`,
  `every_mirror_carries_a_whole_segment_fh4`,
  `cache_recomputes_when_offset_is_outside_cached_range`,
  `cache_hits_when_offset_is_inside_cached_range`.
- **Integration (this commit)**: `rfc8435::layoutget_then_getdeviceinfo_round_trip`
  asserts one mirror per cluster node + 4 MiB segment.
- **BDD (this commit)**: `@pnfs-15b` scenario "LAYOUTGET returns
  a well-formed ff_layout4 body" updated to assert the
  one-segment-N-mirrors shape.
- **E2E (this commit)**: `tests/e2e/test_pnfs.py::test_pnfs_plaintext_fallback`
  is the witness — passes against the 3-node compose with a
  1 MiB read; 8 MiB sustained read completes in < 6 sec wall.

## References

- RFC 8435 §5.1, §13.2 — Flexible Files Layout encoding +
  striping
- ADR-038 — pNFS Layout and DS Subprotocol (rev 2)
- `specs/findings/phase-15c8-nfs41-perf-investigation.md` — the
  tcpdump trace that surfaced the encoding problem
- `specs/findings/phase-15c10-nfs41-perf-investigation.md` —
  the failed-experiments record (single-mirror pinning,
  return_on_close=false, max_stripes=1)
- Commits: `379a524` (Phase 15c.9 layout encoding),
  `d3c2329` (cache coverage check),
  `6517679` (LAYOUTRETURN routing + lrs_present),
  `1d5b576` (BDD update)
