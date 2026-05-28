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
The `kiseki-client bench` tool is the headline native-throughput driver. Two
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
2. **3 client nodes = parallel distributed load** (the whole point of the 3
   clients). Run the bench on all three concurrently against the cluster:
   ```bash
   for c in kiseki-client-1 kiseki-client-2 kiseki-client-3; do
     gcloud compute ssh "$c" ... --command="kiseki-client bench \
       --endpoint kiseki://10.0.0.10:9103 --tenant 179e565c-... --namespace <bench-ns> \
       --shape get-heavy --concurrency 16 --object-size 65536 --duration-secs 30 --json" &
   done; wait        # then sum ops_per_sec + mib_per_sec across the 3 JSON lines
   ```
   Reference (2026-05-27, default profile, 64 KB): **get-heavy ≈ 10.6k ops/s ·
   662 MiB/s aggregate** (3×~3.5k), p99 ~90 ms, 0 errors. **put-heavy ≈ 264 ops/s
   · 16.5 MiB/s aggregate** — commit-bound on the distributed multi-shard write
   path (per-write composition + forward + Raft commit); writes are correct but
   throughput is low, a known multi-node write characteristic.

### 4b. The other protocols
S3 is path-style, no auth: `http://<ip>:9000/<bucket>/<key>`.
- iperf3 baseline (wire ceiling).
- S3 PUT distinct objects (`head -c <size> /dev/urandom`) to one node; GET from
  ANOTHER node; `cmp` to verify; **capture HTTP codes** (`curl -w "%{http_code}"`) —
  a backgrounded `curl -sf` hides 500s. Allow a few seconds settle before cross-node
  GET (Raft composition replication lag, else false MISMATCH).
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
