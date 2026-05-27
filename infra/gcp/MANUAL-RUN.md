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
```

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

## 4. Drive each protocol BY HAND (distinct payloads, cross-node, check errors)
S3 is path-style, no auth: `http://<ip>:9000/<bucket>/<key>`.
- iperf3 baseline (wire ceiling).
- S3 PUT distinct objects (`head -c <size> /dev/urandom`) to one node; GET from
  ANOTHER node; `cmp` to verify; **capture HTTP codes** (`curl -w "%{http_code}"`) —
  a backgrounded `curl -sf` hides 500s. Allow a few seconds settle before cross-node
  GET (Raft composition replication lag, else false MISMATCH).
- NFS / pNFS / FUSE: one mount at a time.
- **Between every step:** `curl http://<ip>:9090/metrics | grep requests_total` and
  `journalctl -u kiseki-server` on the involved nodes. Those are truth; script
  summaries are not. Halt on first non-2xx spike or error log.

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
