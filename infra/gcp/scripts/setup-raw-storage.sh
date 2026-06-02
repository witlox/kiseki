#!/bin/bash
# Setup script for Kiseki storage nodes with RAW block devices.
# Disks are NOT mounted — Kiseki DeviceBackend manages them directly.
#
# Variables: node_id, node_ip, all_peers, raft_port, raw_devices, device_class, meta_dir
set -eo pipefail

# GCE metadata runner doesn't set HOME or full PATH — fix it
export HOME="$${HOME:-/root}"
export PATH="$$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$${PATH:-}"

echo "=== Kiseki storage node ${node_id} (${device_class}) ==="

# Install runtime dependencies
dnf install -y --allowerasing openssl-libs unzip iperf3 fio curl bc tar gzip 2>&1 | tail -3

# Download pre-built release binaries
if [ ! -f /usr/local/bin/kiseki-server ]; then
  ARCH=$(uname -m)
  RELEASE_URL="${binary_url_base}/kiseki-server-$${ARCH}.tar.gz"
  echo "Downloading kiseki-server ($${ARCH}) from $RELEASE_URL ..."
  curl -sfL "$RELEASE_URL" -o /tmp/kiseki-server.tar.gz || {
    echo "ERROR: Failed to download from $RELEASE_URL"
    exit 1
  }
  tar xzf /tmp/kiseki-server.tar.gz -C /usr/local/bin/
  chmod +x /usr/local/bin/kiseki-server /usr/local/bin/kiseki-admin
  echo "Installed kiseki-server and kiseki-admin"
fi

# Create metadata directory (on boot disk — NOT on raw devices)
mkdir -p ${meta_dir}/{raft,keys,small,chunks}

# Verify raw devices exist
echo "Raw devices (${device_class}):"
IFS=',' read -ra DEVS <<< "${raw_devices}"
FAST_DEV_COUNT=0
for dev in "$${DEVS[@]}"; do
  if [ -b "$dev" ]; then
    SIZE=$(blockdev --getsize64 "$dev" 2>/dev/null || echo "?")
    echo "  $dev: $((SIZE / 1024 / 1024 / 1024)) GB — raw (no filesystem)"
    # Detect fast storage: /sys/block/<dev>/queue/rotational == 0
    # signals SSD/NVMe (per ADR-030 §1). A node with no fast device is
    # operationally "boot-disk fallback" — the runtime will warn at
    # boot per ADR-030 amendment.
    DEV_BASENAME=$(basename "$dev")
    ROTATIONAL_PATH="/sys/block/$${DEV_BASENAME}/queue/rotational"
    if [ -r "$${ROTATIONAL_PATH}" ] && [ "$(cat $${ROTATIONAL_PATH})" = "0" ]; then
      FAST_DEV_COUNT=$((FAST_DEV_COUNT + 1))
    fi
  else
    echo "  $dev: NOT FOUND"
  fi
done

# ADR-030 (2026-05-31 amendment) — admin-driven metadata device role.
# `setup-raw-storage.sh` does NOT auto-promote any device to the
# metadata role; every raw device goes to the chunk pool. The runtime
# emits its own loud `cluster_warnings` ERROR when KISEKI_DATA_DIR
# lives on the boot disk and no device has been assigned to the
# metadata role via `kiseki-admin pool add-device metadata-pool ...`.
#
# Mirror that warning here at provision time so operators see the
# guidance early — before the cluster boots and the runtime warning
# starts firing.
if [ "$${FAST_DEV_COUNT}" -gt 0 ]; then
  cat <<WARNING
==========================================================================
NOTICE (ADR-030 2026-05-31 amendment): $${FAST_DEV_COUNT} fast (NVMe/SSD)
device(s) detected, but NONE is currently assigned to the metadata-pool
role. Metadata + small-tier writes will live on the boot disk until
operator action.

Recommended for production:
  kiseki-admin pool add-device metadata-pool <one-of-the-fast-devices>

The runtime will emit a persistent cluster_warning until this is done.
See specs/architecture/adr/030-dynamic-small-file-placement.md and
docs/performance/capacity-planning.md.
==========================================================================
WARNING
else
  cat <<WARNING
==========================================================================
WARNING (ADR-030): NO fast (NVMe/SSD) device detected on this node.
Metadata + small-tier writes will live on the boot disk. This is
"emergency fallback" mode per ADR-030 amendment and is operationally
unsuitable for production (boot-disk write latency, capacity ceiling).

Provision at least one NVMe/SSD device per node and assign it to the
metadata pool via `kiseki-admin pool add-device metadata-pool <dev>`.
==========================================================================
WARNING
fi

# Create Kiseki device config — lists raw block devices for DeviceBackend
# The server reads this to initialize its device pool
cat > ${meta_dir}/devices.json <<EOF
{
  "node_id": ${node_id},
  "device_class": "${device_class}",
  "devices": [
$(IFS=','; i=0; for dev in ${raw_devices}; do
    [ $i -gt 0 ] && echo ","
    echo -n "    {\"path\": \"$dev\", \"class\": \"${device_class}\", \"pool\": \"default\"}"
    i=$((i+1))
done)
  ]
}
EOF
echo "Device config: ${meta_dir}/devices.json"
cat ${meta_dir}/devices.json

# Create systemd service for kiseki-server
cat > /etc/systemd/system/kiseki-server.service <<EOF
[Unit]
Description=Kiseki Storage Server (node ${node_id}, ${device_class})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/kiseki-server
Restart=always
RestartSec=5
LimitNOFILE=65536

# Core config
Environment=KISEKI_DATA_ADDR=0.0.0.0:9100
Environment=KISEKI_ADVISORY_ADDR=0.0.0.0:9101
Environment=KISEKI_ADVISORY_STREAM_ADDR=0.0.0.0:9102
# ADR-042 §2.2 TCP-framed native binding. Default was 9101 in the
# original ADR but that collided with ADR-021 advisory which also
# defaults to 9101 — TCP-framed lost the bind race and the listener
# silently exited (observed on the 2026-05-07 GCP compact run). The
# server-side default is now 9103; this line makes the choice
# explicit in the systemd unit so an operator looking at the file
# sees the port plan at a glance.
Environment=KISEKI_NATIVE_TCP_ADDR=0.0.0.0:9103
# ADR-042 §4 server-side proxy fallback. With multi-shard topology
# (e.g. the default profile's 18 shards across 6 nodes) a single-
# endpoint native/FUSE/NFS client maps ~(N-1)/N of its writes to
# shards led by a peer node; the local gateway gets ForwardToLeader
# and would 5xx without this fallback. The Rust-side default is now
# `on` (GH #97) — this line keeps the choice explicit in the unit
# so an operator reading the file sees the routing decision.
Environment=KISEKI_NATIVE_PROXY_FALLBACK=on
Environment=KISEKI_S3_ADDR=0.0.0.0:9000
Environment=KISEKI_NFS_ADDR=0.0.0.0:2049
Environment=KISEKI_METRICS_ADDR=0.0.0.0:9090

# Metadata on boot disk (fast SSD), data on raw devices
Environment=KISEKI_DATA_DIR=${meta_dir}
# ADR-049 phase 5a: device-inventory tags so the catalog policy
# can target `Tag("nvme-fast")` etc. The boot SSD hosts
# `KISEKI_DATA_DIR` (metadata + raft log fjall). Raw-block-device
# entries go via `KISEKI_RAW_DEVICES` above (orthogonal axis per
# §D11.1). Operator can extend this list when additional mount
# points are configured.
Environment=KISEKI_DEVICE_TAGS=${meta_dir}=data-dir-default
# ADR-049 §D2.5: Raft log path is bootstrap-only — never resolver-
# routed. Defaults to `${meta_dir}/raft` when unset; we set it
# explicitly so an operator changing `KISEKI_DATA_DIR` later
# doesn't accidentally orphan the Raft log.
Environment=KISEKI_RAFT_LOG_DIR=${meta_dir}/raft
# Only node 1 bootstraps (seeds the Raft cluster).
# Other nodes join as followers via Raft RPCs from the leader.
%{ if node_id == 1 ~}
Environment=KISEKI_BOOTSTRAP=true
%{ else ~}
Environment=KISEKI_BOOTSTRAP=false
%{ endif ~}

# Cluster identity
Environment=KISEKI_NODE_ID=${node_id}
Environment=KISEKI_RAFT_PEERS=${all_peers}
Environment=KISEKI_RAFT_ADDR=${node_ip}:${raft_port}

# Raw device paths for DeviceBackend (comma-separated)
Environment=KISEKI_RAW_DEVICES=${raw_devices}

# Raft runtime threads — needs to exceed max concurrent writes to avoid
# blocking on redb I/O in the state machine apply path.
Environment=KISEKI_RAFT_THREADS=64

# F-1 (2026-05-15 GCP perf-run finding): without this, every composition
# write does an immediate fjall fsync. Under sustained NFS-write load
# the hydrator's apply_hydration_batch fsync becomes the dominant cost
# (~50 ops/sec observed). 100ms eventual-durability mode buffers WAL
# appends and drives a periodic fsync — the loss-window is bounded to
# the interval, and Raft + the under-replication scrub re-replicate
# any compositions lost on a leader crash. Documented as safe for
# multi-node deployments in runtime.rs's open-fjall path.
Environment=KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100

# ADR-038 §D4.2: plaintext NFS fallback (no TLS bundle in perf-test env)
Environment=KISEKI_INSECURE_NFS=true
Environment=KISEKI_ALLOW_PLAINTEXT_NFS=true

# Closes #52: without this, /cluster/info on the metrics port returns
# `auth misconfigured` because the perf-test deployment has no
# KISEKI_ADMIN_TOKEN/KISEKI_CLIENT_TOKEN set. perf-common.sh's
# `discover_leader` needs an unauthenticated read of /cluster/info to
# find the Raft leader; the local docker-compose.3node.yml already
# sets this for the same reason. Production deployments that DO set
# admin/client tokens should leave this off.
Environment=KISEKI_CLUSTER_INFO_PUBLIC=true

# Closes #56: kiseki-admin hits /ui/api/cluster which is admin-gated.
# Without this, kiseki-admin status reported `Nodes: 0/0` against a
# healthy cluster because the 401 body parsed as zero counts. Mirrors
# the local docker-compose.3node.yml `KISEKI_ADMIN_AUTH_DISABLED:
# "true"` posture for the same perf-test reason as #52.
Environment=KISEKI_ADMIN_AUTH_DISABLED=true

# Eventual-durability flush cadence (ADR-022 rev-3 + d5c56ad).
# All three are CRITICAL for perf:
#   * KISEKI_RAFT_FLUSH_INTERVAL_MS unset → Raft log uses
#     sync-per-write fsync, capping PUT at ~31 k op/s. Setting it
#     switches to periodic-flush; multi-node Raft re-replicates the
#     loss window on restart so the durability contract is
#     preserved at the cluster level.
#   * KISEKI_COMPOSITION_FLUSH_INTERVAL_MS — same shape for the
#     composition store (fjall). Without it every PUT fsyncs the
#     composition row inline.
#   * KISEKI_CHUNK_FLUSH_INTERVAL_MS — chunk-data device sync. The
#     periodic task runs regardless of the env var (defaults to
#     100 ms when unset); set it explicitly so per-knob tuning is
#     visible in the systemd unit.
# 100 ms = 1 minor election timeout = the same cadence ADR-022 §4
# uses for the loss-window analysis in docs/operations/durability.md.
Environment=KISEKI_RAFT_FLUSH_INTERVAL_MS=100
Environment=KISEKI_COMPOSITION_FLUSH_INTERVAL_MS=100
Environment=KISEKI_CHUNK_FLUSH_INTERVAL_MS=100

# 2026-06-01 — Raft TCP transport per-peer connection cap. The in-code
# default was raised to 256 in the same commit that added these lines;
# they are written here explicitly so future operators see the choice
# in the systemd unit and can tune without rebuilding.
#
# Why this matters: on the 2026-06-01 instrumented run (default cap = 16
# at the time), `raft_transport_rpc{op=append_entries}` mean ballooned
# to 129 ms (vs ~150 µs same-zone GCP RTT floor). Journals showed
# `rejecting Raft RPC connection — per-peer cap exceeded peer=… active=17`.
# 18 shards × min_acks=2 fan = up to 36 inflight per follower; the
# 16-slot cap rejected, leader retried, AppendEntries RTT exploded.
# See specs/performance/2026-06-01-gcp-instrumented-single-client.md.
#
#   * KISEKI_RAFT_PER_PEER_MAX — server-side inbound cap.
#   * KISEKI_RAFT_CONN_POOL_PER_PEER — client-side outbound pool size.
#
# 128 is double the typical 18-shard fan × 2 followers = 36 with
# headroom for retransmits + the leaderless quorum-write producer.
Environment=KISEKI_RAFT_PER_PEER_MAX=128
Environment=KISEKI_RAFT_CONN_POOL_PER_PEER=128

# 2026-05-09: bump kiseki_chunk_cluster to debug so wrapper-layer
# warnings (peer GetFragment timeouts → surfaced as ChunkError::Io
# per the wrapper fix in commit a69e490) land in the journal. Lets
# us post-mortem any read-path stall on the cluster instead of
# only seeing the user-visible Io error.
Environment=RUST_LOG=info,kiseki_chunk_cluster=debug

# pprof CPU flamegraph dump on graceful shutdown (pprof feature). Harmless
# when the binary was built without the feature — the env var is unread.
Environment=KISEKI_PPROF_OUT=/var/log/kiseki-pprof.svg

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable kiseki-server

# Stagger startup: node 1 starts first, others wait for node 1's
# Raft RPC port to be reachable before starting. This gives node 1
# time to initialize the Raft group and begin leader election before
# followers join.
if [ "${node_id}" -eq 1 ]; then
  echo "Node 1: starting first (cluster seed)"
  systemctl start kiseki-server
else
  SEED_IP=$(echo "${all_peers}" | tr ',' '\n' | grep '^1=' | cut -d= -f2 | cut -d: -f1)
  SEED_PORT=$(echo "${all_peers}" | tr ',' '\n' | grep '^1=' | cut -d= -f2 | cut -d: -f2)
  echo "Waiting for seed node ($SEED_IP:$SEED_PORT) ..."
  for i in $(seq 1 120); do
    if curl -sf --connect-timeout 2 "http://$SEED_IP:9090/health" >/dev/null 2>&1; then
      echo "  Seed node ready after $${i}s"
      break
    fi
    sleep 1
  done
  # Brief delay to let Raft initialize on seed before followers join
  sleep 3
  systemctl start kiseki-server
fi

echo "=== Node ${node_id} (${device_class}) started ==="
echo "  Metadata:    ${meta_dir}"
echo "  Raw devices: ${raw_devices}"
echo "  Raft:        ${node_ip}:${raft_port}"
echo "  S3:          ${node_ip}:9000"
echo "  NFS:         ${node_ip}:2049"
echo "  Dashboard:   http://${node_ip}:9090/ui"
echo "  Cluster:     http://${node_ip}:9090/cluster/info"
