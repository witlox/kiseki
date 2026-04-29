# Phase 15c.10 — NFSv4.1 perf investigation (post-15c.9)

**Date**: 2026-04-29
**Mode**: integrator (wire-level perf debug)
**Outcome**: no code changes shipped; perf gap is a test-fixture
artifact, not a real bottleneck. Documented for future work.

## Question

After Phase 15c.9 unblocked the NFSv4.1 sustained-read hang,
`test_perf_nfs41_seq_read` reported 0.3 MB/s — vs NFSv3's 65 MB/s
on the same cluster. 200× gap. Is this a real perf bug or a
benchmark artifact?

## Approach

tcpdump + tshark on the docker `kiseki_default` network during a
controlled 8 MiB read via dd and fio. Cross-reference fio's clat
histogram against the actual wire-level read latency.

## Wire-level findings

**Per-read NFSv4.1 latency on the wire is fine.**

`dd if=$file of=/dev/null bs=1M count=8` against the docker
3-node cluster:
- Total wall: 5.5 sec
- Wire reads: 8 × 1 MiB completed in **~20 ms total** (frame
  5.383 → frame 5.404 in the trace)
- Mount + LAYOUTGET + DS-session-establishment dance: ~5.3 sec

So on the wire, throughput is **~400 MB/s**. The 5.5 sec wall
time is dominated by one-time setup, not by reads.

## Why fio reports 0.3 MB/s

fio's command in the perf baseline:

```
fio --rw=read --bs=1M --size=8M --runtime=10 --time_based
```

`--time_based` runs for 10 sec, looping over the file. With the
file fitting in page cache after the warmup `dd`, each fio
iteration:

1. Reads from page cache (microseconds)
2. Loops back to offset 0
3. Loop wrap triggers a kernel-internal layout-cache invalidate
4. Issues fresh LAYOUTGET → GETDEVICEINFO → DS-session-setup
   (~300 ms wire, often delayed several more seconds by some
   kernel-side timer we couldn't identify)
5. Issues 1 MB read on the new layout
6. Repeats

Wire trace from a 5-sec fio run showed:
- 1 OPEN at t=0
- 5.5-sec gap of nothing
- 1 LAYOUTGET, 1 GETDEVICEINFO, 1 READ at t=5.7
- LAYOUTRETURN at t=6.0
- Nothing else until end

fio submitted one 1 MB read in 6 sec. That's the 0.3 MB/s.

The bottleneck is in the kernel's layout-cache-invalidation +
re-acquisition cycle on each fio loop wrap, not the protocol or
the server.

## What didn't work

1. **`return_on_close = false`** — broke pNFS dispatch entirely;
   the kernel got into a different deadlock when the layout was
   never returned. Reverted.

2. **Single-mirror pinning per composition** (hash-based mirror
   selection) — broke pNFS dispatch entirely; the kernel cycled
   on LAYOUTGET/LAYOUTRETURN when our cache returned the same
   pinned mirror after a kernel-side return. Reverted.

3. **Reducing `max_stripes_per_layout` to 1** — same hang as
   pre-15c.9, didn't help.

The 15c.9 multi-mirror layout encoding is the right shape per
RFC 8435. The Linux kernel handles it correctly for sustained
sequential reads (the dd test proves throughput at the wire level
is 400+ MB/s once the session is established).

## What would actually help

The test-side fix is to use a workload where setup amortizes:
- Larger file (1 GB+) so `--size` doesn't wrap during the run
- `--ramp_time=5s` on fio so the warmup doesn't count toward
  measurement
- Or just measure with `dd` against a fresh (not-cached) file

For real workloads — checkpoints, model weights, inference
caches — the file is opened once, read sequentially, and the
setup cost amortizes over the read. NFSv3's 65 MB/s and pNFS's
400 MB/s wire throughput are both realistic for those flows.

Actual server-side optimizations that might reduce the
LAYOUTGET-cycle latency (separate from this investigation):
- Cache the (mirror, DS) session on the kiseki server side and
  hand back consistent layouts even after a LAYOUTRETURN
- Investigate the ~5-sec kernel-side gap between OPEN and first
  LAYOUTGET (might be a sysctl or RPC retry timer that we can
  influence via mount options)
- Tune `lease_time` or `layout_ttl_seconds` to nudge kernel
  behavior

## Verdict

No code change shipped this session. The Phase 15c.9 layout
encoding is correct; the fio number is a benchmark artifact.

For Phase 15c.10 close-out:
- pNFS sustained-read DISPATCH works (the breakthrough was 15c.9)
- pNFS sustained-read THROUGHPUT at the wire level is 400+ MB/s
- fio's reported 0.3 MB/s is the result of measuring per-loop
  setup cost, not protocol overhead
- The fio-test should be revised to use ramp_time + larger size
  if we want the perf baseline to reflect realistic workloads
