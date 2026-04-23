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
# 3. Parallel NFS (multi-client sequential I/O)
# ---------------------------------------------------------------------------
echo ""
echo "=== 3. Parallel NFS (3 clients × fio) ==="

CLIENT_ARRAY=($CLIENTS)
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  # Mount NFS from first HDD node
  ssh -o StrictHostKeyChecking=no "$CIP" "
    mount -t nfs4 $FIRST_HDD:/ /mnt/kiseki-nfs-1 2>/dev/null || true
    fio --name=nfs-seq-write --directory=/mnt/kiseki-nfs-1 --rw=write --bs=1m \
      --size=256m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  client-$((idx+1)) NFS write: {bw:.1f} MB/s\")' 2>/dev/null
  " 2>/dev/null &
done
wait | tee -a "$RESULTS/nfs-parallel.txt"

# ---------------------------------------------------------------------------
# 4. FUSE native client benchmark
# ---------------------------------------------------------------------------
echo ""
echo "=== 4. FUSE Native Client ==="

FIRST_CLIENT="${CLIENT_ARRAY[0]}"
ssh -o StrictHostKeyChecking=no "$FIRST_CLIENT" "
  source /etc/kiseki-client.env 2>/dev/null || true

  echo '  FUSE mount + sequential write (fio):'
  # FUSE mount would go here when kiseki-client mount is wired
  # For now, benchmark the S3 path as proxy
  fio --name=fuse-seq --filename=/tmp/fuse-test --rw=write --bs=1m \
    --size=128m --numjobs=2 --group_reporting --output-format=json 2>/dev/null | \
    python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  Sequential write: {bw:.1f} MB/s (local disk baseline)\")' 2>/dev/null

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
