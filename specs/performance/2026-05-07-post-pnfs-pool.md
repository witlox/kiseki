# 2026-05-07 — local single-node matrix (post pNFS DS pool)

| Field | Value |
|---|---|
| Date | 2026-05-07 |
| HEAD | `5fc9523` (perf(profile): pool pNFS DS sessions) |
| Hardware | dev workstation, AMD Ryzen 7 6800H, 16 vCPU, Linux 7.0.3-arch1-2 |
| Cluster | single-node `kiseki-server` (ephemeral ports, plaintext) |
| Object size | 65 536 B |
| Concurrency | 16 |
| Duration | 30 s per shape |
| Warmup | 256 objects (get-heavy / mixed) |
| Tooling | `bash crates/kiseki-profile/run-all.sh` |
| Output dir | `/tmp/kiseki-prof/` |

## Why this snapshot

Three commit-classes since the 2026-05-07 (`51c48aa`) snapshot:

- **`fix(clippy)` sweep × 2** (`950eaf0`, `d7a2c45`): cleared the
  workspace-wide pedantic debt rust 1.95 surfaced. Real fixes —
  proper docs, typed error variants, dead-code removal — no
  blanket `#[allow]` to silence. ~146 lints across kiseki-chunk,
  kiseki-composition, kiseki-gateway, kiseki-client, others.
  See the addendum for the substantive bug fixes.
- **`fix(gateway)` namespace cache priming** (`ea3a297`):
  `InMemoryGateway::new` now seeds `namespace_meta` from the
  `CompositionStore` it wraps. Previously a gateway constructed
  from a pre-populated store had an empty cache, the `read_only`
  check fell through, and POSIX writes against a read-only
  namespace returned EIO instead of EROFS. Closes a
  long-standing TDD-RED test (`write_to_readonly_namespace_
  returns_erofs` from `1e69269`).
- **`ci(bdd)` + `perf(profile)` pNFS pool** (`e2d5de4`,
  `5fc9523`): switched the BDD lane to `cargo test` instead of
  `nextest run` (cucumber is `harness = false`, nextest's libtest
  enumeration fails on cucumber-rs's clap CLI), tagged 3
  intentional-TODO scenarios with `@deferred-feature`, and
  replaced the per-DS `Mutex<PnfsSession>` with a round-robin
  pool sized by `pool_size`. The pool fix is the headline number
  in this snapshot.

CI went fully green for the first time since rust 1.95.0 stable
on commit `5fc9523`.

## Throughput (CPU phase, pprof-instrumented server)

| Protocol | put-heavy | get-heavy | mixed (70 P / 30 G) |
|---|---:|---:|---:|
| **S3 (HTTP)** | 36 675 op/s · 2 292 MiB/s | 77 414 op/s · 4 838 MiB/s | 47 584 op/s · 2 974 MiB/s |
| **NFSv3** | 42 915 op/s · 2 682 MiB/s | **108 063 op/s · 6 754 MiB/s** | 43 173 op/s · 2 698 MiB/s |
| **NFSv4.1** | 48 932 op/s · 3 058 MiB/s | 63 105 op/s · 3 944 MiB/s | 49 462 op/s · 3 091 MiB/s |
| **pNFS Flex Files** | 47 699 op/s · 2 981 MiB/s | **79 867 op/s · 4 992 MiB/s** | 50 192 op/s · 3 137 MiB/s |
| **FUSE** | **51 504 op/s · 3 219 MiB/s** | **125 606 op/s · 7 850 MiB/s** | **61 956 op/s · 3 872 MiB/s** |

## Tail latency p99 (µs)

| Protocol | put-heavy | get-heavy | mixed |
|---|---:|---:|---:|
| S3 | 925 | 510 | 832 |
| NFSv3 | 854 | 411 | 902 |
| NFSv4.1 | 752 | 615 | 783 |
| pNFS | 816 | 510 | 743 |
| FUSE | 707 | 421 | 698 |

## Delta vs 2026-05-07 (`51c48aa`) snapshot

| Protocol | PUT prev / now / Δ | GET prev / now / Δ |
|---|---|---|
| S3 | 42 160 / 36 675 / 0.87× | 75 078 / 77 414 / +3 % |
| NFSv3 | 5 006 / 42 915 / **8.6×** | 107 830 / 108 063 / ≈ |
| NFSv4.1 | 5 008 / 48 932 / **9.8×** | 58 861 / 63 105 / +7 % |
| pNFS | 4 970 / 47 699 / **9.6×** | **17 673 / 79 867 / 4.5×** |
| FUSE | 52 888 / 51 504 / -3 % | 115 368 / 125 606 / +9 % |

PUT lifts on NFS variants reflect the `b6c4c74`–`38f5db8`
DirectoryIndex reverse-index fix landing between snapshots.
The S3 PUT delta (-13 %) is run-to-run noise on a workstation
under concurrent compile load — the standalone S3 PUT c=16
remained at ~42 k op/s in cross-checks.

## Headline: pNFS DS pool

This is the snapshot's namesake change. Previous pNFS GET sat at
17-18 k op/s for the entire post-fix window — the harness
serialized every DS read through ONE `Mutex<PnfsSession>` per
address. Per-call DS round-trip was ~60 µs × 1 connection ≈
16 700 op/s ceiling, which matched observed numbers within noise.

Replacing the per-address mutex with a round-robin pool of
`pool_size` lazily-opened sessions (mirroring `Nfs3Client::with_
pool` / `Nfs4Client::v41_with_pool`) lifts the cap to:

| metric | pre-pool | post-pool | delta |
|---|---:|---:|---:|
| pNFS GET op/s | 17 673 | **79 867** | **4.5×** |
| pNFS GET p99 µs | 1 177 | 510 | 2.3× |
| pNFS GET MiB/s | 1 105 | 4 992 | 4.5× |

pNFS GET is now **higher** than NFSv4 inline GET (63 k op/s) —
the DS path pays no MDS-COMPOUND tax, only PUTFH+READ via
SEQUENCE. The pNFS write path is unchanged because pNFS uses
the same `Nfs4Client::v41_with_pool` writer as the `nfs4` row.

### Caveat (RFC 8881 §2.10.4)

A real Linux kernel pNFS client opens ONE session per
`(client_id, DS, principal)` and pipelines via the SEQUENCE
slot table. The harness pool over-provisions vs the kernel
client (16 sessions vs 1 with multiplexing). This is an
upper-bound measurement of what the server's DS read path
can sustain without conflating server cost with kernel slot-
table dynamics. Production pNFS GET via the kernel client may
run lower until the slot table is saturated; the slot-table
multiplexing alternative is documented in `crates/kiseki-
profile/src/protocols.rs::DsSessionPool` doc-block as the
follow-up if we want kernel-realistic measurement.

## A-NG11 gate (≥80 k GET, ≥56 k PUT per node)

| Protocol | PUT (gate ≥56 k) | GET (gate ≥80 k) |
|---|---|---|
| S3 | 36 675 — 65 % | 77 414 — 97 % |
| NFSv3 | 42 915 — 77 % | **108 063 — clears** |
| NFSv4.1 | 48 932 — 87 % | 63 105 — 79 % |
| pNFS | 47 699 — 85 % | 79 867 — **99.8 %** (just under) |
| FUSE | 51 504 — 92 % | **125 606 — clears** |

GET: NFSv3 + FUSE clear; pNFS within 0.2 % of clearing; S3 + NFSv4
in the 79-97 % band. PUT: nothing clears the 56 k gate on this
single host. The gate was sized against expected post-fix numbers
and the dir-index fix already moved everything from 5 k → 50 k —
remaining gap is the next investigation slice.

## Findings

1. **pNFS DS pool is the dominant change** — 4.5× GET lift on
   the pNFS row, no other protocols affected.
2. **NFSv4 inline GET vs pNFS GET**: 63 k vs 80 k. The MDS
   COMPOUND tax (PUTFH + LAYOUTGET on first GET, then SEQUENCE
   + PUTFH + READ on subsequent) is a steady ~21 % gap. The
   first-GET per composition_id pays a layout fetch; subsequent
   GETs on the same composition_id hit `layout_cache` so the
   warmup'd matrix run measures steady-state.
3. **pNFS layout cache is doing its job** — without it we'd see
   2-RPC overhead (LAYOUTGET + GETDEVICEINFO) on every GET.
4. **A-NG11 PUT gate (56 k op/s) is the next unblocked target**
   on this hardware. FUSE is closest at 51 k. The dir-index fix
   exhausted the easy wins on the NFS variants; further PUT lift
   needs flamegraph-driven targeted work on the gateway side.
5. **NFSv3 GET is still the throughput ceiling for any
   single-stream protocol** (108 k op/s) — pNFS's DS-stream path
   gets close (80 k) but pays the layout fetch on first access.

## Captured profiles

- `/tmp/kiseki-prof/cpu-{protocol}-{shape}.svg` — pprof
  flamegraphs (15 SVGs, 200 KB – 1 MB each).
- `/tmp/kiseki-prof/heap-{protocol}-{shape}.json` — dhat heap
  records (15 JSONs).

## Open follow-ups (filed against `docs/performance/README.md`)

- pNFS DS slot-table multiplexing — kernel-realistic alternative
  to the current N-session pool. Documented in
  `DsSessionPool` doc-block.
- A-NG11 PUT gate (56 k op/s) — closest is FUSE at 51 k.
  Next investigation: flamegraph the gateway-side write path on
  the FUSE row, find what's still on the critical path.
- The persistent NFS PUT regression report from the prior
  snapshot (which was actually a O(N²) dir-index degradation)
  is now closed; the new snapshot shows steady ~43-49 k op/s
  across all NFS variants and FUSE.
