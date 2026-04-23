#!/bin/bash
# Setup script for benchmark controller node.
# Templatefile variables: storage_ips, client_ips
set -euo pipefail

echo "=== Benchmark controller setup ==="

dnf install -y --allowerasing python3 python3-pip jq bc wget fio nfs-utils iperf3
pip3 install --break-system-packages boto3 awscli tabulate 2>/dev/null || pip3 install boto3 awscli tabulate

# Download kiseki-admin
ARCH=$(uname -m)
wget -q "https://github.com/witlox/kiseki/releases/latest/download/kiseki-server-$${ARCH}.tar.gz" -O /tmp/kiseki-server.tar.gz 2>/dev/null || true
if [ -f /tmp/kiseki-server.tar.gz ]; then
  tar xzf /tmp/kiseki-server.tar.gz -C /usr/local/bin/ kiseki-admin 2>/dev/null || true
fi

# Store cluster info
cat > /etc/kiseki-bench.env <<EOF
STORAGE_IPS="${storage_ips}"
CLIENT_IPS="${client_ips}"
FIRST_STORAGE=$(echo "${storage_ips}" | cut -d',' -f1)
EOF

echo "=== Benchmark controller ready ==="
echo "Storage nodes: ${storage_ips}"
echo "Client nodes: ${client_ips}"
echo "Run: /opt/kiseki-bench/run-all-benchmarks.sh"
