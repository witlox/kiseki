#!/bin/bash
# Setup script for benchmark controller node.
# Templatefile variables: storage_ips, client_ips, perf_bucket
set -eo pipefail

export HOME="$${HOME:-/root}"
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$${PATH:-}"

echo "=== Benchmark controller setup ==="

dnf install -y --allowerasing python3 python3-pip jq bc wget fio nfs-utils iperf3 \
  curl openssh-clients 2>&1 | tail -3
pip3 install --break-system-packages boto3 awscli tabulate 2>/dev/null || \
  pip3 install boto3 awscli tabulate

# Install Google Cloud CLI for gsutil (result upload)
if ! command -v gsutil &>/dev/null; then
  echo "Installing Google Cloud CLI..."
  dnf install -y --allowerasing google-cloud-cli 2>&1 | tail -3 || {
    # Fallback: add repo manually
    cat > /etc/yum.repos.d/google-cloud-sdk.repo <<'REPO'
[google-cloud-cli]
name=Google Cloud CLI
baseurl=https://packages.cloud.google.com/yum/repos/cloud-sdk-el9-x86_64
enabled=1
gpgcheck=1
repo_gpgcheck=0
gpgkey=https://packages.cloud.google.com/yum/doc/rpm-package-key.gpg
REPO
    dnf install -y google-cloud-cli 2>&1 | tail -3
  }
fi

# Download kiseki-admin
ARCH=$(uname -m)
wget -q "https://github.com/witlox/kiseki/releases/latest/download/kiseki-server-$${ARCH}.tar.gz" -O /tmp/kiseki-server.tar.gz 2>/dev/null || true
if [ -f /tmp/kiseki-server.tar.gz ]; then
  tar xzf /tmp/kiseki-server.tar.gz -C /usr/local/bin/ kiseki-admin 2>/dev/null || true
fi

# Create benchmark directory
mkdir -p /opt/kiseki-bench/results

# Store cluster info
cat > /etc/kiseki-bench.env <<EOF
STORAGE_IPS="${storage_ips}"
CLIENT_IPS="${client_ips}"
FIRST_STORAGE=$(echo "${storage_ips}" | cut -d',' -f1)
KISEKI_PERF_BUCKET="${perf_bucket}"
EOF

# Generate SSH key for passwordless access to client/storage nodes
if [ ! -f /root/.ssh/id_ed25519 ]; then
  ssh-keygen -t ed25519 -N "" -f /root/.ssh/id_ed25519 2>/dev/null
fi

echo "=== Benchmark controller ready ==="
echo "Storage nodes: ${storage_ips}"
echo "Client nodes: ${client_ips}"
echo "Results bucket: ${perf_bucket}"
echo "Run: /opt/kiseki-bench/perf-suite.sh"
