# 2026-06-10 — Perf bench / eval / proto-matrix correctness review

Adversarial review (3 reviewers + per-finding verification, 32 agents)
of the measurement stack ahead of the #212 saturation A/B. 29 findings;
25 confirmed, 4 weakened, 0 refuted.

**Verdict: the stack as it stands would have produced a silent null
result for the #212 A/B.** The bench's bones are sound (closed-loop
workers, real single-connection multiplexing via request_id, warmup
outside the clock, success-only throughput, unique composition names +
fresh idempotency keys per PUT) — but the defects below compound into
"both arms measure the same thing and nobody notices".

## Critical (must fix before any GCP spend)

### C-1 PUT payload dedup trap (bench.rs)
Seeds are pure functions of (worker_id, op_index): `bench.rs:395`
`(worker_id << 40) ^ n`, warmup `bench.rs:347` `(1<<63) | i`. No
run-nonce, no client-node discriminator. Consequences:
- 3-client sweep: all three nodes emit IDENTICAL payload streams →
  ~2/3 of PUTs dedup.
- Any re-run on a non-wiped cluster (the second A/B arm!) → ~100%
  dedup.
- Verified mechanism at the 4 KiB inline shape: the skip happens
  **inside `SmallObjectStore::put`** (`small_object_store.rs:159-166`
  `if existed { return Ok(false) }` — no commit) — i.e. dedup skips
  **exactly the fsync the #217 A/B varies**. PUTs still "succeed"
  (fresh `bench-{uuid}` names → composition + Raft commit), so ops/s
  stays plausible. Both arms would read identical.
Fix: seed = hash(run_nonce_uuid, client_node_id, worker_id, n); put
the nonce in the report JSON; optionally assert server dedup-fraction
< few % post-run.

### C-2 No halt-on-functional-break
`kiseki-client bench` counts errors but exits 0 at 100% error rate
(put-heavy shape); gcp-mode phase branches never check rc or the
errors field. Violates the standing rule (never quote numbers while
ops 500). Fix: error-rate gate in bench (non-zero exit), `errors>0 →
halt` in every gcp phase branch.

### C-3 Saturation sweep undrivable
No phase script or MANUAL-RUN command passes `--connections` (pinned
to default 1), and the report JSON omits the connections axis
entirely (`BenchReport`, bench.rs:124-150) — the 9 sweep cells would
be indistinguishable. GH #138 risk at conc≥48 over conn=1 is
*probably* stale post-slice-4 (PR #204) but unproven at 256.
Fix: plumb `--connections` through phases 10/11 + record both axes +
per-client JSON per cell.

### C-4 No A/B arm control or provenance
The GCP boot template (`setup-raw-storage.sh` systemd unit) sets
neither `KISEKI_SMALL_OBJECT_FLUSH_INTERVAL_MS` nor
`KISEKI_INTENT_FLUSH_INTERVAL_MS` → both arms silently run the
post-#217 group-commit default. The strict arm cannot be booted, and
artifacts don't record the arm (only the server boot log line does).
Fix: arm env passthrough in the boot template + arm label in bench
JSON + 00-health assertion of the effective mode.

### C-5 Local kiseki-profile cannot host this A/B
(a) Both spawn paths `env_clear()` with a pre-#217 allowlist — the
knobs never reach the spawned server; (b) data dirs are
`tempfile::tempdir()` on tmpfs → fsync is ~free, the variable under
test vanishes locally (TMPDIR override exists but is undocumented/
unused). Treat the local matrix as unusable for fsync A/Bs until
fixed.

## Major (fix or consciously accept)

- **No warm-up discipline anywhere** (bench has no discarded warm
  pass for put-heavy; no phase/runbook step). The known cold-ramp
  ~2× artifact lands inside the measured window. Combined with C-1:
  a warm-up pass would *worsen* dedup on the measured pass.
- **GETs verify nothing** — 0-byte/truncated responses count as
  success; MiB/s fabricated from object_size. #127-class regressions
  invisible.
- **GET working set is degenerate** — 256 warmup objects × 4 KiB =
  1 MiB, fully cache-resident; GET numbers measure the DecryptCache.
- **Phase 11 aims all 3 clients at ONE leader endpoint**
  (`perf-common.sh` leader_endpoints) — the documented ~8× crush;
  clients must spread across distinct leader/data endpoints.
- **Dedup-degenerate payloads in S3-latency + FUSE phases** —
  constant/all-zero bytes (`dd if=/dev/zero`, fixed python payload):
  measure the refcount/composition path, not the write path.
- **Phase 30 (gcp mode) runs bench on bench-ctrl which never gets
  kiseki-client installed** (`setup-bench-ctrl.sh` installs only
  kiseki-admin) → halts; hand-installing would make S3 numbers
  ctrl-VM-local anyway.
- **Inline-threshold recompute can silently move 4 KiB OFF the
  inline path mid-run** — per-shard recompute task may bump 4096 →
  65536 within ~60 s of namespace create (and
  `KISEKI_INLINE_THRESHOLD_RECOMPUTE_S=0` silently falls back to 60 —
  can't be disabled by env). Must pin/verify the threshold for the
  run or the measured path changes under us.
- **S3 suite MB/s assumes success** (backgrounded `curl -sf`,
  assumed-total-bytes / elapsed).
- **kiseki-profile 'fuse' cells aren't FUSE** (no kernel mount; native
  TCP path + inode bookkeeping); in-process drivers don't wire a
  small store at all (different code path than production inline).

## Minor (hygiene)

- `stats.rs` pct(): `idx = floor(n*p/100)` over-reads the tail by one
  rank (p99 of 100 samples returns the max).
- kiseki-profile Mixed shape is ~75/25, not the documented 70/30;
  elapsed computation is dead code (always nominal duration).
- bench latency window includes `make_payload` + a Vec copy
  (negligible at 4 KiB); post-provision wait is a blind 500 ms sleep.
- Phase 11 truncates per-client JSONs on re-run (sweep cells
  overwrite each other).

## What was verified as CORRECT

- 4096-byte objects DO take the inline path (gate `piece_len <=
  threshold` inclusive; `ShardConfig::default().inline_threshold_bytes
  = 4096`) — the right #212 target, while the threshold holds (see
  recompute caveat above).
- `--concurrency` = N closed-loop workers (true fixed in-flight);
  one connection genuinely carries N in-flight (slice-4 request_id
  demux). Throughput = successful ops / single wall clock.
- 00-health.sh #115 capacity HALT + leader-agreement gates are real.
- NFS phase pre-write + `--direct=1` read-back is the correct shape.
- kiseki-profile *measured-PUT* payload stamping is dedup-safe (the
  trap is only in warmup objects there).

Full transcript: workflow `wmnvjed13` (32 agents), 2026-06-10.
