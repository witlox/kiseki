#!/bin/bash
# Setup script for kiseki GPU client nodes (gpu profile).
#
# Targets the Google Deep Learning VM image (Debian 11 + CUDA 12.x +
# nvidia drivers preinstalled). Adds: nvidia-fs / cuFile, kiseki-client,
# NFS+FUSE mount points, L2 cache disk.
#
# Variables: storage_ips, cache_dev, client_id, release_tag, profile
set -eo pipefail

export HOME="$${HOME:-/root}"
export PATH="/usr/local/cuda/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$${PATH:-}"

echo "=== Kiseki GPU client ${client_id} setup (profile=${profile}) ==="

# The DLVM image is Debian-based. apt instead of dnf.
export DEBIAN_FRONTEND=noninteractive

# Wait for the GCE NVIDIA-driver installer (post-boot) to finish before
# poking nvidia-fs. The DLVM image runs google_install_nvidia_drivers on
# first boot and locks dpkg until it's done.
for i in $(seq 1 120); do
  if [ ! -f /var/lib/dpkg/lock-frontend ] || ! fuser /var/lib/dpkg/lock-frontend &>/dev/null; then
    break
  fi
  sleep 5
done

apt-get update -qq
apt-get install -y -qq nfs-common fuse3 fio iperf3 jq python3 python3-pip \
  curl wget bc tar gzip 2>&1 | tail -3

# nvidia-fs is the kernel module behind cuFile / GDS. The DLVM image ships
# CUDA but the kernel module is shipped separately.
if ! lsmod | grep -q nvidia_fs; then
  echo "Installing nvidia-fs (cuFile kernel module)..."
  apt-get install -y -qq nvidia-fs nvidia-gds-12-3 2>&1 | tail -3 || \
    apt-get install -y -qq nvidia-fs 2>&1 | tail -3 || true
  modprobe nvidia-fs 2>/dev/null || \
    echo "WARNING: nvidia-fs module not loadable — cuFile will use bounce buffers"
fi

# Default cuFile config — kiseki-client links libcufile and reads this
# at first GDS open(). We only set the bare minimum; the rest of the
# defaults are fine for benchmark use.
if [ ! -f /etc/cufile.json ]; then
  cat > /etc/cufile.json <<'CUFILE_EOF'
{
  "logging": {
    "level": "ERROR"
  },
  "execution": {
    "max_io_threads": 4,
    "max_request_parallelism": 4,
    "parallel_io": true
  },
  "properties": {
    "max_direct_io_size_kb": 16384,
    "use_poll_mode": false,
    "allow_compat_mode": true
  }
}
CUFILE_EOF
fi

# Download kiseki-client from a GitHub release.
if [ ! -f /usr/local/bin/kiseki-client ]; then
  ARCH=$(uname -m)
  echo "Downloading kiseki-client ($${ARCH}) from ${release_tag}..."
  curl -sfL "https://github.com/witlox/kiseki/releases/download/${release_tag}/kiseki-client-$${ARCH}.tar.gz" \
    -o /tmp/kiseki-client.tar.gz || {
    echo "ERROR: Failed to download client release"
    exit 1
  }
  tar xzf /tmp/kiseki-client.tar.gz -C /usr/local/bin/
  chmod +x /usr/local/bin/kiseki-client 2>/dev/null || true

  # kiseki-admin for diagnostics
  curl -sfL "https://github.com/witlox/kiseki/releases/download/${release_tag}/kiseki-server-$${ARCH}.tar.gz" \
    -o /tmp/kiseki-server.tar.gz || true
  if [ -f /tmp/kiseki-server.tar.gz ]; then
    tar xzf /tmp/kiseki-server.tar.gz -C /usr/local/bin/ kiseki-admin 2>/dev/null || true
  fi
fi

# Format + mount the L2 cache disk (PD-SSD attached as kiseki-cache).
if [ -b "${cache_dev}" ]; then
  if ! blkid "${cache_dev}" >/dev/null 2>&1; then
    mkfs.xfs -f ${cache_dev} 2>&1 | tail -2
  fi
  mkdir -p /cache
  mountpoint -q /cache || mount ${cache_dev} /cache
  grep -q "${cache_dev} /cache" /etc/fstab || \
    echo "${cache_dev} /cache xfs defaults,noatime 0 0" >> /etc/fstab
fi
mkdir -p /cache /mnt/kiseki-fuse

# NFS mount points (one per storage node). Mounting itself happens in the suite.
IFS=',' read -ra STORAGES <<< "${storage_ips}"
for i in "$${!STORAGES[@]}"; do
  mkdir -p "/mnt/kiseki-nfs-$((i+1))"
done

# AWS CLI config — the perf suite uses curl directly, but boto3-based
# tooling on the box benefits from a sane default endpoint.
FIRST_STORAGE=$(echo "${storage_ips}" | cut -d',' -f1)
mkdir -p ~/.aws
cat > ~/.aws/config <<EOF
[default]
region = us-east-1
s3 =
  endpoint_url = http://$${FIRST_STORAGE}:9000
  signature_version = s3v4
EOF

cat > /etc/kiseki-client.env <<EOF
STORAGE_IPS="${storage_ips}"
FIRST_STORAGE=$${FIRST_STORAGE}
CACHE_DIR=/cache
CLIENT_ID=${client_id}
KISEKI_CLIENT_PROFILE=${profile}
KISEKI_CACHE_MODE=organic
KISEKI_CACHE_DIR=/cache
# Larger L2 budget for ML datasets — 800 GB of the 1 TB cache disk.
KISEKI_CACHE_L2_MAX=858993459200
KISEKI_CACHE_META_TTL_MS=5000
EOF

echo "=== GPU client ${client_id} ready ==="
echo "  GPU:             $(nvidia-smi -L 2>/dev/null | head -1 || echo 'driver not yet ready')"
echo "  nvidia-fs:       $(lsmod | grep -q nvidia_fs && echo loaded || echo not-loaded)"
echo "  Cache:           /cache ($(df -h /cache 2>/dev/null | tail -1 | awk '{print $2}'))"
echo "  S3 endpoint:     http://$${FIRST_STORAGE}:9000"
echo "  Storage hosts:   ${storage_ips}"
