#!/bin/bash
# Setup script for benchmark controller node.
# Templatefile variables: storage_ips, client_ips, perf_bucket, release_tag,
#                         binary_url_base, profile, bench_suite
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
wget -q "${binary_url_base}/kiseki-server-$${ARCH}.tar.gz" -O /tmp/kiseki-server.tar.gz 2>/dev/null || true
if [ -f /tmp/kiseki-server.tar.gz ]; then
  tar xzf /tmp/kiseki-server.tar.gz -C /usr/local/bin/ kiseki-admin 2>/dev/null || true
fi

# Closes #54: stage the benchmark scripts.
#
# Pre-fix /opt/kiseki-bench/ shipped EMPTY except for results/, so
# every fresh GCP cluster needed the operator to tee perf-common.sh
# and perf-suite.sh up over ssh before any phase could run. The
# 2026-05-16 run wasted ~15 min on this dance.
#
# The benchmarks tarball is uploaded to the same staging bucket
# (binary_url_base) as the kiseki binaries — same operator workflow,
# same `gcloud storage cp` step. The `bench`/perf-common.sh path
# closes the loop with the per-phase / sticky-RUN_ID work in #55.
mkdir -p /opt/kiseki-bench/results
wget -q "${binary_url_base}/benchmarks.tar.gz" -O /tmp/benchmarks.tar.gz 2>/dev/null || true
if [ -s /tmp/benchmarks.tar.gz ]; then
  tar xzf /tmp/benchmarks.tar.gz -C /opt/kiseki-bench/ 2>&1 | tail -5
  # Make the driver + every phase executable. The tarball preserves
  # mode bits from the source tree, but some operators upload via
  # tools that strip exec bits (e.g. gsutil-as-windows), so be
  # defensive here.
  chmod +x /opt/kiseki-bench/bench 2>/dev/null || true
  chmod +x /opt/kiseki-bench/*.sh 2>/dev/null || true
  chmod +x /opt/kiseki-bench/phases/*.sh 2>/dev/null || true
  chmod +x /opt/kiseki-bench/tests/*.sh 2>/dev/null || true
  echo "Benchmark scripts staged at /opt/kiseki-bench/"
  ls /opt/kiseki-bench/ | sed 's/^/  /'
else
  echo "WARNING: benchmarks.tar.gz not found at ${binary_url_base}/" >&2
  echo "         operators must upload it alongside the kiseki binaries," >&2
  echo "         or scripts must be staged manually via ssh." >&2
fi

# Store cluster info — sourced by every perf-suite-*.sh and metrics-collector.sh
cat > /etc/kiseki-bench.env <<EOF
STORAGE_IPS="${storage_ips}"
CLIENT_IPS="${client_ips}"
FIRST_STORAGE=$(echo "${storage_ips}" | cut -d',' -f1)
KISEKI_PERF_BUCKET="${perf_bucket}"
KISEKI_PROFILE="${profile}"
KISEKI_BENCH_SUITE="${bench_suite}"
# Tenant UUID used by `kiseki-client bench` and the bench namespace
# topology (`OrgId(Uuid::from_u128(1))` in `default_ids`, matched by
# the server's `bootstrap_tenant`). Threaded into setup-shards.sh
# and any operator tooling that needs it.
KISEKI_BENCH_TENANT_ID="00000000-0000-0000-0000-000000000001"
EOF

# Register SSH key with OS Login for ctrl→node access.
# The ctrl service account has roles/compute.osAdminLogin,
# so this key grants SSH to all project instances.
if [ ! -f /root/.ssh/id_ed25519 ]; then
  ssh-keygen -t ed25519 -N "" -f /root/.ssh/id_ed25519 2>/dev/null
fi
gcloud compute os-login ssh-keys add --key-file=/root/.ssh/id_ed25519.pub --ttl=30d 2>/dev/null || true

# Store the OS Login username for the perf-suite's node_ssh wrapper.
OS_USER=$(gcloud compute os-login describe-profile --format='value(posixAccounts[0].username)' 2>/dev/null || echo root)
echo "SSH_USER=$OS_USER" >> /etc/kiseki-bench.env
echo "SSH key registered (OS Login user: $OS_USER)"

# Post-boot topology setup: create multi-shard `bench-ns-<i>`
# namespaces so `kiseki-client bench --namespace-fanout N` actually
# fans PUTs across multiple shard leaders (#66 fix 2, #68
# endpoint, #69 this script). Best-effort: if leader convergence
# is slow or the admin endpoint isn't ready, log + continue —
# phase 00's shard-count probe is the secondary safety net.
if [ -x /opt/kiseki-bench/setup-shards.sh ]; then
  echo "=== Setting up bench namespace topology ==="
  bash /opt/kiseki-bench/setup-shards.sh 2>&1 | sed 's/^/  /' \
    || echo "  (setup-shards.sh returned non-zero — phase 00 will fail-loud if topology is incomplete)"
else
  echo "WARNING: /opt/kiseki-bench/setup-shards.sh not present — bench fanout will collapse to bootstrap shard" >&2
fi

echo "=== Benchmark controller ready ==="
echo "Profile:        ${profile}"
echo "Storage nodes:  ${storage_ips}"
echo "Client nodes:   ${client_ips}"
echo "Results bucket: ${perf_bucket}"
echo "Bench suite:    ${bench_suite}"
echo "Run:            /opt/kiseki-bench/${bench_suite}"
