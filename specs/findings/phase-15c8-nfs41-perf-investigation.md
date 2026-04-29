# Phase 15c.8 — NFSv4.1 fio sustained-read investigation (tcpdump session)

**Date**: 2026-04-29
**Mode**: integrator (kernel-side wire investigation)
**Outcome**: 3 real bugs shipped + 1 architectural follow-up scoped.

## Symptom

`fio --rw=read --bs=1M --size=8M --time_based --runtime=10` against
a pNFS-mounted 8 MiB file from a Linux 6.x kernel client hangs for
180s+. Same shape with `dd if=... bs=1M count=8` — the read never
completes. `test_pnfs_plaintext_fallback` (1 MiB single-stripe) is
green; the hang only kicks in for sustained sequential reads that
need more than one stripe.

## Wire-level capture

`tcpdump -i any -w nfs.pcap "port 2049 or port 2052"` on the
docker `kiseki_default` network during the hang. Decoded with
`tshark`. Key timeline:

```
t=0.009  OPEN+LAYOUTGET → 5224-byte response (64 stripes, OK)
t=5.057  GETDEVICEINFO #1 (resolves kiseki-node1 only)
t=5.057  TCP connect → port 2052 (DS) on node1
t=5.060  EXCHANGE_ID #1 to DS
t=5.142  EXCHANGE_ID #2 to DS  (kernel retries — odd)
t=5.184  EXCHANGE_ID #3 to DS  (third try; eventually CREATE_SESSION)
t=5.349  READ stateid=... offset=0 len=1048576 → 1 MiB succeeds via DS
t=5.382  ... SILENCE on DS for ~2.6s ...
t=5.382  LAYOUTGET on MDS port for offset=1MB, len=16384
         → loops: ~30,000 LAYOUTGETs + LAYOUTRETURNs in 2.6s
t=8.009  DESTROY_SESSION on DS, dd times out
```

The kernel:
1. Got the 64-stripe layout (1 segment per stripe; each segment
   has 1 mirror with 1 DS).
2. Issued **exactly one** GETDEVICEINFO — for stripe 0's device.
3. Read stripe 0 via DS-direct (one successful 1 MiB read).
4. Tried to read stripe 1 (offset=1MB), which lives on a
   *different* device (kiseki-node2) per our round-robin layout.
5. **Never issued GETDEVICEINFO for the second device.**
6. Looped on LAYOUTGET/LAYOUTRETURN until the read syscall
   returned EIO.

## Root cause — layout encoding

Our LAYOUTGET emits one `layout4` segment per stripe, each with:

```
ff_layout4 {
    ffl_stripe_unit: 1 MiB
    ffl_mirrors: [
        ff_mirror4 { ffm_data_servers: [single_DS_for_this_stripe] }
    ]
}
```

64 segments × 1 mirror × 1 DS = 64 distinct (segment, DS) pairs.
Linux's flex-files driver doesn't reliably dispatch reads across
*per-segment* DS endpoints — it expects the RFC 8435 §5.1 *striping
within one segment* shape:

```
ff_layout4 {                              // ONE segment
    ffl_stripe_unit: 1 MiB
    ffl_mirrors: [                        // multiple mirrors
        ff_mirror4 { data_servers: [DS_node1] },   // mirror 0
        ff_mirror4 { data_servers: [DS_node2] },   // mirror 1
        ff_mirror4 { data_servers: [DS_node3] },   // mirror 2
    ]
}
```

The kernel uses
`mirror_index = (offset / stripe_unit) % num_mirrors` to pick the
DS for any byte. Each mirror's DS holds *every Nth stripe*
concatenated as a contiguous stream — so DS_node1 holds stripes
0, 3, 6, 9, ..., DS_node2 holds 1, 4, 7, ..., DS_node3 holds 2, 5,
8, .... The DS reads each mirror as one logical file via a single
file handle.

We don't do this. The DS server (`pnfs_ds_server.rs::op_read_ds`)
expects `(stripe_index, op_offset)` per-stripe addressing baked
into the file handle. To support proper RFC 8435 striping we'd
need a different DS-side addressing scheme: either consolidate
stripes per-mirror server-side, or change the FH to encode a
mirror_id and let the DS map back to (composition, mirror_stripe).

That's a substantial follow-up — **Phase 15c.9**.

## What this session shipped (3 real bugs killed)

Each is RFC-correct independent of the bigger architectural issue
above:

| Commit | Bug |
|---|---|
| `d3c2329` | Layout cache returned stale entries that didn't cover the requested `(offset, length)`. Pre-fix: any LAYOUTGET that asked for offset=N got back the cached offset=0 layout if the FIRST request seeded the cache at offset=0; kernel saw the segments not covering its range, re-issued LAYOUTGET, ~29k retries in 8s. |
| `6517679` | LAYOUTRETURN routed to legacy `LayoutManager` (Phase 14 stub at `ctx.layouts`) instead of `MdsLayoutManager`. Server-side cache never invalidated; kernel cleared its local state but server kept serving the stale layout. |
| `6517679` | LAYOUTRETURN response said `lrs_present=true` followed by an all-zeros stateid — incoherent per RFC 5661 §18.4.4. Linux's state machine read it as "layout still partially held with stateid=ANON" and refused to settle. Kiseki doesn't track partial layout state; correct response is `lrs_present=false`. |

Unit-tested via `cache_recomputes_when_offset_is_outside_cached_range`,
`layout_return_by_stateid_removes_matching_entry`, etc.

## What this session did NOT fix

- **8 MiB sustained-read hang.** Same root cause as before — the
  per-stripe layout segment encoding doesn't match Linux's
  flex-files dispatch model. Tried `max_stripes_per_layout: 1` to
  emit one stripe per LAYOUTGET (forcing kernel to re-LAYOUTGET on
  each new stripe). Result: same hang, EIO after 13s. The kernel's
  pNFS state machine is deadlocked at a deeper level than any
  cap can fix.

## Verified post-session

- `test_pnfs_plaintext_fallback` (1 MiB pNFS DS-direct read) —
  PASSES.
- `cargo clippy --workspace --lib --tests -- -D warnings` —
  clean.
- `cargo test -p kiseki-gateway --lib` — 145+ tests pass.
- Full e2e suite (`pytest tests/e2e/ -m e2e`) — 29 passed, 2
  failed (the documented NFSv4.1 perf hang), 3 skipped (expected).

## Phase 15c.9 follow-up scope

Re-encode our flex-files layout to be one segment with multiple
mirrors:

1. Server-side: `op_layoutget_ff` emits one `layout4` segment
   covering the requested range, with N `ff_mirror4`s where N =
   number of DSes (one per cluster node).
2. DS-side: change `pnfs_ds_server::op_read_ds` to address by
   mirror_id (or accept whole-file FHs) rather than per-stripe FHs.
3. Either: (a) physically restripe data into per-mirror streams,
   or (b) keep replication-N at the chunk store (every node holds
   every chunk) and let each mirror's DS read any byte from its
   local store.

(b) is the simpler path — the cluster is already Replication-3 so
every node CAN serve any byte. The DS just needs to translate the
mirror's view (logical offset within the per-mirror stream) back
to the composition's absolute offset. With identical content on
every mirror, that translation is the identity function and the
mirror_index is just a load-balancing hint.

This is a Phase 15c.9 / Phase 16-extended task, not Phase 15c.8.
