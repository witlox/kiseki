#!/bin/bash
# Setup script for Kiseki storage nodes with RAW block devices.
# Disks are NOT mounted — Kiseki DeviceBackend manages them directly.
#
# Variables: node_id, node_ip, all_peers, raft_port, raw_devices, device_class, meta_dir
set -euo pipefail

echo "=== Kiseki storage node ${node_id} (${device_class}) ==="

# Install build dependencies
dnf install -y --allowerasing gcc gcc-c++ openssl-devel cmake make git \
  unzip iperf3 fio curl 2>&1 | tail -3

# Install Rust
if ! command -v rustc &>/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable 2>&1 | tail -3
fi
source "$HOME/.cargo/env" 2>/dev/null || true

# Install protoc
if ! command -v protoc &>/dev/null; then
  curl -sLO https://github.com/protocolbuffers/protobuf/releases/download/v29.5/protoc-29.5-linux-x86_64.zip
  unzip -o protoc-29.5-linux-x86_64.zip -d /usr/local 2>&1 | tail -1
fi

# Clone and build kiseki-server (if not already built)
if [ ! -f /usr/local/bin/kiseki-server ]; then
  echo "Building kiseki-server from source..."
  git clone --depth=1 https://github.com/witlox/kiseki.git /tmp/kiseki 2>&1 | tail -1
  cd /tmp/kiseki
  sed -i 's/default = \["fips"\]/default = []/' crates/kiseki-crypto/Cargo.toml
  cargo build --release --bin kiseki-server --bin kiseki-admin 2>&1 | tail -3
  cp target/release/kiseki-server target/release/kiseki-admin /usr/local/bin/
  echo "Build complete"
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
Environment=KISEKI_BOOTSTRAP=true

# Cluster identity
Environment=KISEKI_NODE_ID=${node_id}
Environment=KISEKI_RAFT_PEERS=${all_peers}
Environment=KISEKI_RAFT_ADDR=${node_ip}:${raft_port}

# Raw device paths for DeviceBackend (comma-separated)
Environment=KISEKI_RAW_DEVICES=${raw_devices}

Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable kiseki-server
systemctl start kiseki-server

echo "=== Node ${node_id} (${device_class}) started ==="
echo "  Metadata:    ${meta_dir}"
echo "  Raw devices: ${raw_devices}"
echo "  Raft:        ${node_ip}:${raft_port}"
echo "  S3:          ${node_ip}:9000"
echo "  NFS:         ${node_ip}:2049"
echo "  Dashboard:   http://${node_ip}:9090/ui"
