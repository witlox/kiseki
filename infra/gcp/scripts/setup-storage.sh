#!/bin/bash
# Setup script for Kiseki storage nodes.
# Templatefile variables: node_id, node_ip, peer_ips, data_dir, disk_type, metrics_port
set -euo pipefail

echo "=== Kiseki storage node ${node_id} setup ==="

# Install dependencies
dnf install -y --allowerasing nfs-utils fio iperf3 wget tar

# Set up data directory based on disk type
case "${disk_type}" in
  local-nvme)
    # Local NVMe SSDs are at /dev/nvme0n1, /dev/nvme0n2
    mkfs.xfs -f /dev/nvme0n1 2>/dev/null || true
    mkdir -p ${data_dir}
    mount /dev/nvme0n1 ${data_dir} || true
    echo "/dev/nvme0n1 ${data_dir} xfs defaults,noatime 0 0" >> /etc/fstab
    ;;
  pd-ssd|pd-balanced)
    # Attached persistent disk
    DEV="/dev/disk/by-id/google-kiseki-data"
    if [ -b "$DEV" ]; then
      mkfs.xfs -f "$DEV" 2>/dev/null || true
      mkdir -p ${data_dir}
      mount "$DEV" ${data_dir} || true
      echo "$DEV ${data_dir} xfs defaults,noatime 0 0" >> /etc/fstab
    fi
    ;;
esac

mkdir -p ${data_dir}/{raft,keys,small,chunks}

# Download kiseki-server from latest release
ARCH=$(uname -m)
RELEASE_URL="https://github.com/witlox/kiseki/releases/latest/download/kiseki-server-$${ARCH}.tar.gz"
cd /tmp
wget -q "$RELEASE_URL" -O kiseki-server.tar.gz 2>/dev/null || echo "Release not available yet — build from source"
if [ -f kiseki-server.tar.gz ]; then
  tar xzf kiseki-server.tar.gz -C /usr/local/bin/ 2>/dev/null || true
fi

# Create systemd service
cat > /etc/systemd/system/kiseki-server.service <<EOF
[Unit]
Description=Kiseki Storage Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/kiseki-server
Restart=always
RestartSec=5
LimitNOFILE=65536

Environment=KISEKI_DATA_ADDR=0.0.0.0:9100
Environment=KISEKI_ADVISORY_ADDR=0.0.0.0:9101
Environment=KISEKI_ADVISORY_STREAM_ADDR=0.0.0.0:9102
Environment=KISEKI_S3_ADDR=0.0.0.0:9000
Environment=KISEKI_NFS_ADDR=0.0.0.0:2049
Environment=KISEKI_METRICS_ADDR=0.0.0.0:${metrics_port}
Environment=KISEKI_DATA_DIR=${data_dir}
Environment=KISEKI_BOOTSTRAP=true
Environment=KISEKI_NODE_ID=${node_id}
Environment=KISEKI_RAFT_PEERS=${peer_ips}
Environment=KISEKI_RAFT_ADDR=${node_ip}:9300
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable kiseki-server

# Start if binary exists
if [ -f /usr/local/bin/kiseki-server ]; then
  systemctl start kiseki-server
  echo "Kiseki storage node ${node_id} started on ${node_ip}"
else
  echo "Kiseki binary not found — install manually or build from source"
fi

echo "=== Storage node ${node_id} setup complete ==="
