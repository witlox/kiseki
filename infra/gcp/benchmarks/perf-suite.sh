#!/bin/bash
# Kiseki "default" profile — broad-coverage benchmark suite.
#
# Targets the cluster shape provisioned by var.profile = "default":
#   * 6 × c3-standard-22 storage nodes, all-NVMe device pool, Tier_1 50 Gbps
#   * 3 × c3-standard-22 client nodes with 200 GB PD-SSD L2 cache
#
# Tests S3, NFSv4, pNFS, and FUSE paths. fio sizes are large enough to defeat
# the 88 GB host page cache, and every fio call uses --direct=1 — without
# that the read paths cache after warmup and the numbers look like RAM
# bandwidth, not kiseki bandwidth (hard-learned: see specs/findings/
# phase-15c10-nfs41-perf-investigation.md).
#
# Run on the bench-ctrl node. Results land in /tmp/kiseki-perf-default-* and
# upload to gs://${KISEKI_PERF_BUCKET}/.

source "$(dirname "$0")/perf-common.sh"

trap 'stop_metrics; write_summary "DEFAULT-PROFILE" cluster-info cluster-state transport bandwidth nfs-write pnfs fuse s3-latency s3-write s3-read s3-parallel-write metrics-snapshot; upload_results' EXIT

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║       Kiseki Performance — Default Profile (broad)           ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ Cluster: 6 × c3-standard-22, all-NVMe                        ║"
echo "║ Clients: 3 × c3-standard-22 with 200GB PD-SSD cache          ║"
echo "║ Parallelism: $PAR (override via KISEKI_BENCH_PAR)"
echo "║ Results: $RESULTS"
echo "╚═══════════════════════════════════════════════════════════════╝"

start_metrics

# ---------------------------------------------------------------------------
# 0. Cluster health + leader discovery
# ---------------------------------------------------------------------------
log "=== 0. Cluster Health & Leader Discovery ==="
discover_leader
log ""
log "  Raft leader: node $LEADER_ID → S3=$LEADER_S3 NFS=$LEADER_HOST:2049"
log "  All writes routed to leader; reads distributed"

EP="$LEADER_S3"

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
# 2. Transport selection (informational — GCP exposes no RDMA)
# ---------------------------------------------------------------------------
log ""
log "=== 2. Transport Selection ==="
log "  GCP: no RDMA/RoCEv2 exposure on c3 → kiseki picks TCP+TLS" | tee -a "$RESULTS/transport.txt"
for ip in $ALL_STORAGE; do
  RDMA=$(node_ssh "$ip" "ls /sys/class/infiniband/ 2>/dev/null | wc -l" 2>/dev/null || echo "0")
  log "  $ip: IB=$RDMA → TCP" | tee -a "$RESULTS/transport.txt"
done

# ---------------------------------------------------------------------------
# 3. Inter-node TCP bandwidth (client→leader, storage↔storage)
# ---------------------------------------------------------------------------
log ""
log "=== 3. Inter-Node TCP Bandwidth (iperf3 baseline) ==="
node_ssh "$LEADER_HOST" "pkill iperf3 2>/dev/null; iperf3 -s -D 2>/dev/null" 2>/dev/null
sleep 1
for ip in $CLIENTS_WS; do
  BW=$(node_ssh "$ip" "iperf3 -c $LEADER_HOST -t 5 -J 2>/dev/null" 2>/dev/null | \
       python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "N/A")
  log "  $ip (client) → $LEADER_HOST (leader): ${BW} Gbps" | tee -a "$RESULTS/bandwidth.txt"
done

OTHER_STORAGE=$(echo $ALL_STORAGE | tr ' ' '\n' | grep -v "^${LEADER_HOST}$" | head -1)
if [ -n "$OTHER_STORAGE" ]; then
  node_ssh "$OTHER_STORAGE" "pkill iperf3 2>/dev/null; iperf3 -s -D 2>/dev/null" 2>/dev/null
  sleep 1
  BW=$(node_ssh "$LEADER_HOST" "iperf3 -c $OTHER_STORAGE -t 5 -J 2>/dev/null" 2>/dev/null | \
       python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "N/A")
  log "  $LEADER_HOST → $OTHER_STORAGE (storage↔storage): ${BW} Gbps" | tee -a "$RESULTS/bandwidth.txt"
fi

# ---------------------------------------------------------------------------
# Generic fio runner — sizes blow past page cache; --direct=1 forces O_DIRECT.
# ---------------------------------------------------------------------------
# Per-job size (for 4 jobs): 4 GB write + 8 GB read, larger than 88 GB host
# RAM is unnecessary thanks to --direct=1, but we still want the working set
# to be much larger than the per-disk DRAM cache (~1-2 GB on local NVMe).
WRITE_SIZE="4G"
READ_SIZE="8G"
RAND_SIZE="2G"
NUMJOBS=4

# ---------------------------------------------------------------------------
# 4. NFSv4 sequential write (3 clients → leader)
# ---------------------------------------------------------------------------
log ""
log "=== 4. NFSv4 Sequential Write (3 clients → leader, --direct=1) ==="
PIDS=""
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    mkdir -p /mnt/kiseki-nfs-leader
    umount /mnt/kiseki-nfs-leader 2>/dev/null || true
    mount -t nfs4 -o vers=4.2,rsize=1048576,wsize=1048576 $LEADER_NFS_HOST:/ /mnt/kiseki-nfs-leader 2>/dev/null
    if mountpoint -q /mnt/kiseki-nfs-leader 2>/dev/null; then
      fio --name=nfs-write --directory=/mnt/kiseki-nfs-leader --rw=write --bs=1m \
        --size=$WRITE_SIZE --numjobs=$NUMJOBS --direct=1 --group_reporting \
        --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  client-$((idx+1)): {bw:.1f} MB/s (NFSv4.2)\")' 2>/dev/null || \
        echo '  client-$((idx+1)): fio parse error'
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
# 4b. pNFS write+read (3 clients with layout delegation)
# ---------------------------------------------------------------------------
log ""
log "=== 4b. pNFSv4.1 Write+Read (3 clients, layout delegation) ==="
PIDS=""
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    mkdir -p /mnt/kiseki-pnfs
    umount /mnt/kiseki-pnfs 2>/dev/null || true
    mount -t nfs4 -o vers=4.2,pnfs,rsize=1048576,wsize=1048576 $LEADER_NFS_HOST:/ /mnt/kiseki-pnfs 2>/dev/null
    if mountpoint -q /mnt/kiseki-pnfs 2>/dev/null; then
      fio --name=pnfs-write --directory=/mnt/kiseki-pnfs --rw=write --bs=1m \
        --size=$WRITE_SIZE --numjobs=$NUMJOBS --direct=1 --group_reporting \
        --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"  client-$((idx+1)) write: {bw:.1f} MB/s\")' 2>/dev/null || \
        echo '  client-$((idx+1)) write: fio parse error'
      fio --name=pnfs-read --directory=/mnt/kiseki-pnfs --rw=read --bs=1m \
        --size=$READ_SIZE --numjobs=$NUMJOBS --direct=1 --group_reporting \
        --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"read\"][\"bw\"]/1024; print(f\"  client-$((idx+1)) read:  {bw:.1f} MB/s\")' 2>/dev/null || \
        echo '  client-$((idx+1)) read: fio parse error'
      echo '--- mountstats ---'
      grep -A5 'kiseki-pnfs' /proc/self/mountstats 2>/dev/null | grep -i layout || \
        echo '  No LAYOUTGET observed (fallback to NFSv4.2)'
      rm -f /mnt/kiseki-pnfs/pnfs-* 2>/dev/null
      umount /mnt/kiseki-pnfs 2>/dev/null || true
    else
      echo '  client-$((idx+1)): pNFS mount failed'
    fi
  " 2>/dev/null | tee -a "$RESULTS/pnfs.txt" &
  PIDS="$PIDS $!"
done
for pid in $PIDS; do wait $pid 2>/dev/null || true; done

if grep -q "LAYOUTGET\|nfsv4-flexfiles" "$RESULTS/pnfs.txt" 2>/dev/null; then
  log "  pNFS: layout delegation ACTIVE" | tee -a "$RESULTS/pnfs.txt"
else
  log "  pNFS: no layout delegation observed — fell back to NFSv4.2" | tee -a "$RESULTS/pnfs.txt"
fi

# ---------------------------------------------------------------------------
# 5. FUSE native client (write/read/random/metadata) on client-1
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

    echo '  Sequential write (fio, $NUMJOBS jobs × $WRITE_SIZE, --direct=1):'
    fio --name=fuse-write --directory=/mnt/kiseki-fuse --rw=write --bs=1m \
      --size=$WRITE_SIZE --numjobs=$NUMJOBS --direct=1 --group_reporting \
      --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"write\"][\"bw\"]/1024; print(f\"    Write: {bw:.1f} MB/s\")' 2>/dev/null

    echo '  Sequential read (fio, $NUMJOBS jobs × $READ_SIZE, --direct=1):'
    fio --name=fuse-read --directory=/mnt/kiseki-fuse --rw=read --bs=1m \
      --size=$READ_SIZE --numjobs=$NUMJOBS --direct=1 --group_reporting \
      --output-format=json 2>/dev/null | \
      python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"read\"][\"bw\"]/1024; print(f\"    Read:  {bw:.1f} MB/s\")' 2>/dev/null

    echo '  Random 4K read (fio, $NUMJOBS jobs × $RAND_SIZE, --direct=1, 30s):'
    fio --name=fuse-rand --directory=/mnt/kiseki-fuse --rw=randread --bs=4k \
      --size=$RAND_SIZE --numjobs=$NUMJOBS --direct=1 --runtime=30 --time_based --ramp_time=5 \
      --group_reporting --output-format=json 2>/dev/null | \
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
# 6. S3 PUT latency (1KB × 100, p50/p99 from client-1)
# ---------------------------------------------------------------------------
log ""
log "=== 6. S3 PUT Latency (1KB × 100 → leader, from client-1) ==="
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
# 7. S3 sequential write (single client, sweep object size)
# ---------------------------------------------------------------------------
log ""
log "=== 7. S3 Sequential Write (client-1 → leader, ${PAR}∥) ==="
node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  PAR=$PAR
  for SIZE in 1 4 16 64; do
    TOTAL=\$(( 1024 / SIZE ))
    [ \"\$SIZE\" -ge 64 ] && TOTAL=16
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
# 8. S3 read throughput (single client)
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
# 9. S3 parallel write (3 clients, aggregate)
# ---------------------------------------------------------------------------
log ""
log "=== 9. S3 Parallel Write (3 clients → leader, aggregate throughput) ==="
log "  3 clients × 100 × 1MB = 300 MB total, ${PAR} concurrent per client"
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
log "=== 10. Prometheus Metrics Snapshot ==="
for ip in $ALL_STORAGE; do
  REQS=$(curl -sf "http://$ip:9090/metrics" 2>/dev/null | grep "kiseki_gateway_requests_total" | awk '{sum+=$2} END{print sum+0}')
  log "  $ip: gateway_requests=$REQS" | tee -a "$RESULTS/metrics-snapshot.txt"
done

log ""
log "╔═══════════════════════════════════════════════════════════════╗"
log "║                Default-profile benchmark complete            ║"
log "║ Results: $RESULTS"
log "╚═══════════════════════════════════════════════════════════════╝"
