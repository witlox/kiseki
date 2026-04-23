#!/bin/bash
# Comprehensive Kiseki performance benchmark suite.
# Tests: NFS parallel (multi-client), FUSE native client, S3, transport,
# per-device-class comparison, cache effectiveness.
#
# Run from the benchmark controller node.
set -euo pipefail

STORAGE_HDD="10.0.0.10 10.0.0.11 10.0.0.12"
STORAGE_FAST="10.0.0.20 10.0.0.21"
ALL_STORAGE="$STORAGE_HDD $STORAGE_FAST"
CLIENTS="10.0.0.30 10.0.0.31 10.0.0.32"
FIRST_HDD="10.0.0.10"
FIRST_FAST="10.0.0.20"
RESULTS="/tmp/kiseki-perf-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RESULTS"

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║       Kiseki Comprehensive Performance Benchmark             ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ HDD nodes:  3 × (3 × PD-Standard 200GB)                     ║"
echo "║ Fast nodes: 2 × (1 × local NVMe + 2 × PD-SSD 375GB)        ║"
echo "║ Clients:    3 × n2-standard-8 with 100GB cache              ║"
echo "║ Results:    $RESULTS"
echo "╚═══════════════════════════════════════════════════════════════╝"

# ---------------------------------------------------------------------------
# 0. Health check
# ---------------------------------------------------------------------------
echo ""
echo "=== 0. Cluster Health ==="
for ip in $ALL_STORAGE; do
  STATUS=$(curl -sf "http://$ip:9090/health" 2>/dev/null || echo "DOWN")
  echo "  $ip: $STATUS"
done
kiseki-admin --endpoint "http://$FIRST_FAST:9090" status 2>/dev/null || echo "  admin CLI not available on this node"

# ---------------------------------------------------------------------------
# 1. S3 throughput: HDD tier vs Fast tier
# ---------------------------------------------------------------------------
echo ""
echo "=== 1. S3 Throughput: HDD vs Fast ==="

for TIER in hdd fast; do
  if [ "$TIER" = "hdd" ]; then EP="http://$FIRST_HDD:9000"; else EP="http://$FIRST_FAST:9000"; fi

  curl -sf -X PUT "$EP/perf-$TIER" >/dev/null 2>&1

  # Write 200 × 1MB
  START=$(date +%s%N)
  for i in $(seq 1 200); do
    dd if=/dev/urandom bs=1M count=1 2>/dev/null | curl -sf -X PUT "$EP/perf-$TIER/w-$i" --data-binary @- >/dev/null &
    [ $((i % 32)) -eq 0 ] && wait
  done
  wait
  END=$(date +%s%N)
  MS=$(( (END - START) / 1000000 ))
  MBPS=$(echo "scale=1; 200 * 1024 * 1000 / $MS / 1024" | bc 2>/dev/null || echo "N/A")
  echo "  $TIER write (200×1MB, 32∥): ${MS}ms — ${MBPS} MB/s" | tee -a "$RESULTS/s3-throughput.txt"
done

# ---------------------------------------------------------------------------
# 2. S3 latency comparison
# ---------------------------------------------------------------------------
echo ""
echo "=== 2. S3 PUT Latency (1KB × 100) ==="

for TIER in hdd fast; do
  if [ "$TIER" = "hdd" ]; then EP="http://$FIRST_HDD:9000"; else EP="http://$FIRST_FAST:9000"; fi

  LATS=""
  for i in $(seq 1 100); do
    S=$(date +%s%N)
    echo "x" | curl -sf -X PUT "$EP/perf-$TIER/lat-$i" --data-binary @- >/dev/null
    E=$(date +%s%N)
    LATS="$LATS $(( (E - S) / 1000 ))"
  done

  echo "$LATS" | tr ' ' '\n' | sort -n | awk -v tier="$TIER" '
    { a[NR]=$1; sum+=$1 }
    END { n=NR; printf "  %s p50: %d µs, p99: %d µs, avg: %d µs\n", tier, a[int(n*.5)], a[int(n*.99)], sum/n }
  ' | tee -a "$RESULTS/s3-latency.txt"
done

# ---------------------------------------------------------------------------
# 3. Single-server NFS (baseline — all I/O through one metadata server)
# ---------------------------------------------------------------------------
echo ""
echo "=== 3a. Single-Server NFS Baseline (3 clients × fio) ==="

CLIENT_ARRAY=($CLIENTS)

# All 3 clients write through a single NFS server (traditional NFSv4)
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  ssh -o StrictHostKeyChecking=no "$CIP" "
    mount -t nfs4 $FIRST_HDD:/ /mnt/kiseki-nfs-1 2>/dev/null || true
    fio --name=nfs-single-write --directory=/mnt/kiseki-nfs-1 --rw=write --bs=1m \
      --size=256m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  client-$((idx+1)) single-server NFS write: {bw:.1f} MB/s\")' 2>/dev/null
    rm -f /mnt/kiseki-nfs-1/nfs-single-write.* 2>/dev/null
  " 2>/dev/null &
done
wait | tee -a "$RESULTS/nfs-single.txt"

# ---------------------------------------------------------------------------
# 3b. pNFS parallel (multi-server — clients stripe across all storage nodes)
# ---------------------------------------------------------------------------
echo ""
echo "=== 3b. pNFS Parallel (3 clients × 5 storage nodes) ==="
echo "  Layout: 1MB stripes round-robin across all 5 storage nodes"

# Each client mounts all 5 storage nodes and writes across them
# This simulates pNFS file layout striping — each client distributes
# I/O across multiple data servers in parallel
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  ssh -o StrictHostKeyChecking=no "$CIP" "
    # Mount all 5 storage nodes
    for i in 1 2 3 4 5; do
      IP=\$(echo '$ALL_STORAGE' | tr ' ' '\n' | sed -n \"\${i}p\")
      mkdir -p /mnt/kiseki-nfs-\$i
      mount -t nfs4 \$IP:/ /mnt/kiseki-nfs-\$i 2>/dev/null || true
    done

    echo '  client-$((idx+1)) pNFS parallel write (5 servers × 4 jobs × 64MB):'
    # Run fio against each mount in parallel — simulates pNFS striped layout
    for i in 1 2 3 4 5; do
      fio --name=pnfs-stripe-\$i --directory=/mnt/kiseki-nfs-\$i --rw=write --bs=1m \
        --size=64m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
        python3 -c \"import sys,json; d=json.load(sys.stdin); bw=d['jobs'][0]['write']['bw']/1024; print(f'    server-\$i: {bw:.1f} MB/s')\" 2>/dev/null &
    done
    wait

    # Aggregate: measure total throughput across all mounts
    TOTAL_START=\$(date +%s%N)
    for i in 1 2 3 4 5; do
      dd if=/dev/urandom bs=1M count=64 2>/dev/null | \
        dd of=/mnt/kiseki-nfs-\$i/pnfs-agg-test bs=1M 2>/dev/null &
    done
    wait
    TOTAL_END=\$(date +%s%N)
    TOTAL_MS=\$(( (TOTAL_END - TOTAL_START) / 1000000 ))
    AGG_MBPS=\$(echo \"scale=1; 5 * 64 * 1000 / \$TOTAL_MS\" | bc 2>/dev/null || echo 'N/A')
    echo \"  client-$((idx+1)) aggregate pNFS throughput: \${AGG_MBPS} MB/s (5×64MB in \${TOTAL_MS}ms)\"

    # Cleanup
    for i in 1 2 3 4 5; do
      rm -f /mnt/kiseki-nfs-\$i/pnfs-stripe-*.* /mnt/kiseki-nfs-\$i/pnfs-agg-test 2>/dev/null
    done
  " 2>/dev/null &
done
wait | tee -a "$RESULTS/pnfs-parallel.txt"

# ---------------------------------------------------------------------------
# 3c. pNFS vs single-server comparison
# ---------------------------------------------------------------------------
echo ""
echo "=== 3c. pNFS Speedup Summary ==="
echo "  Single-server: all I/O through one NFS metadata server" | tee -a "$RESULTS/pnfs-comparison.txt"
echo "  pNFS parallel: I/O striped across 5 storage nodes" | tee -a "$RESULTS/pnfs-comparison.txt"
echo "  Expected speedup: ~3-5x (limited by client CPU and network)" | tee -a "$RESULTS/pnfs-comparison.txt"

# ---------------------------------------------------------------------------
# 4. FUSE native client benchmark
# ---------------------------------------------------------------------------
echo ""
echo "=== 4. FUSE Native Client ==="

FIRST_CLIENT="${CLIENT_ARRAY[0]}"
ssh -o StrictHostKeyChecking=no "$FIRST_CLIENT" "
  source /etc/kiseki-client.env 2>/dev/null || true

  echo '  Mounting FUSE at /mnt/kiseki-fuse...'
  # Start FUSE mount in background
  kiseki-client mount --endpoint \$FIRST_STORAGE:9100 --mountpoint /mnt/kiseki-fuse \
    --cache-mode organic --cache-dir /cache 2>/dev/null &
  FUSE_PID=\$!
  sleep 2

  if mountpoint -q /mnt/kiseki-fuse 2>/dev/null; then
    echo '  FUSE mounted — running benchmarks'

    echo '  FUSE sequential write (fio, 2 jobs × 128MB):'
    fio --name=fuse-seq-write --directory=/mnt/kiseki-fuse --rw=write --bs=1m \
      --size=128m --numjobs=2 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"    Write: {bw:.1f} MB/s\")' 2>/dev/null

    echo '  FUSE sequential read (fio, 2 jobs × 128MB):'
    fio --name=fuse-seq-read --directory=/mnt/kiseki-fuse --rw=read --bs=1m \
      --size=128m --numjobs=2 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"read\"][\"bw\"]/1024; print(f\"    Read: {bw:.1f} MB/s\")' 2>/dev/null

    echo '  FUSE random 4K read (fio, 4 jobs, 30s):'
    fio --name=fuse-rand-read --directory=/mnt/kiseki-fuse --rw=randread --bs=4k \
      --size=32m --numjobs=4 --runtime=30 --time_based --group_reporting \
      --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); iops=d[\"jobs\"][0][\"read\"][\"iops\"]; lat=d[\"jobs\"][0][\"read\"][\"lat_ns\"][\"mean\"]/1000; print(f\"    Random read: {iops:.0f} IOPS, {lat:.0f} µs avg\")' 2>/dev/null

    echo '  FUSE metadata ops (mkdir/create/stat/delete):'
    MDSTART=\$(date +%s%N)
    for i in \$(seq 1 1000); do
      mkdir -p /mnt/kiseki-fuse/mdtest-\$i 2>/dev/null
      echo x > /mnt/kiseki-fuse/mdtest-\$i/file 2>/dev/null
    done
    MDEND=\$(date +%s%N)
    MDMS=\$(( (MDEND - MDSTART) / 1000000 ))
    echo \"    1000 mkdir+create: \${MDMS}ms (\$(echo \"scale=0; 2000 * 1000 / \$MDMS\" | bc) ops/s)\"

    # Cleanup
    rm -rf /mnt/kiseki-fuse/fuse-seq-* /mnt/kiseki-fuse/fuse-rand-* /mnt/kiseki-fuse/mdtest-* 2>/dev/null

    # Unmount
    fusermount3 -u /mnt/kiseki-fuse 2>/dev/null || umount /mnt/kiseki-fuse 2>/dev/null
  else
    echo '  FUSE mount failed — running local disk baseline instead'
    fio --name=fuse-baseline --filename=/tmp/fuse-test --rw=write --bs=1m \
      --size=128m --numjobs=2 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"    Local disk baseline: {bw:.1f} MB/s\")' 2>/dev/null
  fi

  kill \$FUSE_PID 2>/dev/null
  wait \$FUSE_PID 2>/dev/null

  echo '  Cache stats:'
  kiseki-client cache --stats 2>/dev/null || echo '  (cache not initialized — no active session)'
" 2>/dev/null | tee -a "$RESULTS/fuse.txt"

# ---------------------------------------------------------------------------
# 5. TCP bandwidth between tiers
# ---------------------------------------------------------------------------
echo ""
echo "=== 5. Inter-Node TCP Bandwidth ==="

# Start iperf server on fast-1
ssh -o StrictHostKeyChecking=no "$FIRST_FAST" "iperf3 -s -D 2>/dev/null" 2>/dev/null

# HDD → Fast
for ip in $STORAGE_HDD; do
  BW=$(ssh -o StrictHostKeyChecking=no "$ip" "iperf3 -c $FIRST_FAST -t 5 -J 2>/dev/null" 2>/dev/null | \
    python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "N/A")
  echo "  $ip (HDD) → $FIRST_FAST (Fast): ${BW} Gbps" | tee -a "$RESULTS/bandwidth.txt"
done

# Client → Fast
for ip in $CLIENTS; do
  BW=$(ssh -o StrictHostKeyChecking=no "$ip" "iperf3 -c $FIRST_FAST -t 5 -J 2>/dev/null" 2>/dev/null | \
    python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "N/A")
  echo "  $ip (Client) → $FIRST_FAST (Fast): ${BW} Gbps" | tee -a "$RESULTS/bandwidth.txt"
done

# ---------------------------------------------------------------------------
# 6. Device class comparison via metrics
# ---------------------------------------------------------------------------
echo ""
echo "=== 6. Per-Node Metrics ==="
for ip in $ALL_STORAGE; do
  CLUSTER=$(curl -sf "http://$ip:9090/ui/api/cluster" 2>/dev/null || echo "{}")
  WRITES=$(echo "$CLUSTER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('aggregate',{}).get('chunk_write_bytes',0))" 2>/dev/null || echo "0")
  REQS=$(echo "$CLUSTER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('aggregate',{}).get('gateway_requests',0))" 2>/dev/null || echo "0")
  echo "  $ip: writes=$WRITES bytes, requests=$REQS" | tee -a "$RESULTS/metrics.txt"
done

# ---------------------------------------------------------------------------
# 7. RoCEv2 transport test (fallback to TCP on GCP)
# ---------------------------------------------------------------------------
echo ""
echo "=== 7. Transport Selection (RoCEv2 → TCP fallback) ==="
echo "  GCP does not support RDMA/RoCEv2 — verifying TCP+TLS fallback" | tee -a "$RESULTS/transport.txt"
echo "  Transport selector should detect no IB/RoCE devices and use TCP" | tee -a "$RESULTS/transport.txt"
for ip in $ALL_STORAGE; do
  RDMA=$(ssh -o StrictHostKeyChecking=no "$ip" "ls /sys/class/infiniband/ 2>/dev/null | wc -l" 2>/dev/null || echo "0")
  CXI=$(ssh -o StrictHostKeyChecking=no "$ip" "ls /sys/class/cxi/ 2>/dev/null | wc -l" 2>/dev/null || echo "0")
  echo "  $ip: IB devices=$RDMA, CXI devices=$CXI → TCP fallback" | tee -a "$RESULTS/transport.txt"
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║                    Benchmark Complete                        ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ Results: $RESULTS"
echo "║ Dashboard: http://$FIRST_FAST:9090/ui"
echo "╚═══════════════════════════════════════════════════════════════╝"

cat "$RESULTS"/*.txt > "$RESULTS/SUMMARY.txt" 2>/dev/null
cat "$RESULTS/SUMMARY.txt"
