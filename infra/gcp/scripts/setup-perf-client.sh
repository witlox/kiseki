#!/bin/bash
# Setup script for Kiseki performance test client nodes.
# Runs FUSE mount, NFS mount, and benchmark tools.
#
# Variables: storage_ips, cache_dev, client_id
set -euo pipefail

echo "=== Kiseki client ${client_id} setup ==="

# Install dependencies
dnf install -y --allowerasing nfs-utils fuse3 fuse3-devel fio iperf3 \
  python3 python3-pip curl wget git gcc gcc-c++ openssl-devel cmake make unzip 2>&1 | tail -3
pip3 install --break-system-packages boto3 awscli 2>/dev/null || true

# Install Rust (for building kiseki-client)
if ! command -v rustc &>/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable 2>&1 | tail -3
fi
source "$HOME/.cargo/env" 2>/dev/null || true

# Install protoc
if ! command -v protoc &>/dev/null; then
  curl -sLO https://github.com/protocolbuffers/protobuf/releases/download/v29.5/protoc-29.5-linux-x86_64.zip
  unzip -o protoc-29.5-linux-x86_64.zip -d /usr/local 2>&1 | tail -1
fi

# Build kiseki-client with FUSE support
if [ ! -f /usr/local/bin/kiseki-client ]; then
  echo "Building kiseki-client..."
  git clone --depth=1 https://github.com/witlox/kiseki.git /tmp/kiseki 2>&1 | tail -1
  cd /tmp/kiseki
  sed -i 's/default = \["fips"\]/default = []/' crates/kiseki-crypto/Cargo.toml
  cargo build --release --bin kiseki-client --features fuse -p kiseki-client 2>&1 | tail -3
  cp target/release/kiseki-client /usr/local/bin/ 2>/dev/null || true
  # Also build kiseki-admin for diagnostics
  cargo build --release --bin kiseki-admin 2>&1 | tail -3
  cp target/release/kiseki-admin /usr/local/bin/ 2>/dev/null || true
fi

# Format cache disk (this one IS a filesystem — for L2 cache)
if [ -b "${cache_dev}" ]; then
  mkfs.xfs -f ${cache_dev} 2>/dev/null || true
  mkdir -p /cache
  mount ${cache_dev} /cache 2>/dev/null || true
  echo "${cache_dev} /cache xfs defaults,noatime 0 0" >> /etc/fstab
fi
mkdir -p /cache

# Set up NFS mount points (all 5 storage nodes)
IFS=',' read -ra STORAGES <<< "${storage_ips}"
for i in "$${!STORAGES[@]}"; do
  ip="$${STORAGES[$i]}"
  mnt="/mnt/kiseki-nfs-$((i+1))"
  mkdir -p "$mnt"
  echo "$ip:/ $mnt nfs4 defaults,noatime,soft,timeo=30 0 0" >> /etc/fstab
done

# Set up FUSE mount point
mkdir -p /mnt/kiseki-fuse

# Configure AWS CLI for S3 tests
FIRST_STORAGE=$(echo "${storage_ips}" | cut -d',' -f1)
mkdir -p ~/.aws
cat > ~/.aws/config <<EOF
[default]
region = us-east-1
s3 =
  endpoint_url = http://$${FIRST_STORAGE}:9000
  signature_version = s3v4
EOF

# Environment for benchmarks
cat > /etc/kiseki-client.env <<EOF
STORAGE_IPS="${storage_ips}"
FIRST_STORAGE=$${FIRST_STORAGE}
CACHE_DIR=/cache
CLIENT_ID=${client_id}
KISEKI_CACHE_MODE=organic
KISEKI_CACHE_DIR=/cache
KISEKI_CACHE_L2_MAX=85899345920
KISEKI_CACHE_META_TTL_MS=5000
EOF

echo "=== Client ${client_id} ready ==="
echo "  NFS mounts: /mnt/kiseki-nfs-{1..5}"
echo "  FUSE mount: /mnt/kiseki-fuse"
echo "  Cache dir:  /cache ($(df -h /cache 2>/dev/null | tail -1 | awk '{print $2}'))"
echo "  S3 endpoint: http://$${FIRST_STORAGE}:9000"
