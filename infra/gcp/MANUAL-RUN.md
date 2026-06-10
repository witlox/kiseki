# GCP perf cluster — manual run procedure

**The matrix run is how we FIND bugs, not just validate** (#97, #99, #102, #107 were
all found this way; the distributed-multi-shard write 500 below was found on
2026-05-27). So drive it deliberately and watch it.

**Golden rule: the suite scripts (`run-perf.sh`, `perf-suite*.sh`) are a REFERENCE,
not a runner.** Never `nohup` them — they launch the whole fio matrix and run away
(a 16 GB `--direct=1` NFS write hits the per-COMMIT wall at ~1.3 MB/s and looks
hung). `cat` them for the exact commands / endpoints / sizes, then execute each step
yourself and check error counters between steps. Halt on the first break.

## 0. Prereqs (once)
```bash
gcloud auth login                       # account with access to project cscs-400112
# Register the SSH key with OS Login so connects are STABLE (no per-connect push,
# which causes intermittent exit-255). The cluster boots with enable-oslogin=TRUE.
gcloud compute os-login ssh-keys add --key-file=.gcp-build/gcp_ssh_key.pub --project=cscs-400112
```
SSH from then on (note: `scp` rejects `--ssh-flag`, use `--scp-flag`):
```bash
gcloud compute ssh <node> --project=cscs-400112 --zone=europe-west1-b \
  --ssh-key-file=.gcp-build/gcp_ssh_key --ssh-flag=-F --ssh-flag=/dev/null
```
Don't loop-hammer SSH (intermittent 255); space calls / retry with backoff. Filter
noise: `grep -vE "post-quantum|store now|openssh|WARNING: connection|Permanently added"`.

## 1. Build + stage binaries (only if `main` moved)
```bash
docker run --rm -v "$PWD":/src \
  -v "$PWD/.gcp-build/cache-cargo":/root/.cargo \
  -v "$PWD/.gcp-build/cache-target":/src/target \
  -v "$PWD/.gcp-build/dist":/out \
  -v "$(command -v protoc)":/usr/local/bin/protoc:ro \
  rockylinux:9 bash /src/.gcp-build/build.sh        # glibc-2.34 floor enforced
git checkout crates/kiseki-crypto/Cargo.toml         # build.sh disables FIPS in-place
cd .gcp-build/dist
for f in kiseki-server kiseki-client; do sha256sum $f-x86_64.tar.gz | awk '{print $1}' > $f-x86_64.tar.gz.sha256; done
gcloud storage cp kiseki-{server,client}-x86_64.tar.gz{,.sha256} \
  gs://kiseki-bench-binaries-pwitlox-20260502/
```

## 2. Spawn (default profile = 6 storage + 3 client, EC-4+2 reachable)
```bash
cd infra/gcp && terraform apply -auto-approve      # perf.auto.tfvars: cscs-400112, europe-west1-b, default
terraform output                                   # node IPs (storage 10.0.0.10-15, clients .30-32)
```
Cluster is ready in ~1-2 min (don't over-wait). Verify:
```bash
# on storage-1:
kiseki-admin status      # Nodes N/N
kiseki-admin shards      # leader map
kiseki-admin capacity    # GH #115 PREFLIGHT GUARD — see below
```

**GH #115 capacity preflight (do this BEFORE measuring).** `kiseki-admin
capacity` (alias `df`) must show each node using its **full provisioned
NVMe** (the `default` profile = ~1.5 TB/node), not ~4 GiB. A ~4 GiB total
means `KISEKI_RAW_DEVICES` did not wire and the node fell back to the
small file — the silent cap that burned the May-2026 run. `phases/00-health.sh`
now HALTs on this automatically (floor `KISEKI_MIN_DEVICE_GB`, default
32 GiB); if you skip the phase scripts, eyeball `kiseki-admin capacity`
yourself. Also note the per-class (fast/bulk/cold) line + dedup ratio —
those are the new observability you watch through the run.

## 3. Create the MULTI-SHARD topology (this is the key step)
Bucket-PUT alone parks every shard's leader on node 1. `shard split` makes an idle
learner. The command that gives **distributed leaders** is:
```bash
# namespace-id MUST be UUIDv5(NAMESPACE_DNS, <bucket>) so S3 routes to it.
# tenant = bootstrap 00000000-0000-0000-0000-000000000001
NSID=$(python3 -c 'import uuid;print(uuid.uuid5(uuid.NAMESPACE_DNS,"msbench"))')
kiseki-admin --endpoint http://10.0.0.10:9090 topology namespace-create \
  "$NSID" --tenant 00000000-0000-0000-0000-000000000001 --shards 6
kiseki-admin shards      # confirm 6 shards, leaders on nodes 1..6 (DISTRIBUTED)
```

**Tiered placement (ADR-045).** To exercise class-aware placement, add a
tier policy at create time:
```bash
kiseki-admin --endpoint http://10.0.0.10:9090 topology namespace-create \
  "$NSID" --tenant 00000000-...-001 --shards 6 \
  --tier fast=10T --tier cold=100T          # spill order: fast → cold
```
Writes then land on the declared class; `kiseki-admin capacity` shows the
per-class fill move. **Caveat:** the `default`/`transport`/`gpu` profiles
are all-NVMe, so fast-vs-cold routing is only *observable* on a
**mixed-media** profile (NVMe + HDD) — not yet a defined profile. On an
all-NVMe cluster the policy is exercised but every tier is `fast`.

## 4. Drive each protocol BY HAND (distinct payloads, cross-node, check errors)

### 4a. Native bench FIRST — and the full PARALLEL 3-client run
The `kiseki-client bench` tool is the headline native-throughput driver. Three
hard-won gotchas:

1. **The bench namespace MUST be created under the bench tenant** — use
   `infra/gcp/benchmarks/setup-shards.sh` (the canonical method), which creates
   `namespace 6658810a-…` (`uuid5(DNS,"kiseki-bench")`) under tenant
   `179e565c-…` (`uuid5(DNS,"kiseki-bench-tenant")`). **Do NOT** hand-create it
   under the bootstrap tenant: writes succeed (no write-authz yet — see IAM
   #117) but `get-heavy` then fails with millions of `tenant mismatch /
   AuthenticationFailed` errors, because the read path resolves the tenant from
   the **namespace registration**, not the request. If you must hand-create,
   pass `--tenant 179e565c-d506-5c59-8f82-7ae6e13f0aff`, or create any namespace
   under that tenant and point the bench at it with `--tenant <id> --namespace <id>`.
2. **Create the namespace with `--shards 18`**, NOT the default 6. The 18-shard
   topology is what every recent A/B in `specs/performance/2026-06-0*.md` uses
   and what the L4 / W12 / W11 baselines compare against. Six shards × six nodes
   = one leader per node and dramatically lower per-shard concurrency density;
   the coalescer barely fills.
3. **Each client → distinct storage IP** (NOT all three pointed at storage-1).
   Per-client endpoint `kiseki://10.0.0.1<c>:9103` where `<c>` ∈ {0, 1, 2}
   addresses storage-{1, 2, 3} respectively. Pointing all three at one node
   pays the forward-to-leader hop on 5/6 of writes and crushes the headline
   number by ~8× (verified 2026-06-02 — operator mistake). For the **full
   6-node ingress spread** (#229 / #226 step 2), pass a comma-separated
   endpoint list with `--connections ≥ 6` instead — see the
   "#226 step 2" section below.

#### Pass A — internal re-baseline (vs L4 / W12 / W11)
Same shape as every recent A/B. 5 min wall-clock, 3 shapes.

```bash
NS=6658810a-1c4d-564c-a888-7564b5e9e576       # bench namespace
TEN=179e565c-d506-5c59-8f82-7ae6e13f0aff      # bench tenant

for shape in put-heavy get-heavy mixed; do
  for c in 1 2 3; do
    gcloud compute ssh "kiseki-client-$c" ... --command="kiseki-client bench \
      --endpoint kiseki://10.0.0.1$((c-1)):9103 \
      --tenant $TEN --namespace $NS \
      --shape $shape --concurrency 32 --object-size 65536 \
      --duration-secs 60 --json" &
  done
  wait
done | tee pass-a.json
```

**L4 baseline (2026-06-02, last clean A/B before #129):** put-heavy
**8 707 op/s**, get-heavy **97 053 op/s**, mixed **13 270 op/s**. If Pass A is
below those by more than 10%, suspect regression and chase the per-PR
metrics — start with `kiseki_intent_coalesce_wait_seconds` and
`kiseki_intent_put_batch_size` on storage-6 (typical mixed-leader). The L4
writeup (`specs/performance/2026-06-02-gcp-l4-mutex-notify-coalescer.md`) is
the diagnostic reference.

#### Pass B/C — 4 KB IOPS (competitive — ADR-042 + ADR-047 design point)
Same loop, `--object-size 4096`. This is the **headline competitive cell**
(`docs/performance/competitive-targets.md`): Ceph 5–20k, Lustre 30–80k
(MDS-bound), VAST 50–150k (without Optane), kiseki target 360k. If aggregate
PUT lands above 20k, we beat Ceph small-file IOPS for the first time. ~5 min.

Pre-flight: verify per-shard `inline_threshold_bytes ≥ 4096`. The recompute
task may bump it to 65536 within the first 60 s after namespace create; check
via `kiseki-admin --endpoint http://10.0.0.10:9090 shards | head -2` (look for
the bumped value). At 4 KB ≤ threshold, #129's `inline_payloads` Raft-replicated
path fires; each replica writes the ciphertext to its local SmallObjectStore
and reads resolve locally — no chunk-fabric round-trip.

#### Pass G (optional) — concurrency sweep (W12 §O-3 prediction)
Same as Pass A but `--concurrency 128`. W12 writeup predicts put 12–15k op/s
from higher per-shard density alone. No code change, env-var free. Free lift
if the prediction holds.

#### Pass D — native bulk (competitive — bytes/sec, not IOPS)
Same 3-client × distinct-leader loop as Pass A, larger objects to drive the
**aggregate bandwidth** competitive cells. Target 5.2 GB/s aggregate write
@ EC-4+2 (vs Ceph ~5 GB/s R-3, Lustre ~5-8 R-1 no-durability, VAST ~3 GB/s
without Optane); 16 GB/s aggregate read (NIC ceiling, Ceph 11-13, Lustre
8-10, VAST 12-14).

```bash
for shape in put-heavy get-heavy; do
  for c in 1 2 3; do
    gcloud compute ssh "kiseki-client-$c" ... --command="kiseki-client bench \
      --endpoint kiseki://10.0.0.1$((c-1)):9103 \
      --tenant $TEN --namespace $NS \
      --shape $shape --concurrency 8 --object-size 1048576 \
      --duration-secs 60 --json" &
  done
  wait
done | tee pass-d.json
# mib_per_sec × 3 clients = aggregate. /1024 for GiB/s.
```

(1 MiB objects, conc=8 per client — drive bandwidth not IOPS. Larger
sizes are NIC-bound and don't tell us more.)

### 4c — only if explicitly testing other surfaces
S3 / FUSE / NFS / pNFS commands kept below for reference, but **the
default perf run is native only.** Add a surface here only when its
specific change has landed and you need to validate it.

S3 is path-style, no auth: `http://<ip>:9000/<bucket>/<key>`. Cross-node
smoke: PUT distinct objects (`head -c <size> /dev/urandom`) to one node;
GET from ANOTHER node; `cmp` to verify; **capture HTTP codes**
(`curl -w "%{http_code}"`) — a backgrounded `curl -sf` hides 500s. Allow
a few seconds settle before cross-node GET (Raft composition replication
lag, else false MISMATCH).
- NFS / pNFS / FUSE: one mount at a time. **Reads need a pre-written file** — the
  2026-05-27 "NFS read inconclusive" was an `fio --rw=read` over a file that was
  never written. Pre-write a small file, then read it back with `--direct=1` so the
  read bypasses the page cache and actually exercises the gateway:
  ```bash
  # On a client node. Three NFS axes: v3, v4(.2), pNFS(4.1). The small
  # READ file keeps the commit-bound pre-write bounded (~256 MB @ the
  # multi-node write rate). O_DIRECT read-back = real gateway read path.

  # --- NFSv3 (gateway serves MOUNT on 2049, no rpcbind; nolock) ---
  mount -t nfs -o vers=3,proto=tcp,port=2049,mountport=2049,mountproto=tcp,nolock,rsize=1048576,wsize=1048576 \
        10.0.0.10:/ /mnt/k-nfs3
  mountpoint -q /mnt/k-nfs3 || echo NFS3-MOUNT-FAILED
  fio --name=w --directory=/mnt/k-nfs3 --rw=write --bs=1m --size=4G --numjobs=4 \
      --direct=1 --runtime=60 --time_based --group_reporting | grep -E 'WRITE:'
  fio --name=rd --directory=/mnt/k-nfs3 --rw=write --bs=1m --size=256m --numjobs=1 \
      --direct=1 --end_fsync=1 >/dev/null
  fio --name=rd --directory=/mnt/k-nfs3 --rw=read  --bs=1m --size=256m --numjobs=1 \
      --direct=1 --group_reporting | grep -E 'READ:'
  rm -f /mnt/k-nfs3/* ; umount /mnt/k-nfs3

  # --- NFSv4.2 write + read ---
  mount -t nfs4 -o vers=4.2,rsize=1048576,wsize=1048576 10.0.0.10:/ /mnt/k-nfs
  mountpoint -q /mnt/k-nfs || { echo MOUNT-FAILED; }
  # write throughput (time-bounded)
  fio --name=w --directory=/mnt/k-nfs --rw=write --bs=1m --size=4G --numjobs=4 \
      --direct=1 --runtime=60 --time_based --group_reporting | grep -E 'WRITE:'
  # read: pre-write then O_DIRECT read-back (real read path, not a cache hit)
  fio --name=rd --directory=/mnt/k-nfs --rw=write --bs=1m --size=256m --numjobs=1 \
      --direct=1 --end_fsync=1 >/dev/null
  fio --name=rd --directory=/mnt/k-nfs --rw=read  --bs=1m --size=256m --numjobs=1 \
      --direct=1 --group_reporting | grep -E 'READ:'
  rm -f /mnt/k-nfs/w* /mnt/k-nfs/rd* ; umount /mnt/k-nfs

  # pNFS Flex Files: mount vers=4.1 so the kernel does LAYOUTGET → DS reads.
  mount -t nfs4 -o vers=4.1,rsize=1048576,wsize=1048576 10.0.0.10:/ /mnt/k-pnfs
  nfsstat -m | grep -iE 'vers=4.1|flexfiles'   # confirm pNFS, not MDS fallback
  fio --name=p --directory=/mnt/k-pnfs --rw=write --bs=1m --size=256m --numjobs=1 \
      --direct=1 --end_fsync=1 >/dev/null
  fio --name=p --directory=/mnt/k-pnfs --rw=read  --bs=1m --size=256m --numjobs=1 \
      --direct=1 --group_reporting | grep -E 'READ:'
  rm -f /mnt/k-pnfs/p* ; umount /mnt/k-pnfs
  ```
  FUSE: after the #124 connect-timeout fix, `kiseki-client mount` prints
  `Connecting native gateway pool …` then `Connected; attaching FUSE session …`.
  If it stalls at *Connecting* the native port (9103) is unreachable/firewalled
  (now fails in 10 s with a clear error, not a silent hang); if it stalls at
  *attaching*, the issue is the libfuse session / mountpoint, not the network.
- **Between every step:** `curl http://<ip>:9090/metrics | grep requests_total`,
  `kiseki-admin capacity` (used/free/% + dedup ratio + per-class — watch used
  climb, dedup hold, and no tier hit Full), and `journalctl -u kiseki-server`
  on the involved nodes. Those are truth; script summaries are not. Halt on the
  first non-2xx spike or error log.
- **ENOSPC signature changed (GH #115):** a full chunk pool now returns a clean
  S3 **507 Insufficient Storage** (native `resource_exhausted`), NOT the old
  `device full → quorum lost → 500`. If you see 507, the pool is full
  (expected), not broken. (Filling 1.5 TB/node for real isn't practical here;
  the `@integration` "Chunk pool full → 507" scenario covers the path.)

### Operational resize drills (ADR-025, IAM-independent)
```bash
kiseki-admin --endpoint http://10.0.0.10:9090 device list
kiseki-admin --endpoint http://10.0.0.10:9090 pool rebalance fast
kiseki-admin --endpoint http://10.0.0.10:9090 pool set-threshold fast --warning 75 --critical 85
kiseki-admin --endpoint http://10.0.0.10:9090 device evacuate <device-id>
```
(Per-tenant **quota** enforcement is deferred on the IAM milestone — see
ADR-045 §D6; these device/pool ops are operator-scoped and need no identity.)

## #212 saturation A/B (small-object / intent fsync arms)

The A/B varies the per-write fsync posture of the small-object store and
the per-shard intent stores. **Both knobs default to the post-#217
group-commit mode — with no arm file, both "arms" measure the same
thing and the A/B is a silent null result** (C-4, 2026-06-10
bench-correctness review). Run at `--object-size 4096` (≤ the inline
threshold) or the knobs under test never fire.

### Arm swap (write the arm file on ALL storage nodes, restart, verify)

The systemd unit (`scripts/setup-raw-storage.sh`) reads
`EnvironmentFile=-/etc/kiseki/perf-arm.env` — optional file, values
override the unit's `Environment=` lines. Swap arms WITHOUT editing the
unit:

```bash
# Arm STRICT (per-write fsync — the pre-#217 posture):
for n in 1 2 3 4 5 6; do
  gcloud compute ssh "kiseki-storage-$n" ... --command="sudo bash -c '
    mkdir -p /etc/kiseki
    printf \"KISEKI_SMALL_OBJECT_FLUSH_INTERVAL_MS=0\nKISEKI_INTENT_FLUSH_INTERVAL_MS=0\n\" > /etc/kiseki/perf-arm.env
    systemctl restart kiseki-server'"
done

# Arm GROUP-COMMIT (100 ms — the post-#217 default, but write it
# EXPLICITLY so the artifact records the arm, not an absence):
#   ... same loop with =100 for both knobs ...
```

**Verify the EFFECTIVE mode on every node — the boot log line is truth,
the env file is only intent:**

```bash
journalctl -u kiseki-server -b | grep -E 'intent store: (strict|group commit)|small object store'
# strict arm  → "intent store: strict per-write fsync (KISEKI_INTENT_FLUSH_INTERVAL_MS=0)"
#               + "small object store: … group_commit=false"
# group arm   → "intent store: group commit (page-cache per write + periodic fsync, #212)"
#               + "small object store: … group_commit=true flush_interval_ms=100"
```

`phases/00-health.sh` snapshots `/etc/kiseki/perf-arm.env` from
storage-1 into the results dir (`ARM-DEFAULT` when absent), so every
run's artifacts carry the arm label. Re-run phase 00 after every swap.

### 3×3 sweep loop (per arm)

Phases 10/11 honor `BENCH_CONCURRENCY` / `BENCH_CONNECTIONS` and embed
the cell label `conc<N>-conn<M>` in every output file, so cells never
overwrite each other:

```bash
cd /opt/kiseki-bench
for conc in 64 128 256; do
  for conn in 1 4 8; do
    # warm pass — DISCARD (see warm-run discipline below)
    BENCH_CONCURRENCY=$conc BENCH_CONNECTIONS=$conn KISEKI_BENCH_OBJECT_SIZE=4096 \
      ./bench run 11-native-parallel || break 2
    # measured pass (same cell label — overwrites the warm pass's files,
    # which is the discard)
    BENCH_CONCURRENCY=$conc BENCH_CONNECTIONS=$conn KISEKI_BENCH_OBJECT_SIZE=4096 \
      ./bench run 11-native-parallel || break 2
  done
done
```

Phase 11 spreads client *i* → storage node *i mod N* native endpoints
by default (all-clients-at-one-leader pays the forward-to-leader hop on
(N-1)/N of writes — the verified ~8× crush). `BENCH_SINGLE_ENDPOINT=1`
restores the old single-leader aim if you specifically want to measure
that. `BENCH_SPREAD_ALL=1` (GH #229 — see the next section) goes the
other way: every client dials ALL storage nodes.

## #226 step 2 — 6-node ingress spread (GH #229)

The #226 occupancy analysis showed writes enter via only HALF the
cluster: 3 client VMs map 1:1 to storage-1/2/3, so storage-4/5/6 do
zero ingress work and per-node ingress occupancy is the cluster
ceiling. Step 2 of the 100k roadmap widens ingress 3→6 by having each
client spread its connections across **all** storage nodes. Expected
with step 1: ~44–50k (the 48k bar sits at the top of this band).

Mechanism: `kiseki-client bench --endpoint` accepts a comma-separated
list; with N `--connections` and E endpoints, connection *i* dials
endpoint *i mod E*. Workers still round-robin ops across the whole
pool, so closed-loop in-flight semantics are unchanged and ops spread
evenly across endpoints. The report's `endpoints` JSON field records
the list, so spread cells are distinguishable in artifacts post-hoc.

Two ways to run the 6-ingress arm:

```bash
# A) Phase 11, opt-in (default stays client-i→storage-i):
BENCH_SPREAD_ALL=1 BENCH_CONNECTIONS=6 BENCH_CONCURRENCY=128 \
  KISEKI_BENCH_OBJECT_SIZE=4096 ./bench run 11-native-parallel

# B) Hand-driven (per client VM; same per-client value on all 3):
EPS=kiseki://10.0.0.10:9103,kiseki://10.0.0.11:9103,kiseki://10.0.0.12:9103,kiseki://10.0.0.13:9103,kiseki://10.0.0.14:9103,kiseki://10.0.0.15:9103
kiseki-client bench --endpoint "$EPS" \
  --tenant $TEN --namespace $NS \
  --shape put-heavy --concurrency 128 --connections 6 \
  --object-size 4096 --duration-secs 60 --json
```

**Connection-count guidance: `--connections` must be ≥ E (here 6) to
dial every endpoint.** With fewer connections only the first
`--connections` endpoints get a socket (conn *i* → endpoint *i mod E*;
the bench warns on stderr). `--connections 1` + a list degenerates to
the FIRST endpoint, deterministically — i.e. exactly the old
single-ingress shape. `BENCH_SPREAD_ALL=1` and `BENCH_SINGLE_ENDPOINT=1`
are mutually exclusive (phase 11 HALTs).

A/B the arms in one sweep: run each cell once with the default
client-i→storage-i mapping (3-ingress) and once with
`BENCH_SPREAD_ALL=1` (6-ingress); the runbook decides per run which
arm is the keeper. Watch per-node `kiseki_gateway_requests_total` —
the 6-ingress arm should show all six storage nodes taking ingress
(forwarding share rises toward a uniform 17/18 with 18 shards;
route-to-leader #135 becomes more valuable, not required).

### Warm-run discipline

The first pass per cell is the ramp: cold connection pools, cold
DecryptCache, fjall journal growth — a known ~2× artifact inside the
measured window. **Discard the first pass per cell** (the loop above
runs each cell twice; the second pass's files are the keepers). The
bench also has `--warmup-secs` for in-process warm-up outside the
measured clock — prefer it for hand-driven runs; the double-pass loop
is the belt-and-braces for everything the in-process warm-up can't
touch (server-side caches survive between passes).

### Halt-on-error contract

Every phase that drives `kiseki-client bench` now (a) propagates the
bench exit code and (b) independently sums the report JSONs' `errors`
field — either non-zero → the phase logs `HALT` and exits 2, and the
`bench` driver stops the run. **Never quote numbers from a run that
halted** — investigate the break first (standing rule: no perf numbers
while ops are 500ing). A halted cell invalidates the whole arm sweep
until the cause is understood, because an error-rate change between
arms means the arms are no longer measuring the same workload.

## 5. Tear down IMMEDIATELY when done (~$13-18/hr)
```bash
cd infra/gcp && terraform destroy -auto-approve
terraform state list | wc -l        # expect 0
```

## Known findings
- **Distributed multi-shard S3 writes 500 (found 2026-05-27, GH #111).** This is the
  known **ADR-042 §4 server-side-leader-forwarding follow-up** — the proxy-to-leader
  design landed for the **native** path (`KISEKI_NATIVE_PROXY_FALLBACK` on by default;
  `@deferred-feature` scenarios in `native-gateway.feature`), but **S3 + NFS ingress
  aren't wired into it**. (Native distributed-multi-shard is unverified — only S3 was
  tested here.) With `namespace-create --shards N` spreading leaders, an S3 PUT routing
  to a shard led by a *remote* node fails:
  `raft_shard_store: append_chunk_and_delta: shard append failed error=leader
  unavailable: ShardId(...)` → gateway rolls back composition → HTTP 500. The gateway
  appends only to shards it leads locally and does **not** forward/redirect the write
  to the remote shard leader. ~1/6 of writes (those routing to the local-leader
  shard) succeed; the rest 500. Invisible when all leaders sit on node 1.
- NFS/pNFS/FUSE writes are throughput-bound by per-COMMIT composition (~1.3 MB/s on
  the multi-node path) — correctness OK, but the suite's 4 GB `--direct=1` fio jobs
  won't finish in a sane window. Use small sizes for a quick functional pass.
