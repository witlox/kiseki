# Competitive perf targets — kiseki vs Lustre / Ceph / VAST on identical GCP hardware

Back-of-napkin comparison of kiseki against the three filesystems we
most often get asked about: **Lustre** (HPC reference), **Ceph**
(general-purpose open source), **VAST** (commercial disaggregated
all-flash). Same hardware shape, same NIC and storage ceilings —
the differences are purely architectural (replication, metadata
distribution, write path, dedup) and software-level (years of
hardening, kernel client vs userspace).

Companion to:
- [`targets.md`](targets.md) — kiseki's own derived targets per GCP profile
- [`roadmap.md`](roadmap.md) — gap analysis + planned work
- [`README.md`](README.md) — latest measured numbers

> **How to use this doc:** when re-measuring on a 6-node GCP cluster
> (or a similar-shape on-prem deployment), compare each protocol's
> aggregate number to the right-hand columns. If kiseki is *below* a
> competitor's well-hardened number on the same hardware, the gap is
> still in the implementation. If kiseki is at-or-above, the win is
> real and the next bottleneck is somewhere else (NIC, storage,
> client concurrency).
>
> **Numbers are derived, not measured.** None of the three
> competitors are run on c3-standard-22-lssd in this comparison —
> public benchmarks on identical GCP shapes don't exist. The numbers
> are interpolated from each system's community / vendor benchmarks
> on similar Xeon-Platinum + NVMe hardware and adjusted for the
> NIC/storage ceilings of this profile. **Update derivations, don't
> celebrate the gap.**

## Hardware reference — GCP `default` profile

Same shape for everyone. The ceilings apply uniformly; what differs
is how much of them each system can actually use.

| Resource | Per node | Cluster aggregate (6 nodes) | Aggregate to 3 clients |
|---|---:|---:|---:|
| NIC | ~21 Gbps = 2.6 GB/s each way | 15.6 GB/s each way | 7.8 GB/s each way (3 × 2.6) |
| Local NVMe write | 4 × 1.6 GB/s = 6.4 GB/s | 38 GB/s | — |
| Local NVMe read | 4 × 3.2 GB/s = 12.8 GB/s | 77 GB/s | — |
| CPU AES-NI | 22 × 5 GB/s = 110 GB/s | 660 GB/s | — |

**Binding constraint for bulk I/O: NIC.** Storage and CPU are
≥ 2.5× above NIC; the ceiling for sequential reads/writes is set
by the network. Differentiation between systems is mostly in how
efficiently they use the NIC, what replication overhead they take,
and how fast their metadata path is.

## Comparison table

| | **kiseki today**¹ | **kiseki targets**² | **Lustre**³ | **Ceph (CephFS)**⁴ | **VAST**⁵ |
|---|---:|---:|---:|---:|---:|
| **Replication shape** | EC-4+2 (1.5× write) | EC-4+2 (1.5× write) | typically R-1 + RAID (**none** on local SSD) | R-3 (3× write) | EC-4+2 + dedup (~0.75× effective) |
| **Durability on GCP local SSD** | full (cross-node) | full (cross-node) | **none** — VM dies = data dies | full (3-way) | full (cross-DNode) |
| **Aggregate sequential read** | *stale (May: 1.4 GiB/s at under-driven concurrency)* | ~16 GB/s | 8–10 GB/s | 11–13 GB/s | 12–14 GB/s |
| **Aggregate sequential write** | *stale (May: ~15 MB/s, pre-fix era)* | ~5.2 GB/s | 5–8 GB/s (R-1, low durability) | ~5 GB/s (R-3) | 8–10 GB/s |
| **Single-stream bulk read** (per client) | *stale (May: ~180 MB/s)* | ~1.8 GB/s | 2–3 GB/s | 0.8–1.5 GB/s | 2–3 GB/s |
| **Single-stream bulk write** (per client) | *stale (May: ~190 MB/s)* | ~1.8 GB/s | 1.5–2.5 GB/s | 0.5–1.2 GB/s | 1.5–2 GB/s |
| **Small-file 4 KB write IOPS** (aggregate, fresh) | **27.9 k op/s** | ~~360 k~~ audited: 100–200 k conditional | 30–80 k (MDS-bound) | 5–20 k (MDS + OSD) | 50–150 k |
| **Small-file 4 KB write IOPS** (sustained at 3.8 M objects) | **15.1 k op/s** (was 10.6 k pre-#269) | flat-at-fresh is the target | — | — | — |
| **Small-file 4 KB read IOPS** (aggregate, settled) | **275 k op/s** | similar order | 100–200 k | 30–80 k | 100–200 k |
| **Small-write p99 latency** | 39 ms @ conc16 · 142 ms @ saturation (closed-loop) | ~5 ms | ~1–3 ms | ~5–20 ms | ~0.3–1 ms with Optane; **~3–8 ms without**⁶ |
| **Metadata create** (mkdir/create op/s) | *stale (May: ~30 op/s under the since-fixed F-1 wedge)*⁷ | ~5 k op/s | 80–150 k (single MDS) | 5–15 k | 50–100 k |

¹ Small-file rows measured 2026-06-11 on GCP `default` (post-#256/#255
fix validation, main `896433a`, 18-shard topology) — see GH #256 and
[`specs/performance/`](../../specs/performance/) for the run ladder.
Bulk + metadata rows are **stale May-2026 numbers** that predate the
entire June write-path campaign (~25 merged perf PRs, small-file
writes ×97 since); re-measure before quoting (one Pass-D bulk cell
suffices — bulk does not pay the per-op quorum tax that gated the
small-file rows).
² From [`targets.md`](targets.md) `default` profile — derived from
`min(NIC, storage, CPU) ÷ replication × utilization`.
³ Lustre on **local NVMe with R-1** is bandwidth-optimal but has no
durability on GCP (VM ephemeral SSD dies with the VM). The 5–8 GB/s
write number assumes the same R-1 shape kiseki uses for its 5.2 GB/s
target; a realistic prod Lustre on GCP would put OSTs on PD-SSD or
Filestore, which slashes bandwidth ~5×.
⁴ Ceph + BlueStore on NVMe. R-3 default is the binding constraint
on writes (every byte goes 3× over the NIC → ~5 GB/s aggregate at
~15.6 GB/s cluster NIC ÷ 3). Reads at 11–13 GB/s = ~75–85% of NIC
since R-3 reads from one replica. CephFS small-file IOPS is the
known weakness; MDS journal + PG primary serialise per-create.
⁵ VAST sells appliances (Ceres, Universal Storage) with Optane DCPMM
write-buffer + QLC tier. Faithful GCP emulation isn't possible. The
numbers shown are *VAST's algorithms* (similarity dedup, EC across
DNodes, no metadata bottleneck, NFS-direct path) running on
c3-standard-22 with NVMe-only — **the sub-millisecond write latency
disappears without Optane.**
⁶ VAST's famous sub-100-µs write latency is the Optane buffer's
~100 ns. On c3-standard-22 (no Optane), the buffer goes to NVMe
(~10 µs), and the equivalent p99 is ~3–8 ms — same order as Ceph.
⁷ Under hydrator backlog (the F-1 condition before #146 + #147); a
quiet cluster hits ~2 k op/s. **The F-1 wedge itself is fixed in
[#147](https://github.com/witlox/kiseki/pull/147); the steady-state
metadata floor is the upstream commit-pipeline question
([#126](https://github.com/witlox/kiseki/issues/126)).**

## Per-system commentary

### Lustre (LNet over TCP)

The HPC reference filesystem. Centralised MGS/MDS, parallel OSTs,
kernel client. Best aggregate bulk throughput of the three "real"
systems — close to NIC-bound on reads (~8–10 GB/s = ~70% of cluster
NIC at 3 clients).

**Big asterisk on GCP:** Lustre on local SSD has **no real durability
story**. Lustre relies on the OST having a RAID-backed disk or
appliance. On c3-standard-22-lssd, the VM ephemeral SSD dies with the
VM. Realistic prod use needs OSTs on PD-SSD or Filestore, which slashes
bandwidth ~5×.

The "5–8 GB/s write" entry assumes the same R-1, local-NVMe shape
kiseki uses for its 5.2 GB/s target. Apples-to-apples on durability,
Lustre on GCP is closer to ~1 GB/s aggregate write.

**Metadata:** single-MDS shape caps at ~80 k creates/s. DNE
(Distributed Namespace) scales this 4–8× but is rarely deployed.

### Ceph (CephFS + RADOS)

Most general-purpose, weakest perf of the three. R-3 default crushes
write throughput (every byte goes 3× over the NIC) — aggregate
sequential write at ~5 GB/s is just NIC ÷ 3. Reads are better
because R-3 reads from one replica. BlueStore on NVMe is efficient
(~75–85% of raw bandwidth).

**Small files are the known weakness.** MDS journal + OSD PG primary
serialise per-create. CephFS at 4 KB-create has been measured at
5–20 k aggregate on similar shapes; community benchmarks regularly
show this. Latency p99 is ~5–20 ms for a 4 KB write because of the
3-way commit.

**Where kiseki competes:** Ceph's R-3 ceiling vs kiseki's EC-4+2 +
decoupled-ack (ADR-047) is a **2× write-bandwidth advantage on
paper**; the small-file IOPS gap is the deliberate
ADR-047 leaderless intent design.

### VAST

Closest to kiseki in *philosophy* — disaggregated frontend (CNode) +
backend (DNode), EC across DNodes, similarity-based dedup +
compression, no central metadata server, NFSv3/v4.1 + S3 + SMB
protocols.

**The architecture assumes Optane DCPMM as a persistent write buffer
(~100 ns latency).** On c3-standard-22 there's no Optane; the write
buffer goes to NVMe (~10 µs), so the famous sub-100-µs write latency
falls to ~500 µs–3 ms — same order as Ceph.

Bulk throughput stays — VAST's read/write data plane is NIC-bound,
~12–14 GB/s aggregate is realistic on this shape.

**Dedup ratio is workload-dependent.** VAST claims 2–5× similarity
dedup on typical data; kiseki measured **6.03×** on the 2026-05-28
GCP run (better, because content-addressed dedup hits exact-chunk
matches that similarity dedup also finds — *for content with high
exact-chunk redundancy*; on truly random / encrypted data both go to
1.0×).

**Small-file IOPS is good** because metadata is distributed (no
single MDS), but the "100 k+" numbers usually come from the Optane
buffer — without it, ~30–80 k is realistic.

## Where kiseki's *targets* place us

Reading the table top-to-bottom on the targets column:

- **Aggregate read — 16 GB/s** → matches VAST, beats Ceph 25%,
  matches Lustre. NIC-bound just like the others.
- **Aggregate write — 5.2 GB/s @ EC-4+2** → **1.5× better than Ceph
  (R-3)**, close to VAST-without-Optane, beats Lustre-with-durability
  (which on GCP would be ~1 GB/s).
- **Small-file IOPS — 360 k op/s** → **2–4× better than VAST,
  ~30× better than Ceph, ~3–5× better than Lustre.** This is the
  ADR-042 + ADR-047 design point — the leaderless decoupled-ack
  write path is what targets this delta.
- **Write latency p99 — 5 ms** → matches Ceph 4 KB write, beats
  Lustre under MDS load, loses to VAST-with-Optane (sub-1 ms) but
  matches VAST-without-Optane.

## Where kiseki is *today* (2026-06-11)

The June campaign (~25 merged perf PRs: ADR-047 decoupled ack, fjall
sweeps, P1–P4, the fan/committer roadmap, the #256 O(N²) overlay fix,
#255 replication byte-budget) moved the small-file rows from
"30–300× behind" to competitive:

- **4 KB read IOPS: 275 k aggregate, 0 errors** — **above every
  competitor's band on this shape.** The read path delivers the
  design promise today.
- **4 KB write IOPS: 23.7 k aggregate** — **beats Ceph's entire band
  for the first time**; below Lustre's floor and ~half VAST's floor.
  Cumulative ×97 since the May matrix, ×2.4 since the June-10
  baseline (day-normalized ×3.0 vs the 10 k calibration reference).
- **The remaining write gap is architectural, not implementational.**
  Per-op ack costs one intent-quorum network round (~8 ms blocking at
  saturation, conc64 closed-loop); levers since June 10 each measured
  ×1.3–1.4 against ×2 projections — the signature of a latency-bound
  path. Reaching the 360 k design point (and the ~5 ms p99 goal)
  requires changing the unit of consensus at the ingress boundary
  (batched quorum rounds with per-op acks riding the batch) — an
  ADR-grade design decision, tracked as the successor to the #226
  occupancy analysis. Route-to-leader (#135) is the one remaining
  conventional lever (naive 6-ingress spread measured **negative**
  on the healthy binary — un-routed fan adds non-leader contention).
- **Bulk bandwidth rows are unmeasured since May.** Everything that
  made small writes ×97 faster should move bulk writes massively
  (the May 15 MB/s was the since-fixed commit-pipeline era); one
  Pass-D cell updates both rows.
- Known availability caveat under write bursts: GETs can fail (not
  just slow) while the hydrator digests a sustained-burst backlog
  ([#261](https://github.com/witlox/kiseki/issues/261)).

## Honest framing for prod consumers

> "Beats Ceph on small-file writes and beats everything in its class
> on small-file reads (275 k/s aggregate, measured), with full
> cross-node EC durability that Lustre cannot offer on cloud local
> SSD and measured 6× dedup. Small-file write IOPS (23.7 k/s) is
> ~half of VAST-class; closing that gap is a planned consensus-
> batching design change, not incremental tuning. Bulk bandwidth
> unmeasured since the write-path overhaul — re-run pending.
> Pre-production."

## Caveats

1. **Public Ceph / Lustre / VAST numbers on identical GCP shapes
   don't exist.** Interpolated from each system's community / vendor
   benchmarks on similar Xeon-Platinum + NVMe hardware. Update the
   derivations when better data lands.
2. **All three competitors have years of perf hardening**; kiseki
   has had ~3 months of measured perf work and is still pre-prod.
3. **Replication shape matters more than the filesystem name.** A
   Ceph + EC-2+1 pool would beat kiseki's *current* numbers. A
   kiseki cluster with EC-8+2 (also supported) would beat the
   EC-4+2 numbers above.
4. **GCP local SSD is the equaliser** — same NIC, same NVMe, same
   kernel. No system can exceed 15.6 GB/s aggregate NIC ceiling. The
   differentiation is in latency, IOPS, and **how much of the NIC
   each system actually uses**.
5. **Lustre's "no durability on local SSD" caveat is the loud one.**
   The bandwidth numbers shown assume the kiseki-style EC durability
   shape; a fair-on-durability Lustre comparison on GCP needs OSTs on
   persistent disk, dropping bulk write bandwidth ~5×.

## How to update this doc

When re-measuring kiseki on the `default` profile, update the
"kiseki today" column from the latest matrix snapshot in
[`README.md`](README.md). The other three columns shift only on:
- A published Ceph / Lustre / VAST benchmark on a directly
  comparable shape (same NIC, same NVMe-only storage, same node
  count).
- A change in replication shape (e.g. defaulting kiseki to EC-8+2
  would shift the kiseki targets, not the competitor columns).
- A change in GCP hardware (e.g. Tier_1 NIC bump on `transport`
  profile — see [`targets.md`](targets.md) `compact` / `transport`
  sections; this doc covers `default` only).

For other GCP profiles (`compact`, `transport`), add a sibling
section below or a separate table — the methodology in
[`targets.md`](targets.md) section "Methodology" applies uniformly.

## Where kiseki ACTUALLY outperforms today (2026-06-12, measured)

1. **Small-file reads: above every competitor's band.** 275 k op/s
   aggregate 4 KB GETs (settled data) vs Lustre/VAST 100–200 k and
   Ceph 30–80 k on this hardware class. How: content-addressed inline
   tier + decrypt cache + the ADR-042 TCP-framed path — no MDS hop,
   no per-read consensus.
2. **Small-file writes vs Ceph: 1.4–5×.** 27.9 k fresh / 15.1 k
   sustained vs Ceph's 5–20 k band. How: ADR-047 decoupled ack
   (quorum-intent, batch-amortized fan) instead of per-op 3-way
   commit.
3. **Durability on cloud local SSD vs Lustre: categorical.** Cross-
   node EC/replication survives VM loss; Lustre on local SSD does
   not.
4. **Dedup: 6.03× measured** (2026-05-28) vs VAST's claimed 2–5×.

## Known failure modes & limits (2026-06-12, measured)

- **Write amplification ~15–35× is the throughput governor**: every
  4 KiB PUT lands ~6+ durable copies cluster-wide (3 replicas ×
  intent journal/raft log/small store/composition + WALs) before LSM
  compaction re-writes; sustained load saturates the storage device
  and queues every journal commit (the with-volume decay). Survivors
  per the #226/#267 audits: fjall KV-separation; reducing required
  copies. The intent store's share was fixed (#269: epoch-partitioned,
  +42% sustained confirmed); the rest is open.
- **Read availability collapses under sustained cloud-rate ingest at
  volume (#261)**: GETs fail (not just slow) while millions of writes
  of apply backlog drain; reads on settled data are clean (68 k op/s
  local at 1 M objects). Error class capture pending (needs cloud-rate
  arrival). Release-gating for mixed sustained workloads.
- **Replication catch-up frame cap (#255): FIXED** — byte-budgeted
  batches + env escape hatches + restart-under-volume BDD guard.
- Local dev boxes are device-bound: single consumer NVMe saturates at
  ~1/15th of the logical write rate; absolute local numbers are not
  comparable to cluster hardware.
