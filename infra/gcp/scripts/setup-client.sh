#!/bin/bash
# Setup script for Kiseki client/benchmark nodes.
# Templatefile variables: storage_ips, cache_dir, role
set -euo pipefail

echo "=== Kiseki client node setup (role: ${role}) ==="

# Install common dependencies
dnf install -y --allowerasing nfs-utils fio iperf3 wget tar fuse3 fuse3-devel \
  python3 python3-pip jq bc

# Install benchmark tools
pip3 install --break-system-packages boto3 awscli 2>/dev/null || pip3 install boto3 awscli

# Set up cache directory
if [ -b "/dev/disk/by-id/google-kiseki-cache" ]; then
  mkfs.xfs -f /dev/disk/by-id/google-kiseki-cache 2>/dev/null || true
  mkdir -p ${cache_dir}
  mount /dev/disk/by-id/google-kiseki-cache ${cache_dir} || true
fi
mkdir -p ${cache_dir}

# Download kiseki-client from latest release
ARCH=$(uname -m)
RELEASE_URL="https://github.com/witlox/kiseki/releases/latest/download/kiseki-client-$${ARCH}.tar.gz"
cd /tmp
wget -q "$RELEASE_URL" -O kiseki-client.tar.gz 2>/dev/null || echo "Release not available yet"
if [ -f kiseki-client.tar.gz ]; then
  tar xzf kiseki-client.tar.gz -C /usr/local/bin/ 2>/dev/null || true
fi

# Configure AWS CLI for S3 benchmarks (pointing to kiseki S3 gateway)
FIRST_STORAGE=$(echo "${storage_ips}" | cut -d',' -f1)
mkdir -p ~/.aws
cat > ~/.aws/config <<EOF
[default]
region = us-east-1
s3 =
  endpoint_url = http://$${FIRST_STORAGE}:9000
  signature_version = s3v4
EOF

# Set up NFS mount point
mkdir -p /mnt/kiseki-nfs
echo "$${FIRST_STORAGE}:/ /mnt/kiseki-nfs nfs4 defaults,noatime,soft,timeo=30 0 0" >> /etc/fstab

# Role-specific setup
case "${role}" in
  s3-bench)
    echo "S3 benchmark node ready"
    echo "Run: aws s3 ls --endpoint-url http://$${FIRST_STORAGE}:9000"
    ;;
  gpu-bench)
    # Install NVIDIA drivers (T4)
    dnf install -y kernel-devel kernel-headers
    dnf install -y https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/cuda-repo-rhel9-12-6-local-12.6.3_560.35.05-1.x86_64.rpm 2>/dev/null || true
    dnf install -y cuda-toolkit-12-6 2>/dev/null || echo "CUDA install deferred — run manually"
    echo "GPU benchmark node ready (CUDA install may need reboot)"
    ;;
  nfs-fuse-bench)
    # Mount NFS
    mount /mnt/kiseki-nfs 2>/dev/null || echo "NFS mount deferred until storage nodes are up"
    echo "NFS/FUSE benchmark node ready"
    ;;
esac

echo "=== Client node setup complete (role: ${role}) ==="
