#!/bin/bash
# Kiseki "transport" profile — protocol/NIC ceiling suite.
#
# Targets the cluster shape provisioned by var.profile = "transport":
#   * 3 × c3-standard-88 storage, 8 × local NVMe each (~32 GB/s aggregate
#     read per node), Tier_1 100 Gbps egress
#   * 3 × c3-standard-44 client, Tier_1 100 Gbps egress
#
# Goal: surface kiseki's gRPC + S3 gateway overhead vs raw TCP. Disks are
# deliberately faster than the wire so any throughput cap is the protocol
# stack, not I/O. Each test reports an "overhead %" relative to the iperf3
# baseline measured between the same two hosts.
#
# What we deliberately don't test here: EC, metadata ops, FUSE — they belong
# in the default suite. Transport is one variable; keep it isolated.

source "$(dirname "$0")/perf-common.sh"

trap 'stop_metrics; write_summary "TRANSPORT-PROFILE" cluster-info iperf-baseline s3-single-stream s3-concurrency-sweep s3-get-sweep pnfs-aggregate mtls-overhead metrics-snapshot; upload_results' EXIT

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║       Kiseki Performance — Transport Ceiling Profile         ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ Cluster: 3 × c3-standard-88, 8 × local NVMe / node           ║"
echo "║ Clients: 3 × c3-standard-44, Tier_1 100 Gbps                 ║"
echo "║ Goal: protocol overhead vs iperf3 raw-TCP baseline           ║"
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
# 1. iperf3 baseline — measures the wire ceiling for every overhead %.
# ---------------------------------------------------------------------------
log ""
log "=== 1. iperf3 Baseline (raw TCP, 30s, parallel streams = 4) ==="
log "  All overhead % below are relative to these numbers."

node_ssh "$LEADER_HOST" "pkill iperf3 2>/dev/null; iperf3 -s -D 2>/dev/null" 2>/dev/null
sleep 1

declare -A BASELINE_GBPS
for ip in $CLIENTS_WS; do
  BW=$(node_ssh "$ip" "iperf3 -c $LEADER_HOST -t 30 -P 4 -J 2>/dev/null" 2>/dev/null | \
       python3 -c "import sys,json; d=json.load(sys.stdin); print(f'{d[\"end\"][\"sum_received\"][\"bits_per_second\"]/1e9:.1f}')" 2>/dev/null || echo "0")
  BASELINE_GBPS["$ip"]="$BW"
  log "  $ip → $LEADER_HOST: ${BW} Gbps (baseline)" | tee -a "$RESULTS/iperf-baseline.txt"
done

FIRST_CLIENT="${CLIENT_ARRAY[0]}"
BASE_GBPS="${BASELINE_GBPS[$FIRST_CLIENT]:-0}"

# ---------------------------------------------------------------------------
# 2. S3 single-stream large-object PUT (find peak per-stream throughput)
# ---------------------------------------------------------------------------
log ""
log "=== 2. S3 Single-Stream Large-Object PUT (1 / 10 GB) ==="
log "  Single-stream throughput is the hard ceiling for any client that"
log "  cannot or will not parallelize."
node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  curl -sf -X PUT \"\$EP/transport\" >/dev/null 2>&1 || true
  for SIZE_GB in 1 10; do
    SIZE_BYTES=\$(( SIZE_GB * 1024 * 1024 * 1024 ))
    START=\$(date +%s%N)
    dd if=/dev/zero bs=64M count=\$(( SIZE_GB * 16 )) 2>/dev/null | \
      curl -sf -X PUT \"\$EP/transport/single-\${SIZE_GB}g\" \
        -H \"Content-Length: \$SIZE_BYTES\" --data-binary @- >/dev/null
    END=\$(date +%s%N)
    MS=\$(( (END - START) / 1000000 ))
    MBPS=\$(python3 -c \"print(f'{\$SIZE_BYTES / 1024 / 1024 * 1000 / \$MS:.1f}')\")
    GBPS=\$(python3 -c \"print(f'{\$SIZE_BYTES * 8 / 1e9 * 1000 / \$MS:.1f}')\")
    echo \"  PUT \${SIZE_GB} GB single-stream: \${MS}ms — \${MBPS} MB/s (\${GBPS} Gbps)\"
  done
" 2>/dev/null | tee -a "$RESULTS/s3-single-stream.txt"

# Compute overhead vs baseline for the 10 GB case.
GBPS_10G=$(grep "PUT 10 GB" "$RESULTS/s3-single-stream.txt" | grep -oE '[0-9.]+ Gbps' | awk '{print $1}' | tail -1)
if [ -n "$GBPS_10G" ] && [ "$BASE_GBPS" != "0" ]; then
  PCT=$(ratio_pct "$GBPS_10G" "$BASE_GBPS")
  log "  10 GB single-stream: ${GBPS_10G} Gbps / ${BASE_GBPS} Gbps baseline = ${PCT} of wire" | tee -a "$RESULTS/s3-single-stream.txt"
fi

# ---------------------------------------------------------------------------
# 3. S3 PUT concurrency sweep (find the knee)
# ---------------------------------------------------------------------------
log ""
log "=== 3. S3 PUT Concurrency Sweep (64 MB objects @ 1/4/16/64/256∥) ==="
node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  curl -sf -X PUT \"\$EP/sweep\" >/dev/null 2>&1 || true
  for STREAMS in 1 4 16 64 256; do
    TOTAL_MB=\$(( STREAMS * 64 * 4 ))  # 4 objects per stream so even 1∥ has a meaningful runtime
    PER_STREAM=\$(( TOTAL_MB / STREAMS / 64 ))
    START=\$(date +%s%N)
    pids=''
    for s in \$(seq 1 \$STREAMS); do
      (
        for o in \$(seq 1 \$PER_STREAM); do
          dd if=/dev/zero bs=64M count=1 2>/dev/null | \
            curl -sf -X PUT \"\$EP/sweep/n\${STREAMS}-s\${s}-o\${o}\" --data-binary @- >/dev/null
        done
      ) &
      pids=\"\$pids \$!\"
    done
    for p in \$pids; do wait \$p 2>/dev/null; done
    END=\$(date +%s%N)
    MS=\$(( (END - START) / 1000000 ))
    GBPS=\$(python3 -c \"print(f'{\$TOTAL_MB * 8 / 1024 * 1000 / \$MS:.1f}')\")
    echo \"  streams=\${STREAMS} total=\${TOTAL_MB} MB time=\${MS}ms — \${GBPS} Gbps\"
  done
" 2>/dev/null | tee -a "$RESULTS/s3-concurrency-sweep.txt"

# ---------------------------------------------------------------------------
# 4. S3 GET concurrency sweep (over the same objects from test 3)
# ---------------------------------------------------------------------------
log ""
log "=== 4. S3 GET Concurrency Sweep (read-back of test-3 objects) ==="
node_ssh "$FIRST_CLIENT" "
  EP='$LEADER_S3'
  for STREAMS in 1 4 16 64 256; do
    PER_STREAM=4
    TOTAL_MB=\$(( STREAMS * 64 * PER_STREAM ))
    START=\$(date +%s%N)
    pids=''
    for s in \$(seq 1 \$STREAMS); do
      (
        for o in \$(seq 1 \$PER_STREAM); do
          curl -sf \"\$EP/sweep/n\${STREAMS}-s\${s}-o\${o}\" -o /dev/null
        done
      ) &
      pids=\"\$pids \$!\"
    done
    for p in \$pids; do wait \$p 2>/dev/null; done
    END=\$(date +%s%N)
    MS=\$(( (END - START) / 1000000 ))
    GBPS=\$(python3 -c \"print(f'{\$TOTAL_MB * 8 / 1024 * 1000 / \$MS:.1f}')\")
    echo \"  streams=\${STREAMS} total=\${TOTAL_MB} MB time=\${MS}ms — \${GBPS} Gbps\"
  done
" 2>/dev/null | tee -a "$RESULTS/s3-get-sweep.txt"

# ---------------------------------------------------------------------------
# 5. pNFS aggregate (3 clients × parallel reads of one large file)
# ---------------------------------------------------------------------------
log ""
log "=== 5. pNFS Aggregate (3 clients reading 1 × 16 GB file via flex-files) ==="
log "  Each client mounts pNFS, reads the same composition. With 3 mirrors"
log "  this should fan out across all 3 storage nodes — aggregate Gbps"
log "  should approach 3 × per-client baseline."

# First, write the 16 GB seed file via NFS from leader.
node_ssh "${CLIENT_ARRAY[0]}" "
  mkdir -p /mnt/kiseki-pnfs
  umount /mnt/kiseki-pnfs 2>/dev/null || true
  mount -t nfs4 -o vers=4.2,pnfs,rsize=1048576,wsize=1048576 $LEADER_NFS_HOST:/ /mnt/kiseki-pnfs
  if mountpoint -q /mnt/kiseki-pnfs; then
    fio --name=pnfs-seed --directory=/mnt/kiseki-pnfs --rw=write --bs=1m \
      --size=16G --numjobs=1 --direct=1 --output-format=json 2>/dev/null >/dev/null
    echo '  seed file written'
    umount /mnt/kiseki-pnfs
  fi
" 2>/dev/null | tee -a "$RESULTS/pnfs-aggregate.txt"

PIDS=""
AGG_START=$(date +%s%N)
for idx in 0 1 2; do
  CIP="${CLIENT_ARRAY[$idx]}"
  node_ssh "$CIP" "
    mkdir -p /mnt/kiseki-pnfs
    umount /mnt/kiseki-pnfs 2>/dev/null || true
    mount -t nfs4 -o vers=4.2,pnfs,rsize=1048576,wsize=1048576 $LEADER_NFS_HOST:/ /mnt/kiseki-pnfs 2>/dev/null
    if mountpoint -q /mnt/kiseki-pnfs; then
      fio --name=pnfs-agg --directory=/mnt/kiseki-pnfs --rw=read --bs=1m \
        --size=16G --numjobs=4 --direct=1 --group_reporting \
        --output-format=json 2>/dev/null | \
        python3 -c 'import sys,json; d=json.load(sys.stdin); bw=d[\"jobs\"][0][\"read\"][\"bw\"]/1024; gbps=bw*8/1024; print(f\"  client-$((idx+1)): {bw:.1f} MB/s ({gbps:.2f} Gbps)\")' 2>/dev/null
      umount /mnt/kiseki-pnfs 2>/dev/null
    else
      echo '  client-$((idx+1)): mount failed'
    fi
  " 2>/dev/null | tee -a "$RESULTS/pnfs-aggregate.txt" &
  PIDS="$PIDS $!"
done
for pid in $PIDS; do wait $pid 2>/dev/null || true; done
AGG_END=$(date +%s%N)
AGG_MS=$(( (AGG_END - AGG_START) / 1000000 ))
log "  Aggregate wall-clock: ${AGG_MS}ms" | tee -a "$RESULTS/pnfs-aggregate.txt"

# Sum the per-client throughput from the recorded output.
SUM_GBPS=$(grep -oE '[0-9.]+ Gbps' "$RESULTS/pnfs-aggregate.txt" | awk '{sum+=$1} END{print sum}')
log "  Sum of per-client Gbps: ${SUM_GBPS:-N/A} (vs 3× baseline ≈ $(python3 -c "print(3*$BASE_GBPS)" 2>/dev/null) Gbps)" | tee -a "$RESULTS/pnfs-aggregate.txt"

# ---------------------------------------------------------------------------
# 6. mTLS overhead (placeholder — server-side mTLS toggle TBD)
# ---------------------------------------------------------------------------
log ""
log "=== 6. mTLS Overhead ==="
log "  mTLS is on by default in kiseki (ADR-009). To measure overhead, you"
log "  would need a parallel cluster with [security].allow_plaintext = true"
log "  — out of scope for this run." | tee -a "$RESULTS/mtls-overhead.txt"

# ---------------------------------------------------------------------------
# 7. Metrics snapshot
# ---------------------------------------------------------------------------
log ""
log "=== 7. Prometheus Metrics Snapshot ==="
for ip in $ALL_STORAGE; do
  REQS=$(curl -sf "http://$ip:9090/metrics" 2>/dev/null | grep "kiseki_gateway_requests_total" | awk '{sum+=$2} END{print sum+0}')
  CONNS=$(curl -sf "http://$ip:9090/metrics" 2>/dev/null | grep "kiseki_transport_connections_active" | awk '{print $2}' | head -1)
  log "  $ip: gateway_requests=$REQS connections_active=${CONNS:-0}" | tee -a "$RESULTS/metrics-snapshot.txt"
done

log ""
log "╔═══════════════════════════════════════════════════════════════╗"
log "║              Transport-profile benchmark complete            ║"
log "║ Baseline iperf3: ${BASE_GBPS} Gbps                           "
log "║ Results: $RESULTS"
log "╚═══════════════════════════════════════════════════════════════╝"
