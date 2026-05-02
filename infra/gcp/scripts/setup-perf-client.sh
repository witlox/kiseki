#!/bin/bash
# Setup script for Kiseki performance-test CPU client nodes.
# Used by both the "default" and "transport" profiles. Sets up FUSE+NFS
# mount points, the L2 cache disk, and a sane S3 endpoint default.
#
# Variables: storage_ips, cache_dev, client_id, release_tag, binary_url_base, profile
set -eo pipefail

# GCE metadata runner doesn't set HOME or full PATH — fix it
export HOME="$${HOME:-/root}"
export PATH="$$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$${PATH:-}"

echo "=== Kiseki client ${client_id} setup ==="

# Install runtime dependencies
dnf install -y --allowerasing nfs-utils fuse3 fio iperf3 \
  python3 python3-pip curl wget unzip bc tar gzip 2>&1 | tail -3
pip3 install --break-system-packages boto3 awscli 2>/dev/null || true

# Download pre-built release binaries
if [ ! -f /usr/local/bin/kiseki-client ]; then
  ARCH=$(uname -m)
  echo "Downloading kiseki-client ($${ARCH}) from ${binary_url_base} ..."
  curl -sfL "${binary_url_base}/kiseki-client-$${ARCH}.tar.gz" \
    -o /tmp/kiseki-client.tar.gz || {
    echo "ERROR: Failed to download client tarball"
    exit 1
  }
  tar xzf /tmp/kiseki-client.tar.gz -C /usr/local/bin/
  chmod +x /usr/local/bin/kiseki-client 2>/dev/null || true

  # Also grab kiseki-admin for diagnostics
  curl -sfL "${binary_url_base}/kiseki-server-$${ARCH}.tar.gz" \
    -o /tmp/kiseki-server.tar.gz || true
  if [ -f /tmp/kiseki-server.tar.gz ]; then
    tar xzf /tmp/kiseki-server.tar.gz -C /usr/local/bin/ kiseki-admin 2>/dev/null || true
  fi
  echo "Installed kiseki-client and kiseki-admin"
fi

# Format cache disk (this one IS a filesystem — for L2 cache)
if [ -b "${cache_dev}" ]; then
  mkfs.xfs -f ${cache_dev} 2>/dev/null || true
  mkdir -p /cache
  mount ${cache_dev} /cache 2>/dev/null || true
  echo "${cache_dev} /cache xfs defaults,noatime 0 0" >> /etc/fstab
fi
mkdir -p /cache

# Set up NFS mount points (one per storage node, count determined by profile).
# fstab entries left as `noauto`-equivalent: the perf suites mount on demand.
IFS=',' read -ra STORAGES <<< "${storage_ips}"
for i in "$${!STORAGES[@]}"; do
  ip="$${STORAGES[$i]}"
  mnt="/mnt/kiseki-nfs-$((i+1))"
  mkdir -p "$mnt"
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
KISEKI_CLIENT_PROFILE=${profile}
KISEKI_CACHE_MODE=organic
KISEKI_CACHE_DIR=/cache
KISEKI_CACHE_L2_MAX=85899345920
KISEKI_CACHE_META_TTL_MS=5000
EOF

NUM_STORAGES=$${#STORAGES[@]}
echo "=== Client ${client_id} ready ==="
echo "  Profile:      ${profile}"
echo "  NFS mounts:   /mnt/kiseki-nfs-{1..$NUM_STORAGES}"
echo "  FUSE mount:   /mnt/kiseki-fuse"
echo "  Cache dir:    /cache ($(df -h /cache 2>/dev/null | tail -1 | awk '{print $2}'))"
echo "  S3 endpoint:  http://$${FIRST_STORAGE}:9000"
