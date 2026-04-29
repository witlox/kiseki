#!/bin/bash
# Kiseki "gpu" profile — ML training scenario suite.
#
# Targets the cluster shape provisioned by var.profile = "gpu":
#   * 3 × c3-standard-44 storage nodes, 4 × local NVMe each
#   * 2 × a2-highgpu-1g GPU clients (1 × NVIDIA A100 each)
#
# What we want to know:
#   1. cuFile / GPUDirect Storage path is reachable end-to-end
#   2. Bulk dataset stage-in throughput (Slurm prolog scenario)
#   3. Training-loop simulation: random-batch reads with shuffle, repeated
#      across epochs — measures iteration latency p50/p99 and
#      epoch-1-vs-epoch-N delta (= L2 cache hit rate)
#   4. Comparison vs local NVMe baseline so the storage tax is visible

source "$(dirname "$0")/perf-common.sh"

trap 'stop_metrics; write_summary "GPU-PROFILE" cluster-info gds-check stage-in train-loop epoch-deltas local-baseline metrics-snapshot; upload_results' EXIT

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║       Kiseki Performance — GPU / ML Training Profile         ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ Cluster: 3 × c3-standard-44 (NVMe), Tier_1 50 Gbps           ║"
echo "║ Clients: 2 × a2-highgpu-1g (1 × A100 each)                   ║"
echo "║ Goal: cuFile path + training-loop simulation + cache reuse   ║"
echo "║ Results: $RESULTS"
echo "╚═══════════════════════════════════════════════════════════════╝"

start_metrics

# ---------------------------------------------------------------------------
# 0. Cluster + leader
# ---------------------------------------------------------------------------
log "=== 0. Cluster Health & Leader Discovery ==="
discover_leader
log "  Raft leader: node $LEADER_ID → S3=$LEADER_S3"

# ---------------------------------------------------------------------------
# 1. GDS / cuFile environment check
# ---------------------------------------------------------------------------
log ""
log "=== 1. GPUDirect Storage Path Verification ==="
for idx in 0 1; do
  CIP="${CLIENT_ARRAY[$idx]}"
  log "  --- client-$((idx+1)) ($CIP) ---" | tee -a "$RESULTS/gds-check.txt"
  node_ssh "$CIP" "
    echo '  GPU:'
    nvidia-smi -L 2>/dev/null | sed 's/^/    /' || echo '    nvidia-smi missing'
    echo '  CUDA driver:'
    nvidia-smi 2>/dev/null | grep 'CUDA Version' | sed 's/^/    /' || echo '    not detected'
    echo '  nvidia_fs kernel module:'
    if lsmod | grep -q nvidia_fs; then
      echo '    LOADED'
    else
      echo '    NOT LOADED — cuFile will fall back to bounce buffers'
    fi
    echo '  /etc/cufile.json:'
    if [ -f /etc/cufile.json ]; then
      jq -r '.properties // {}' /etc/cufile.json 2>/dev/null | sed 's/^/    /' || \
        echo '    present (jq unavailable)'
    else
      echo '    absent'
    fi
    echo '  gdscheck:'
    /usr/local/cuda/gds/tools/gdscheck.py -p 2>/dev/null | head -20 | sed 's/^/    /' || \
      echo '    gdscheck not found'
  " 2>/dev/null | tee -a "$RESULTS/gds-check.txt"
done

# ---------------------------------------------------------------------------
# 2. Bulk dataset stage-in (Slurm prolog scenario)
# ---------------------------------------------------------------------------
log ""
log "=== 2. Bulk Dataset Stage-In (10 GB synthetic dataset → client cache) ==="
log "  Simulates a Slurm prolog: pull dataset to client-local NVMe before"
log "  the training job starts."

# Seed: write 10 GB to the cluster from client-1 via S3.
FIRST_CLIENT="${CLIENT_ARRAY[0]}"
node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  curl -sf -X PUT \"\$EP/training-set\" >/dev/null 2>&1 || true
  if ! curl -sIf \"\$EP/training-set/shard-0\" >/dev/null 2>&1; then
    echo '  Seeding 10 × 1 GB shards (one-time)'
    for i in \$(seq 0 9); do
      dd if=/dev/urandom bs=1M count=1024 2>/dev/null | \
        curl -sf -X PUT \"\$EP/training-set/shard-\$i\" --data-binary @- >/dev/null
    done
    echo '  Seeded'
  else
    echo '  Dataset already present'
  fi
" 2>/dev/null | tee -a "$RESULTS/stage-in.txt"

# Stage-in: pull to client cache, measure throughput.
for idx in 0 1; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    EP='$LEADER_S3'
    rm -rf /cache/staged && mkdir -p /cache/staged
    sync && echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
    START=\$(date +%s%N)
    pids=''
    for i in \$(seq 0 9); do
      curl -sf \"\$EP/training-set/shard-\$i\" -o /cache/staged/shard-\$i &
      pids=\"\$pids \$!\"
    done
    for p in \$pids; do wait \$p 2>/dev/null; done
    END=\$(date +%s%N)
    MS=\$(( (END - START) / 1000000 ))
    GBPS=\$(python3 -c \"print(f'{10 * 8 * 1024 / \$MS:.2f}')\")
    MBPS=\$(python3 -c \"print(f'{10 * 1024 * 1000 / \$MS:.0f}')\")
    echo \"  client-$((idx+1)): 10 GB in \${MS}ms — \${MBPS} MB/s (\${GBPS} Gbps)\"
  " 2>/dev/null | tee -a "$RESULTS/stage-in.txt"
done

# ---------------------------------------------------------------------------
# 3. Training-loop simulation (random batch reads, kiseki FUSE)
# ---------------------------------------------------------------------------
log ""
log "=== 3. Training Loop Simulation (random 256 KB batches, FUSE) ==="
log "  Approximates a DataLoader pulling shuffled minibatches: 10 000 random"
log "  reads × 256 KB across the staged 10 GB dataset, --direct=1."

for idx in 0 1; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    source /etc/kiseki-client.env 2>/dev/null || true
    umount /mnt/kiseki-fuse 2>/dev/null || true
    kiseki-client mount --endpoint $LEADER_HOST:9100 --mountpoint /mnt/kiseki-fuse \
      --cache-mode organic --cache-dir /cache 2>/dev/null &
    FUSE_PID=\$!
    sleep 3
    if mountpoint -q /mnt/kiseki-fuse; then
      sync && echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
      fio --name=trainloop --directory=/mnt/kiseki-fuse --rw=randread --bs=256k \
        --size=2G --numjobs=8 --direct=1 --runtime=60 --time_based --ramp_time=10 \
        --group_reporting --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json
d=json.load(sys.stdin)
j=d[\"jobs\"][0][\"read\"]
iops=j[\"iops\"]
bw=j[\"bw\"]/1024
p50=j[\"clat_ns\"][\"percentile\"][\"50.000000\"]/1000
p99=j[\"clat_ns\"][\"percentile\"][\"99.000000\"]/1000
print(f\"  client-$((idx+1)): {iops:.0f} IOPS, {bw:.0f} MB/s, p50={p50:.0f}µs p99={p99:.0f}µs\")' 2>/dev/null
      fusermount3 -u /mnt/kiseki-fuse 2>/dev/null || umount /mnt/kiseki-fuse 2>/dev/null
    else
      echo '  client-$((idx+1)): FUSE mount failed'
    fi
    kill \$FUSE_PID 2>/dev/null; wait \$FUSE_PID 2>/dev/null || true
  " 2>/dev/null | tee -a "$RESULTS/train-loop.txt"
done

# ---------------------------------------------------------------------------
# 4. Epoch repeat — same workload run 3 times to measure cache effect
# ---------------------------------------------------------------------------
log ""
log "=== 4. Epoch Repeat (3 epochs, measures L2 cache hit rate) ==="
log "  Epoch 1 = cold; epoch 2/3 should hit /cache (organic mode)."

CIP="${CLIENT_ARRAY[0]}"
node_ssh "$CIP" "
  source /etc/kiseki-client.env 2>/dev/null || true
  umount /mnt/kiseki-fuse 2>/dev/null || true
  rm -rf /cache/* 2>/dev/null
  kiseki-client mount --endpoint $LEADER_HOST:9100 --mountpoint /mnt/kiseki-fuse \
    --cache-mode organic --cache-dir /cache 2>/dev/null &
  FUSE_PID=\$!
  sleep 3
  if mountpoint -q /mnt/kiseki-fuse; then
    for EPOCH in 1 2 3; do
      sync && echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
      fio --name=epoch-\$EPOCH --directory=/mnt/kiseki-fuse --rw=randread --bs=256k \
        --size=2G --numjobs=8 --direct=1 --runtime=30 --time_based --ramp_time=5 \
        --group_reporting --output-format=json 2>/dev/null | \
        python3 -c \"
import sys, json
d = json.load(sys.stdin)
j = d['jobs'][0]['read']
print(f'  epoch \$EPOCH: {j[\\\"iops\\\"]:.0f} IOPS, {j[\\\"bw\\\"]/1024:.0f} MB/s, p99={j[\\\"clat_ns\\\"][\\\"percentile\\\"][\\\"99.000000\\\"]/1000:.0f}µs\"\$(curl -sf http://$LEADER_HOST:9090/metrics 2>/dev/null | grep -E 'kiseki_cache_(hit|miss)_total' | awk '{print \\\" \\\"\\$1\\\"=\\\"\\$2}' | tr '\\n' ' ')')
\" 2>/dev/null
    done
    fusermount3 -u /mnt/kiseki-fuse 2>/dev/null || umount /mnt/kiseki-fuse 2>/dev/null
  fi
  kill \$FUSE_PID 2>/dev/null; wait \$FUSE_PID 2>/dev/null || true
" 2>/dev/null | tee -a "$RESULTS/epoch-deltas.txt"

# ---------------------------------------------------------------------------
# 5. Local-NVMe baseline (the "free" tax floor)
# ---------------------------------------------------------------------------
log ""
log "=== 5. Local-NVMe Baseline (same workload on staged data, no kiseki) ==="
log "  Reads /cache/staged/* directly. Anything kiseki adds shows up as the"
log "  delta vs this number."

CIP="${CLIENT_ARRAY[0]}"
node_ssh "$CIP" "
  if [ ! -d /cache/staged ]; then
    echo '  no staged dataset — run test 2 first'
  else
    sync && echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
    fio --name=local-baseline --directory=/cache/staged --rw=randread --bs=256k \
      --size=2G --numjobs=8 --direct=1 --runtime=30 --time_based --ramp_time=5 \
      --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json
d=json.load(sys.stdin)
j=d[\"jobs\"][0][\"read\"]
print(f\"  local PD-SSD: {j[\\\"iops\\\"]:.0f} IOPS, {j[\\\"bw\\\"]/1024:.0f} MB/s, p99={j[\\\"clat_ns\\\"][\\\"percentile\\\"][\\\"99.000000\\\"]/1000:.0f}µs\")' 2>/dev/null
  fi
" 2>/dev/null | tee -a "$RESULTS/local-baseline.txt"

# ---------------------------------------------------------------------------
# 6. Metrics snapshot
# ---------------------------------------------------------------------------
log ""
log "=== 6. Prometheus Metrics Snapshot ==="
for ip in $ALL_STORAGE; do
  REQS=$(curl -sf "http://$ip:9090/metrics" 2>/dev/null | grep "kiseki_gateway_requests_total" | awk '{sum+=$2} END{print sum+0}')
  CACHE_HITS=$(curl -sf "http://$ip:9090/metrics" 2>/dev/null | grep "kiseki_cache_hit_total" | awk '{sum+=$2} END{print sum+0}')
  log "  $ip: gateway_requests=$REQS cache_hits=$CACHE_HITS" | tee -a "$RESULTS/metrics-snapshot.txt"
done

log ""
log "╔═══════════════════════════════════════════════════════════════╗"
log "║                  GPU-profile benchmark complete              ║"
log "║ Results: $RESULTS"
log "╚═══════════════════════════════════════════════════════════════╝"
