#!/usr/bin/env bash
# Generate test-only TLS material for the docker-compose cluster.
#
# Produces under ./tests/e2e/.tls/ (gitignored):
#   ca.pem          — root CA, used to sign both server and client
#   ca.key          — root CA private key (insecure: test-only)
#   server.pem      — server cert with SANs for all 3 nodes
#   server.key      — server private key
#   client.pem      — client cert (one identity for the test client)
#   client.key      — client private key
#
# Re-running is safe and idempotent: existing files are reused unless
# `--force` is passed. The gen is fast (<1s) because we use ECDSA P-256
# keys, not RSA.
#
# Why one server cert with multiple SANs? RustTLS verifies the SAN
# against the SNI from the client. mount.nfs4 uses the hostname from
# the mount target ('kiseki-node1', etc.) — listing all three node
# names + 'localhost' as SANs lets the same cert serve every node.
#
# Phase 16a step 12 — the cert also carries `spiffe://cluster/fabric/<id>`
# URI SANs (one per node) so the ClusterChunkService SAN-role
# interceptor accepts inbound peer-to-peer fabric calls. In production
# each node would carry its own per-node fabric cert; for the test rig
# we share one cert with all three node identities listed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TLS_DIR="${SCRIPT_DIR}/.tls"
mkdir -p "${TLS_DIR}"

force=0
if [[ "${1:-}" == "--force" ]]; then
    force=1
fi

if [[ -f "${TLS_DIR}/ca.pem" && "${force}" == "0" ]]; then
    echo "TLS material already present in ${TLS_DIR} (use --force to regen)"
    exit 0
fi

cd "${TLS_DIR}"

# 1. Root CA — ECDSA P-256, 10-year validity (test-only).
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -nodes -key ca.key -sha256 -days 3650 \
    -subj "/CN=kiseki-test-ca/O=kiseki-test" \
    -out ca.pem 2>/dev/null

# 2. Server cert — SANs cover all 3 cluster nodes + localhost.
openssl ecparam -name prime256v1 -genkey -noout -out server.key
cat > server.csr.cnf <<'EOF'
[req]
default_bits = 2048
prompt = no
distinguished_name = req_dn
req_extensions = req_ext
[req_dn]
CN = kiseki-node1
O = kiseki-test
[req_ext]
subjectAltName = @alt
[alt]
DNS.1 = kiseki-node1
DNS.2 = kiseki-node2
DNS.3 = kiseki-node3
DNS.4 = localhost
IP.1 = 127.0.0.1
URI.1 = spiffe://cluster/fabric/kiseki-node1
URI.2 = spiffe://cluster/fabric/kiseki-node2
URI.3 = spiffe://cluster/fabric/kiseki-node3
EOF
openssl req -new -key server.key -config server.csr.cnf -out server.csr 2>/dev/null
cat > server.ext.cnf <<'EOF'
subjectAltName = @alt
extendedKeyUsage = serverAuth, clientAuth
[alt]
DNS.1 = kiseki-node1
DNS.2 = kiseki-node2
DNS.3 = kiseki-node3
DNS.4 = localhost
IP.1 = 127.0.0.1
URI.1 = spiffe://cluster/fabric/kiseki-node1
URI.2 = spiffe://cluster/fabric/kiseki-node2
URI.3 = spiffe://cluster/fabric/kiseki-node3
EOF
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out server.pem -days 3650 -sha256 -extfile server.ext.cnf 2>/dev/null

# 3. Client cert — for the privileged test container's tlshd.
openssl ecparam -name prime256v1 -genkey -noout -out client.key
openssl req -new -key client.key \
    -subj "/CN=kiseki-test-client/O=kiseki-test" \
    -out client.csr 2>/dev/null
cat > client.ext.cnf <<'EOF'
extendedKeyUsage = clientAuth
EOF
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out client.pem -days 3650 -sha256 -extfile client.ext.cnf 2>/dev/null

rm -f server.csr server.csr.cnf server.ext.cnf client.csr client.ext.cnf ca.srl

echo "Generated TLS material in ${TLS_DIR}/"
ls -l "${TLS_DIR}/"
