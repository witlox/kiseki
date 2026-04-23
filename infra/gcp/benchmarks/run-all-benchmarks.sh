#!/bin/bash
# Master benchmark runner — orchestrates all performance tests.
# Run from bench-ctrl node.
set -euo pipefail

source /etc/kiseki-bench.env
RESULTS_DIR="/tmp/kiseki-bench-results-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RESULTS_DIR"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║           Kiseki Performance Benchmark Suite                ║"
echo "╠══════════════════════════════════════════════════════════════╣"
echo "║ Storage: $STORAGE_IPS"
echo "║ Clients: $CLIENT_IPS"
echo "║ Results: $RESULTS_DIR"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Wait for cluster health
echo "=== Checking cluster health ==="
for ip in $(echo "$STORAGE_IPS" | tr ',' ' '); do
  if curl -sf "http://$ip:9090/health" >/dev/null 2>&1; then
    echo "  ✓ $ip healthy"
  else
    echo "  ✗ $ip unreachable — aborting"
    exit 1
  fi
done
echo ""

# ---------------------------------------------------------------------------
# 1. S3 throughput benchmark
# ---------------------------------------------------------------------------
echo "=== 1. S3 Throughput Benchmark ==="
S3_ENDPOINT="http://$FIRST_STORAGE:9000"

# Create test bucket
curl -sf -X PUT "$S3_ENDPOINT/perf-test" >/dev/null

# Sequential write: 1000 × 1MB objects
echo "  Writing 1000 × 1MB objects..."
START=$(date +%s%N)
for i in $(seq 1 1000); do
  dd if=/dev/urandom bs=1M count=1 2>/dev/null | \
    curl -sf -X PUT "$S3_ENDPOINT/perf-test/obj-$i" --data-binary @- >/dev/null &
  # Limit concurrency to 32
  [ $(( i % 32 )) -eq 0 ] && wait
done
wait
END=$(date +%s%N)
WRITE_MS=$(( (END - START) / 1000000 ))
WRITE_MBS=$(echo "scale=1; 1000 * 1000000 / $WRITE_MS" | bc 2>/dev/null || echo "N/A")
echo "  Write: ${WRITE_MS}ms (${WRITE_MBS} KB/s)" | tee "$RESULTS_DIR/s3-write.txt"

# Sequential read: 1000 × 1MB objects
echo "  Reading 1000 × 1MB objects..."
START=$(date +%s%N)
for i in $(seq 1 1000); do
  curl -sf "$S3_ENDPOINT/perf-test/obj-$i" -o /dev/null &
  [ $(( i % 32 )) -eq 0 ] && wait
done
wait
END=$(date +%s%N)
READ_MS=$(( (END - START) / 1000000 ))
READ_MBS=$(echo "scale=1; 1000 * 1000000 / $READ_MS" | bc 2>/dev/null || echo "N/A")
echo "  Read: ${READ_MS}ms (${READ_MBS} KB/s)" | tee "$RESULTS_DIR/s3-read.txt"
echo ""

# ---------------------------------------------------------------------------
# 2. S3 latency benchmark
# ---------------------------------------------------------------------------
echo "=== 2. S3 Latency Benchmark ==="

# Small object PUT latency (1KB × 100)
echo "  PUT latency (1KB × 100 sequential)..."
LATENCIES=""
for i in $(seq 1 100); do
  START=$(date +%s%N)
  echo "x" | curl -sf -X PUT "$S3_ENDPOINT/perf-test/lat-$i" --data-binary @- >/dev/null
  END=$(date +%s%N)
  US=$(( (END - START) / 1000 ))
  LATENCIES="$LATENCIES $US"
done

# Compute percentiles
echo "$LATENCIES" | tr ' ' '\n' | sort -n | awk '
  { a[NR] = $1; sum += $1 }
  END {
    n = NR
    printf "  p50: %d µs\n", a[int(n*0.50)]
    printf "  p99: %d µs\n", a[int(n*0.99)]
    printf "  avg: %d µs\n", sum/n
  }
' | tee "$RESULTS_DIR/s3-latency.txt"
echo ""

# ---------------------------------------------------------------------------
# 3. NFS benchmark (run on client-3)
# ---------------------------------------------------------------------------
echo "=== 3. NFS Benchmark ==="
CLIENT_NFS=$(echo "$CLIENT_IPS" | cut -d',' -f3)

ssh -o StrictHostKeyChecking=no "$CLIENT_NFS" bash <<'REMOTE_NFS'
set -e
source /etc/kiseki-bench.env 2>/dev/null || true
FIRST_STORAGE=$(echo "$STORAGE_IPS" | cut -d',' -f1)

# Mount NFS if not already mounted
mountpoint -q /mnt/kiseki-nfs || mount -t nfs4 "$FIRST_STORAGE":/ /mnt/kiseki-nfs 2>/dev/null || {
  echo "  NFS mount failed — skipping NFS benchmarks"
  exit 0
}

echo "  Sequential write (fio, 4 jobs × 256MB)..."
fio --name=seq_write --directory=/mnt/kiseki-nfs --rw=write --bs=1m \
  --size=256m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
  python3 -c "
import sys, json
d = json.load(sys.stdin)
bw = d['jobs'][0]['write']['bw'] / 1024
iops = d['jobs'][0]['write']['iops']
print(f'  Write: {bw:.1f} MB/s, {iops:.0f} IOPS')
" 2>/dev/null || echo "  fio not available"

echo "  Sequential read (fio, 4 jobs × 256MB)..."
fio --name=seq_read --directory=/mnt/kiseki-nfs --rw=read --bs=1m \
  --size=256m --numjobs=4 --group_reporting --output-format=json 2>/dev/null | \
  python3 -c "
import sys, json
d = json.load(sys.stdin)
bw = d['jobs'][0]['read']['bw'] / 1024
iops = d['jobs'][0]['read']['iops']
print(f'  Read: {bw:.1f} MB/s, {iops:.0f} IOPS')
" 2>/dev/null || echo "  fio not available"

echo "  Random 4K read (fio, 4 jobs, 30s)..."
fio --name=rand_read --directory=/mnt/kiseki-nfs --rw=randread --bs=4k \
  --size=64m --numjobs=4 --runtime=30 --time_based --group_reporting \
  --output-format=json 2>/dev/null | \
  python3 -c "
import sys, json
d = json.load(sys.stdin)
iops = d['jobs'][0]['read']['iops']
lat = d['jobs'][0]['read']['lat_ns']['mean'] / 1000
print(f'  Random read: {iops:.0f} IOPS, {lat:.0f} µs avg latency')
" 2>/dev/null || echo "  fio not available"

rm -f /mnt/kiseki-nfs/seq_write.* /mnt/kiseki-nfs/seq_read.* /mnt/kiseki-nfs/rand_read.* 2>/dev/null
REMOTE_NFS
echo "" | tee -a "$RESULTS_DIR/nfs.txt"

# ---------------------------------------------------------------------------
# 4. TCP transport benchmark
# ---------------------------------------------------------------------------
echo "=== 4. TCP Transport Benchmark ==="
echo "  Running iperf3 between storage nodes..."
for ip in $(echo "$STORAGE_IPS" | tr ',' ' ' | tail -n +2); do
  BW=$(ssh -o StrictHostKeyChecking=no "$FIRST_STORAGE" \
    "iperf3 -c $ip -t 10 -J 2>/dev/null" | \
    python3 -c "
import sys, json
d = json.load(sys.stdin)
bw = d['end']['sum_received']['bits_per_second'] / 1e9
print(f'{bw:.2f}')
" 2>/dev/null || echo "N/A")
  echo "  $FIRST_STORAGE → $ip: ${BW} Gbps" | tee -a "$RESULTS_DIR/tcp-bandwidth.txt"
done
echo ""

# ---------------------------------------------------------------------------
# 5. Per-disk-type comparison
# ---------------------------------------------------------------------------
echo "=== 5. Per-Disk-Type Comparison ==="
for ip in $(echo "$STORAGE_IPS" | tr ',' ' '); do
  METRICS=$(curl -sf "http://$ip:9090/ui/api/cluster" 2>/dev/null || echo "{}")
  WRITES=$(echo "$METRICS" | python3 -c "import sys,json; print(json.load(sys.stdin).get('aggregate',{}).get('chunk_write_bytes',0))" 2>/dev/null || echo "0")
  echo "  $ip: $WRITES bytes written" | tee -a "$RESULTS_DIR/disk-comparison.txt"
done
echo ""

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║                    Benchmark Complete                       ║"
echo "╠══════════════════════════════════════════════════════════════╣"
echo "║ Results: $RESULTS_DIR"
echo "║ Dashboard: http://$FIRST_STORAGE:9090/ui"
echo "╚══════════════════════════════════════════════════════════════╝"

# Collect all results into a single report
cat "$RESULTS_DIR"/*.txt > "$RESULTS_DIR/SUMMARY.txt" 2>/dev/null
echo "Full report: $RESULTS_DIR/SUMMARY.txt"
