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
for dev in "$${DEVS[@]}"; do
  if [ -b "$dev" ]; then
    SIZE=$(blockdev --getsize64 "$dev" 2>/dev/null || echo "?")
    echo "  $dev: $((SIZE / 1024 / 1024 / 1024)) GB — raw (no filesystem)"
  else
    echo "  $dev: NOT FOUND"
  fi
done

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
Environment=KISEKI_S3_ADDR=0.0.0.0:9000
Environment=KISEKI_NFS_ADDR=0.0.0.0:2049
Environment=KISEKI_METRICS_ADDR=0.0.0.0:9090

# Metadata on boot disk (fast SSD), data on raw devices
Environment=KISEKI_DATA_DIR=${meta_dir}
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

# ADR-038 §D4.2: plaintext NFS fallback (no TLS bundle in perf-test env)
Environment=KISEKI_INSECURE_NFS=true
Environment=KISEKI_ALLOW_PLAINTEXT_NFS=true

Environment=RUST_LOG=info

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
