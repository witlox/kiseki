#!/bin/bash
# Kiseki cluster performance benchmark suite.
#
# Tests aggregate cluster throughput across S3, NFS, pNFS, and FUSE paths.
# Uses /cluster/info to discover the Raft leader for write routing.
# Scrapes Prometheus metrics continuously during the test.
#
# Test order: lightweight first, S3 concurrent writes last.
#
# Run from the benchmark controller node.
set -o pipefail

STORAGE_HDD="10.0.0.10 10.0.0.11 10.0.0.12"
STORAGE_FAST="10.0.0.20 10.0.0.21"
ALL_STORAGE="$STORAGE_HDD $STORAGE_FAST"
CLIENTS="10.0.0.30 10.0.0.31 10.0.0.32"
RESULTS="/tmp/kiseki-perf-$(date +%Y%m%d-%H%M%S)"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$RESULTS"

# Concurrency level — tune down for small ctrl nodes (e.g., 4 vCPU).
PAR=${KISEKI_BENCH_PAR:-8}

# GCS bucket for result upload (set by Terraform)
GCS_BUCKET="${KISEKI_PERF_BUCKET:-gs://kiseki-perf-results}"

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$RESULTS/perf.log"; }

# SSH wrapper: uses OS Login user + key from bench env, falls back to root.
# The ctrl startup script registers a key via OS Login and stores the
# username in /etc/kiseki-bench.env as SSH_USER.
source /etc/kiseki-bench.env 2>/dev/null || true
SSH_USER="${SSH_USER:-$(gcloud compute os-login describe-profile --format='value(posixAccounts[0].username)' 2>/dev/null || echo root)}"
SSH_KEY=""
[ -f /root/.ssh/id_ed25519 ] && SSH_KEY="-i /root/.ssh/id_ed25519"
node_ssh() { local H=$1; shift; ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 $SSH_KEY "$SSH_USER@$H" "$@"; }
log "SSH: user=$SSH_USER key=${SSH_KEY:-(default)}"

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║       Kiseki Cluster Performance Benchmark                   ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ Cluster: 5 nodes (3 HDD + 2 Fast), single Raft group        ║"
echo "║ Clients: 3 × n2-standard-8 with 100GB SSD cache             ║"
echo "║ Parallelism: $PAR (set KISEKI_BENCH_PAR to override)"
echo "║ Results: $RESULTS"
echo "╚═══════════════════════════════════════════════════════════════╝"

# ---------------------------------------------------------------------------
# Start background metrics collector
# ---------------------------------------------------------------------------
log "Starting metrics collector (10s interval)"
bash "$SCRIPT_DIR/metrics-collector.sh" "$RESULTS" </dev/null >"$RESULTS/collector.log" 2>&1 &
COLLECTOR_PID=$!

cleanup() {
  log "Stopping metrics collector (pid=$COLLECTOR_PID)"
  kill "$COLLECTOR_PID" 2>/dev/null; wait "$COLLECTOR_PID" 2>/dev/null || true
  bash "$SCRIPT_DIR/metrics-collector.sh" --summarize "$RESULTS" 2>/dev/null || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 0. Cluster health + leader discovery
# ---------------------------------------------------------------------------
log "=== 0. Cluster Health & Leader Discovery ==="

LEADER_S3=""
LEADER_ID=""
for ip in $ALL_STORAGE; do
  STATUS=$(curl -sf "http://$ip:9090/health" 2>/dev/null || echo "DOWN")
  log "  $ip: $STATUS"

  if [ -z "$LEADER_S3" ]; then
    INFO=$(curl -sf "http://$ip:9090/cluster/info" 2>/dev/null || echo "{}")
    CANDIDATE=$(echo "$INFO" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('leader_s3',''))" 2>/dev/null || echo "")
    CANDIDATE_ID=$(echo "$INFO" | python3 -c "import sys,json; d=json.load(sys.stdin); l=d.get('leader_id'); print(l if l else '')" 2>/dev/null || echo "")
    if [ -n "$CANDIDATE" ]; then
      LEADER_S3="http://$CANDIDATE"
      LEADER_ID="$CANDIDATE_ID"
    fi
  fi
done

if [ -z "$LEADER_S3" ]; then
  log "  WARNING: No Raft leader found — falling back to first HDD node"
  LEADER_S3="http://10.0.0.10:9000"
  LEADER_ID="unknown"
fi
LEADER_HOST=$(echo "$LEADER_S3" | sed 's|http://||; s|:.*||')
# NFS runs on the same host as S3 — use the leader's reachable IP,
# not the bind address (0.0.0.0) from cluster/info.
LEADER_NFS_HOST="$LEADER_HOST"

log ""
log "  Raft leader: node $LEADER_ID → S3=$LEADER_S3 NFS=$LEADER_HOST:2049"
log "  All writes routed to leader; reads distributed"
{
  echo "leader_id=$LEADER_ID"
  echo "leader_s3=$LEADER_S3"
  echo "leader_host=$LEADER_HOST"
  echo "timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$RESULTS/cluster-info.txt"

EP="$LEADER_S3"
CLIENT_ARRAY=($CLIENTS)

# ---------------------------------------------------------------------------
# 1. Cluster state snapshot
# ---------------------------------------------------------------------------
log ""
log "=== 1. Cluster State ==="
for ip in $ALL_STORAGE; do
  INFO=$(curl -sf "http://$ip:9090/cluster/info" 2>/dev/null || echo "{}")
  log "  $ip: $INFO" | tee -a "$RESULTS/cluster-state.txt"
done

# ---------------------------------------------------------------------------
# 2. Transport detection
# ---------------------------------------------------------------------------
log ""
log "=== 2. Transport Selection ==="
log "  GCP: no RDMA/RoCEv2 → TCP+TLS fallback" | tee -a "$RESULTS/transport.txt"
for ip in $ALL_STORAGE; do
  RDMA=$(node_ssh "$ip" "ls /sys/class/infiniband/ 2>/dev/null | wc -l" 2>/dev/null || echo "0")
  log "  $ip: IB=$RDMA → TCP" | tee -a "$RESULTS/transport.txt"
done

# ---------------------------------------------------------------------------
# 3. TCP bandwidth between nodes
# ---------------------------------------------------------------------------
log ""
log "=== 3. Inter-Node TCP Bandwidth ==="

node_ssh "$LEADER_HOST" "pkill iperf3 2>/dev/null; iperf3 -s -D 2>/dev/null" 2>/dev/null
sleep 1

for ip in $CLIENTS; do
  BW=$(node_ssh "$ip" "iperf3 -c $LEADER_HOST -t 5 -J 2>/dev/null" 2>/dev/null | \
    python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "N/A")
  log "  $ip (client) → $LEADER_HOST (leader): ${BW} Gbps" | tee -a "$RESULTS/bandwidth.txt"
done

FAST1="10.0.0.20"
node_ssh "$FAST1" "pkill iperf3 2>/dev/null; iperf3 -s -D 2>/dev/null" 2>/dev/null
sleep 1
for ip in $STORAGE_HDD; do
  BW=$(node_ssh "$ip" "iperf3 -c $FAST1 -t 5 -J 2>/dev/null" 2>/dev/null | \
    python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "N/A")
  log "  $ip (HDD) → $FAST1 (Fast): ${BW} Gbps" | tee -a "$RESULTS/bandwidth.txt"
done

# ---------------------------------------------------------------------------
# 4. NFS single-server write (from clients)
# ---------------------------------------------------------------------------
log ""
log "=== 4. NFS Write (3 clients → leader, NFSv4) ==="

PIDS=""
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    mkdir -p /mnt/kiseki-nfs-leader
    umount /mnt/kiseki-nfs-leader 2>/dev/null || true
    mount -t nfs4 -o vers=4.2,rsize=1048576,wsize=1048576 $LEADER_NFS_HOST:/ /mnt/kiseki-nfs-leader 2>/dev/null
    if mountpoint -q /mnt/kiseki-nfs-leader 2>/dev/null; then
      fio --name=nfs-write --directory=/mnt/kiseki-nfs-leader --rw=write --bs=1m \
        --size=128m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  client-$((idx+1)): {bw:.1f} MB/s (NFSv4.2)\")' 2>/dev/null || echo '  client-$((idx+1)): fio parse error'
      rm -f /mnt/kiseki-nfs-leader/nfs-write.* 2>/dev/null
      umount /mnt/kiseki-nfs-leader 2>/dev/null || true
    else
      echo '  client-$((idx+1)): NFS mount failed'
    fi
  " 2>/dev/null | tee -a "$RESULTS/nfs-write.txt" &
  PIDS="$PIDS $!"
done
for pid in $PIDS; do wait $pid 2>/dev/null || true; done

# ---------------------------------------------------------------------------
# 4b. pNFS parallel write (layout delegation)
# ---------------------------------------------------------------------------
log ""
log "=== 4b. pNFS Write (3 clients → cluster, layout delegation) ==="
log "  Note: pNFS layout wire-up pending — expects NFSv4.2 fallback"

PIDS=""
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    mkdir -p /mnt/kiseki-pnfs
    umount /mnt/kiseki-pnfs 2>/dev/null || true
    mount -t nfs4 -o vers=4.2,pnfs,rsize=1048576,wsize=1048576 $LEADER_NFS_HOST:/ /mnt/kiseki-pnfs 2>/dev/null
    if mountpoint -q /mnt/kiseki-pnfs 2>/dev/null; then
      fio --name=pnfs-write --directory=/mnt/kiseki-pnfs --rw=write --bs=1m \
        --size=128m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  client-$((idx+1)) write: {bw:.1f} MB/s\")' 2>/dev/null || echo '  client-$((idx+1)) write: fio parse error'
      fio --name=pnfs-read --directory=/mnt/kiseki-pnfs --rw=read --bs=1m \
        --size=128m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"read\"][\"bw\"]/1024; print(f\"  client-$((idx+1)) read:  {bw:.1f} MB/s\")' 2>/dev/null || echo '  client-$((idx+1)) read: fio parse error'
      echo '--- mountstats ---'
      cat /proc/self/mountstats 2>/dev/null | grep -A5 'kiseki-pnfs' | grep -i layout || echo '  No LAYOUTGET observed (fallback to regular NFS)'
      rm -f /mnt/kiseki-pnfs/pnfs-* 2>/dev/null
      umount /mnt/kiseki-pnfs 2>/dev/null || true
    else
      echo '  client-$((idx+1)): pNFS mount failed (expected if server lacks pNFS support)'
    fi
  " 2>/dev/null | tee -a "$RESULTS/pnfs.txt" &
  PIDS="$PIDS $!"
done
for pid in $PIDS; do wait $pid 2>/dev/null || true; done

if grep -q "LAYOUTGET" "$RESULTS/pnfs.txt" 2>/dev/null; then
  log "  pNFS: layout delegation ACTIVE" | tee -a "$RESULTS/pnfs.txt"
else
  log "  pNFS: no layout delegation — fell back to regular NFSv4.2" | tee -a "$RESULTS/pnfs.txt"
fi

# ---------------------------------------------------------------------------
# 5. FUSE native client benchmark
# ---------------------------------------------------------------------------
log ""
log "=== 5. FUSE Native Client (client-1 → leader) ==="

FIRST_CLIENT="${CLIENT_ARRAY[0]}"
node_ssh "$FIRST_CLIENT" "
  source /etc/kiseki-client.env 2>/dev/null || true

  kiseki-client mount --endpoint $LEADER_HOST:9100 --mountpoint /mnt/kiseki-fuse \
    --cache-mode organic --cache-dir /cache 2>/dev/null &
  FUSE_PID=\$!
  sleep 3

  if mountpoint -q /mnt/kiseki-fuse 2>/dev/null; then
    echo '  FUSE mounted'

    echo '  Sequential write (fio, 4 jobs × 128MB):'
    fio --name=fuse-write --directory=/mnt/kiseki-fuse --rw=write --bs=1m \
      --size=128m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"    Write: {bw:.1f} MB/s\")' 2>/dev/null

    echo '  Sequential read (fio, 4 jobs × 128MB):'
    fio --name=fuse-read --directory=/mnt/kiseki-fuse --rw=read --bs=1m \
      --size=128m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"read\"][\"bw\"]/1024; print(f\"    Read: {bw:.1f} MB/s\")' 2>/dev/null

    echo '  Random 4K read (fio, 4 jobs, 30s):'
    fio --name=fuse-rand --directory=/mnt/kiseki-fuse --rw=randread --bs=4k \
      --size=32m --numjobs=4 --runtime=30 --time_based --group_reporting \
      --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); iops=d[\"jobs\"][0][\"read\"][\"iops\"]; lat=d[\"jobs\"][0][\"read\"][\"lat_ns\"][\"mean\"]/1000; print(f\"    IOPS: {iops:.0f}, avg lat: {lat:.0f} µs\")' 2>/dev/null

    echo '  Metadata: 1000 × mkdir+create:'
    MDSTART=\$(date +%s%N)
    for i in \$(seq 1 1000); do
      mkdir -p /mnt/kiseki-fuse/mdtest-\$i 2>/dev/null
      echo x > /mnt/kiseki-fuse/mdtest-\$i/file 2>/dev/null
    done
    MDEND=\$(date +%s%N)
    MDMS=\$(( (MDEND - MDSTART) / 1000000 ))
    OPS=\$(python3 -c \"print(f'{2000 * 1000 / \$MDMS:.0f}')\" 2>/dev/null || echo 'N/A')
    echo \"    \${MDMS}ms — \${OPS} ops/s\"

    rm -rf /mnt/kiseki-fuse/fuse-* /mnt/kiseki-fuse/mdtest-* 2>/dev/null
    fusermount3 -u /mnt/kiseki-fuse 2>/dev/null || umount /mnt/kiseki-fuse 2>/dev/null
  else
    echo '  FUSE mount failed — skipping'
  fi

  kill \$FUSE_PID 2>/dev/null; wait \$FUSE_PID 2>/dev/null || true
" 2>/dev/null | tee -a "$RESULTS/fuse.txt"

# ---------------------------------------------------------------------------
# 6. S3 PUT latency (from client-1, single-threaded)
# ---------------------------------------------------------------------------
log ""
log "=== 6. S3 PUT Latency (1KB × 100 → leader, from client-1) ==="

FIRST_CLIENT="${CLIENT_ARRAY[0]}"
node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  curl -sf -X PUT \"\$EP/perf-seq\" >/dev/null 2>&1 || true
  LATS=''
  for i in \$(seq 1 100); do
    S=\$(date +%s%N)
    echo 'x' | curl -sf -X PUT \"\$EP/perf-seq/lat-\$i\" --data-binary @- >/dev/null
    E=\$(date +%s%N)
    LATS=\"\$LATS \$(( (E - S) / 1000 ))\"
  done
  echo \"\$LATS\" | tr ' ' '\n' | sort -n | awk '
    { a[NR]=\$1; sum+=\$1 }
    END { n=NR; printf \"  p50: %d µs, p99: %d µs, avg: %d µs, min: %d µs, max: %d µs\n\", a[int(n*.5)], a[int(n*.99)], sum/n, a[1], a[n] }
  '
" 2>/dev/null | tee -a "$RESULTS/s3-latency.txt"

# ---------------------------------------------------------------------------
# 7. S3 sequential write throughput (from client-1)
# ---------------------------------------------------------------------------
log ""
log "=== 7. S3 Sequential Write (client-1 → leader, ${PAR}∥) ==="

node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  PAR=$PAR
  for SIZE in 1 4 16; do
    TOTAL=\$((200 / SIZE > 0 ? 200 / SIZE : 50))
    [ \"\$SIZE\" -ge 16 ] && TOTAL=25
    START=\$(date +%s%N)
    for i in \$(seq 1 \$TOTAL); do
      dd if=/dev/urandom bs=\${SIZE}M count=1 2>/dev/null | \
        curl -sf -X PUT \"\$EP/perf-seq/w\${SIZE}m-\$i\" --data-binary @- >/dev/null &
      [ \$((i % PAR)) -eq 0 ] && wait
    done
    wait
    END=\$(date +%s%N)
    MS=\$(( (END - START) / 1000000 ))
    TOTAL_MB=\$((TOTAL * SIZE))
    MBPS=\$(python3 -c \"print(f'{\$TOTAL_MB * 1000 / \$MS:.1f}')\" 2>/dev/null || echo 'N/A')
    echo \"  \${SIZE}MB × \${TOTAL} (\${PAR}∥): \${MS}ms — \${MBPS} MB/s — total \${TOTAL_MB} MB\"
  done
" 2>/dev/null | tee -a "$RESULTS/s3-write.txt"

# ---------------------------------------------------------------------------
# 8. S3 read throughput (from client-1)
# ---------------------------------------------------------------------------
log ""
log "=== 8. S3 Read Throughput (from client-1, objects from test 7) ==="

node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  PAR=$PAR
  START=\$(date +%s%N)
  for i in \$(seq 1 200); do
    curl -sf \"\$EP/perf-seq/w1m-\$i\" -o /dev/null &
    [ \$((i % PAR)) -eq 0 ] && wait
  done
  wait
  END=\$(date +%s%N)
  MS=\$(( (END - START) / 1000000 ))
  MBPS=\$(python3 -c \"print(f'{200 * 1000 / \$MS:.1f}')\" 2>/dev/null || echo 'N/A')
  echo \"  Read 200 × 1MB (\${PAR}∥): \${MS}ms — \${MBPS} MB/s\"
" 2>/dev/null | tee -a "$RESULTS/s3-read.txt"

# ---------------------------------------------------------------------------
# 9. S3 parallel write throughput (3 clients → leader)
# ---------------------------------------------------------------------------
log ""
log "=== 9. S3 Parallel Write (3 clients → leader, aggregate throughput) ==="

log "  3 clients × 100 objects × 1MB = 300 MB total, ${PAR} concurrent per client"
AGG_START=$(date +%s%N)
PIDS=""
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    EP='$LEADER_S3'
    curl -sf -X PUT \"\$EP/perf-agg\" >/dev/null 2>&1 || true
    for i in \$(seq 1 100); do
      dd if=/dev/urandom bs=1M count=1 2>/dev/null | \
        curl -sf -X PUT \"\$EP/perf-agg/c${idx}-\$i\" --data-binary @- >/dev/null &
      [ \$((i % $PAR)) -eq 0 ] && wait
    done
    wait
  " 2>/dev/null &
  PIDS="$PIDS $!"
done
for pid in $PIDS; do wait $pid 2>/dev/null || true; done
AGG_END=$(date +%s%N)
AGG_MS=$(( (AGG_END - AGG_START) / 1000000 ))
AGG_MBPS=$(python3 -c "print(f'{300 * 1000 / $AGG_MS:.1f}')" 2>/dev/null || echo "N/A")
log "  Aggregate: 300 MB in ${AGG_MS}ms — ${AGG_MBPS} MB/s" | tee -a "$RESULTS/s3-parallel-write.txt"

# ---------------------------------------------------------------------------
# 10. Prometheus metrics snapshot
# ---------------------------------------------------------------------------
log ""
log "=== 10. Prometheus Metrics ==="
for ip in $ALL_STORAGE; do
  REQS=$(curl -sf "http://$ip:9090/metrics" 2>/dev/null | grep "kiseki_gateway_requests_total" | awk '{sum+=$2} END{print sum+0}')
  log "  $ip: gateway_requests=$REQS" | tee -a "$RESULTS/metrics-snapshot.txt"
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
log ""
log "╔═══════════════════════════════════════════════════════════════╗"
log "║                    Benchmark Complete                        ║"
log "╠═══════════════════════════════════════════════════════════════╣"
log "║ Results: $RESULTS"
log "╠═══════════════════════════════════════════════════════════════╣"
log "║ Tests run:                                                    ║"
log "║  1. Cluster state                                             ║"
log "║  2. Transport detection                                       ║"
log "║  3. TCP bandwidth (client→leader, HDD→fast)                   ║"
log "║  4. NFS write (3 clients, NFSv4.2)                            ║"
log "║  4b. pNFS write+read (3 clients, layout delegation)           ║"
log "║  5. FUSE native client (write/read/rand/metadata)             ║"
log "║  6. S3 PUT latency (1KB, p50/p99)                             ║"
log "║  7. S3 sequential write (1/4/16 MB, single client)            ║"
log "║  8. S3 read throughput                                        ║"
log "║  9. S3 parallel write (3 clients, aggregate throughput)       ║"
log "║  10. Prometheus metrics                                       ║"
log "╚═══════════════════════════════════════════════════════════════╝"

# Concatenate all result files into SUMMARY.txt
{
  echo "=== KISEKI PERFORMANCE RESULTS ==="
  echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "Results dir: $RESULTS"
  echo "Parallelism: $PAR"
  echo ""
  for f in cluster-info cluster-state transport bandwidth nfs-write pnfs fuse s3-latency s3-write s3-read s3-parallel-write metrics-snapshot; do
    if [ -f "$RESULTS/$f.txt" ]; then
      echo "--- $f ---"
      cat "$RESULTS/$f.txt"
      echo ""
    fi
  done
  if [ -f "$RESULTS/metrics-summary.txt" ]; then
    echo "--- metrics ---"
    cat "$RESULTS/metrics-summary.txt"
  fi
} > "$RESULTS/SUMMARY.txt"

log ""
log "=== SUMMARY ==="
cat "$RESULTS/SUMMARY.txt"

# Upload to GCS if available
if command -v gsutil &>/dev/null; then
  RUN_ID=$(basename "$RESULTS")
  log "Uploading results to $GCS_BUCKET/$RUN_ID/"
  gsutil -m cp -r "$RESULTS" "$GCS_BUCKET/$RUN_ID/" 2>/dev/null && \
    log "Upload complete: $GCS_BUCKET/$RUN_ID/" || \
    log "GCS upload failed (results still at $RESULTS)"
else
  log "gsutil not found — results only at $RESULTS"
fi

echo "$RESULTS" > /tmp/kiseki-perf-latest
