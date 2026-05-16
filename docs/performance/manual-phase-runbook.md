# Manual-phase perf-run runbook

For when you don't trust `perf-suite.sh` to halt cleanly and want to
capture diagnostics if any phase wedges. Mirrors the suite script
phase-for-phase but runs each block on its own so a single wedge
doesn't poison the downstream measurements.

## Why

The 2026-05-15 evening GCP run wedged at Phase 4 (+205 s). The suite
script's trap-on-EXIT preserved phase-0 through phase-3 results, but
phases 4b–10 never ran and the in-flight phase-4 state was lost.
Running phases standalone makes that recovery shape explicit and adds
per-phase log capture.

## Pre-flight checklist (do all three before any phase)

- **Binaries are current**: `gs://kiseki-bench-binaries-pwitlox-20260502/`
  tarballs match the commit you're testing. Build with
  `.gcp-build/build.sh` inside rockylinux:9; if the build host can't
  reach `objects.githubusercontent.com` (sandbox/firewall), bind-mount
  a host-provided protoc at `/usr/local/bin/protoc:ro` — the script
  honors a pre-staged protoc and skips the download:

      docker run --rm \
        -v $(pwd):/src \
        -v $(pwd)/.gcp-build/cache-target:/src/target \
        -v $(pwd)/.gcp-build/cache-cargo:/root/.cargo \
        -v $(pwd)/.gcp-build/dist:/out \
        -v $(which protoc):/usr/local/bin/protoc:ro \
        -w /src rockylinux:9 \
        bash /src/.gcp-build/build.sh

  Upload with `gcloud storage cp .gcp-build/dist/kiseki-{server,client}-x86_64.tar.gz gs://kiseki-bench-binaries-pwitlox-20260502/`.
- **Benchmark scripts are staged**: `bash infra/gcp/scripts/stage-benchmarks.sh`
  packages `infra/gcp/benchmarks/` into a tarball and uploads it
  alongside the binaries. `setup-bench-ctrl.sh` fetches it on boot
  and lands `bench`, `perf-common.sh`, `phases/`, etc. at
  `/opt/kiseki-bench/` (#54). Without this, the bench-ctrl boots with
  an empty results dir and the operator has to ssh-tee scripts up.
- **Local wire smoke passes**: `docker compose -f docker-compose.3node.yml up -d --wait`
  followed by a kernel `mount -t nfs4 -o vers=4.2 kiseki-node1:/ /mnt/test`
  from the `kiseki-pnfs-client:test` image. If mount fails, the wire
  fixes broke the kernel path — stop, don't apply terraform.
- **Cluster is up + healthy**: `terraform -chdir=infra/gcp apply` ;
  `gcloud compute ssh bench-ctrl -- 'curl -sf http://<storage-1>:9090/cluster/info'`
  returns a leader + 3 shards.

## Running one phase at a time

SSH to `bench-ctrl`, then `cd` into the benchmarks dir and source the
shared helpers:

```bash
gcloud compute ssh bench-ctrl --zone=europe-west1-b
cd /opt/kiseki-bench
source ./perf-common.sh
discover_leader     # populates LEADER_*, ALL_STORAGE, CLIENT_ARRAY
echo "leader=$LEADER_HOST"
mkdir -p $RESULTS
```

After each phase block below, inspect its result file before continuing:

```bash
tail -20 "$RESULTS/<phase>.txt"
```

If a phase hangs for >5 min beyond its expected duration, **kill it
and capture diagnostics** (see "When something wedges" below)
before deciding whether to continue.

### Phase 0 — health + leader (free; do first)

```bash
source ./perf-common.sh && discover_leader
echo "Leader: node $LEADER_ID → S3=$LEADER_S3 NFS=$LEADER_HOST:2049"
```

Expected duration: < 5 s. If discover_leader fails, the cluster's
not actually healthy — terraform / boot script problem, not a kiseki
problem.

### Phase 1 — cluster state snapshot

```bash
for ip in $ALL_STORAGE; do
  curl -sf "http://$ip:9090/cluster/info" | tee -a "$RESULTS/cluster-state.txt"
done
```

Expected: every node reports the same leader_id and same 3 shards.
Drift means a follower hasn't fully joined.

### Phase 3 — inter-node TCP bandwidth (iperf3 baseline)

```bash
# (Phase 2 is informational, no measurement.)
LEADER_IP=$(echo "$ALL_STORAGE" | tr ' ' '\n' | sed -n "${LEADER_ID}p")
for cip in "${CLIENT_ARRAY[@]}"; do
  node_ssh "$cip" "iperf3 -c $LEADER_IP -t 30 -P 4 -J" \
    | python3 -c 'import sys,json; d=json.load(sys.stdin); bps=d["end"]["sum_received"]["bits_per_second"]/1e9; print(f"  client→leader: {bps:.1f} Gbps")' \
    | tee -a "$RESULTS/bandwidth.txt"
done
```

Expected: ~45 Gbps on Tier_1, ~10 Gbps elsewhere. **If iperf3 is below
expected, stop — kiseki throughput will be wire-bound and no other
phase is interpretable.**

### Phase 4 — NFSv4 write (this is the F-1 risk phase)

The 2026-05-15 wedge happened here. Before running, take a baseline
hydrator-backlog snapshot from the leader:

```bash
curl -sf "http://$LEADER_HOST:9090/metrics" | \
  grep -E 'hydrator|composition_pending|raft_lag' > "$RESULTS/phase4-pre.txt"
```

Then run the parallel write. **Cap fio with `--runtime` and
`--time_based` so a single client's slow disk doesn't extend the phase
indefinitely.** Stock perf-suite uses an open-ended `--size`; that's
what let the 2026-05-15 wedge hide.

```bash
PIDS=""
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  ( node_ssh "$CIP" "
    mkdir -p /mnt/kiseki-nfs-leader && umount /mnt/kiseki-nfs-leader 2>/dev/null
    mount -t nfs4 -o vers=4.2,rsize=1048576,wsize=1048576 $LEADER_HOST:/ /mnt/kiseki-nfs-leader
    fio --name=nfs-w --directory=/mnt/kiseki-nfs-leader --rw=write --bs=1m \
      --size=8G --numjobs=4 --direct=1 --runtime=120 --time_based \
      --group_reporting --output-format=json 2>/dev/null \
      | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[\"jobs\"][0][\"write\"][\"bw\"]/1024, \"MB/s\")'
    umount /mnt/kiseki-nfs-leader
  " | tee -a "$RESULTS/nfs-write.txt" ) &
  PIDS="$PIDS $!"
done
for pid in $PIDS; do wait $pid; done
```

After it returns, snapshot hydrator state again:

```bash
curl -sf "http://$LEADER_HOST:9090/metrics" | \
  grep -E 'hydrator|composition_pending|raft_lag' > "$RESULTS/phase4-post.txt"
diff "$RESULTS/phase4-pre.txt" "$RESULTS/phase4-post.txt"
```

If the post snapshot shows hydrator backlog (pending compositions ↑,
apply_duration p99 ↑ past 1 s), **F-1 reproduced** — file finding,
skip downstream phases that drive sustained writes (4b, 9, 9b).
FUSE / S3 latency phases (5, 6, 8) are still safe to run; they're
not write-saturating.

### Phase 4b — pNFS write+read

Skip if Phase 4 surfaced F-1. Otherwise mirror Phase 4 with
`vers=4.1` and verify LAYOUTGET via mountstats:

```bash
# (same fan-out shape as Phase 4, with vers=4.1)
# After write+read, check kernel saw a layout:
node_ssh "${CLIENT_ARRAY[0]}" 'grep -A5 "/mnt/kiseki-pnfs" /proc/self/mountstats | grep -i layout || echo "NO LAYOUT"' \
  | tee -a "$RESULTS/pnfs-layout.txt"
```

If "NO LAYOUT", pNFS fell back to MDS — record that and continue.

### Phase 5 — FUSE single-client (write/read/random/metadata)

Single-client, ~5 min wall clock. F-1 risk minimal (single writer).
Lift from perf-suite.sh lines 168–222.

### Phase 6 — S3 PUT latency (1 KB × 100)

```bash
for i in $(seq 1 100); do
  curl -sS -w '%{time_total}\n' -o /dev/null -X PUT \
    -H "Content-Length: 1024" --data-binary "$(head -c 1024 /dev/urandom | base64)" \
    "http://$LEADER_S3/latency-test/obj-$i"
done | python3 -c 'import sys; lats=sorted(float(l)*1000 for l in sys.stdin if l.strip()); print(f"p50={lats[len(lats)//2]:.1f}ms p99={lats[int(len(lats)*0.99)]:.1f}ms")' \
  | tee -a "$RESULTS/s3-latency.txt"
```

### Phase 7 — S3 sequential write (single client, sweep object size)

Single-client. Lift from perf-suite.sh lines 248–268.

### Phase 8 — S3 read throughput

Reads back the objects Phase 7 wrote. Lift from perf-suite.sh
lines 274–289.

### Phase 9 — S3 parallel write (single namespace fan-out)

Same F-1 risk shape as Phase 4. If Phase 4 surfaced F-1, **skip**
Phase 9 — we already know the answer.

### Phase 9b — S3 parallel write (per-client namespace)

Same F-1 risk. Skip if Phase 9 wedged.

### Phase 10 — Prometheus metrics snapshot

Always safe to run. Captures end-state hydrator + raft + chunk-store
counters for the postmortem.

## When something wedges

If a phase exceeds its expected duration by 2×, kill it and capture
diagnostics BEFORE moving on:

```bash
# 1. Identify the hung fio / kiseki process
for cip in "${CLIENT_ARRAY[@]}"; do
  node_ssh "$cip" "pgrep -fa 'fio|kiseki' | head -5"
done

# 2. Capture leader stack trace (gdb attached → log) — last resort
gcloud compute ssh storage-1 --zone=europe-west1-b -- \
  'sudo gdb -batch -ex "thread apply all bt" -p $(pidof kiseki-server)' \
  > "$RESULTS/leader-stacks.txt"

# 3. Capture metrics snapshot (often reveals the queue)
curl -sf "http://$LEADER_HOST:9090/metrics" > "$RESULTS/wedge-metrics.txt"

# 4. Capture per-node syslog (in case OOM-killer fired)
for ip in $ALL_STORAGE; do
  node_ssh "$ip" 'sudo dmesg -T | tail -50' > "$RESULTS/syslog-$ip.txt"
done

# 5. THEN unstuck the cluster (umount stale NFS, kill fio, etc.)
for cip in "${CLIENT_ARRAY[@]}"; do
  node_ssh "$cip" 'umount -f /mnt/kiseki-* 2>/dev/null; pkill -9 fio'
done
```

Upload the wedge artifacts to the results bucket and file an issue
with the metrics snapshot + stack trace before re-running.

## Teardown

```bash
gcloud storage cp -r $RESULTS gs://$KISEKI_PERF_BUCKET/manual-$(date +%Y%m%d-%H%M)/
terraform -chdir=infra/gcp destroy -auto-approve
```
