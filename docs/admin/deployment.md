# Deployment

This guide covers deploying Kiseki in development, multi-node cluster,
and bare-metal production environments.

---

## Docker Compose (development)

The single-node development stack includes Kiseki plus supporting
services for tracing, KMS, and identity.

### Services

| Service | Image | Ports | Purpose |
|---------|-------|-------|---------|
| `kiseki-server` | `Dockerfile.server` (local build) | 2049, 9000, 9090, 9100, 9101 | Storage node |
| `jaeger` | `jaegertracing/all-in-one:latest` | 4317, 16686 | Distributed tracing (OTLP) |
| `vault` | `hashicorp/vault:1.19` | 8200 | Tenant KMS backend (Transit engine) |
| `keycloak` | `quay.io/keycloak/keycloak:26.0` | 8080 | OIDC identity provider |

### Starting the stack

```bash
# Build and start all services
docker compose up --build

# Run in background for e2e tests
docker compose up --build -d && pytest tests/e2e/
```

### Port map (single-node)

| Port | Protocol | Service |
|------|----------|---------|
| 2049 | TCP | NFS (v3 + v4.2) |
| 9000 | HTTP | S3 gateway |
| 9090 | HTTP | Prometheus metrics + admin dashboard |
| 9100 | gRPC | Data-path (log, chunk, composition, view) |
| 9101 | gRPC | Workflow advisory |
| 4317 | gRPC | Jaeger OTLP receiver |
| 16686 | HTTP | Jaeger UI |
| 8200 | HTTP | Vault API |
| 8080 | HTTP | Keycloak admin console |

### Environment (dev defaults)

The development compose file sets these environment variables on the
`kiseki-server` container:

```yaml
KISEKI_DATA_ADDR: "0.0.0.0:9100"
KISEKI_ADVISORY_ADDR: "0.0.0.0:9101"
KISEKI_S3_ADDR: "0.0.0.0:9000"
KISEKI_NFS_ADDR: "0.0.0.0:2049"
KISEKI_METRICS_ADDR: "0.0.0.0:9090"
KISEKI_DATA_DIR: "/data"
KISEKI_BOOTSTRAP: "true"
OTEL_EXPORTER_OTLP_ENDPOINT: "http://jaeger:4317"
OTEL_SERVICE_NAME: "kiseki-server"
```

The `KISEKI_BOOTSTRAP=true` flag tells the node to create an initial
shard on first start, enabling immediate use without manual cluster
initialization.

### Vault (dev mode)

Vault runs in dev mode with the root token `kiseki-e2e-token`. This is
suitable only for development and testing. The Transit secrets engine is
used by Kiseki as a tenant KMS backend (ADR-028 Provider 2).

```bash
# Verify Vault is ready
curl http://localhost:8200/v1/sys/health
```

### Keycloak (dev mode)

Keycloak runs with `start-dev` and default admin credentials
(`admin`/`admin`). Configure OIDC realms for tenant identity provider
integration.

---

## Docker Compose (3-node cluster)

The multi-node compose file (`docker-compose.3node.yml`) deploys a
3-node Raft cluster for testing consensus, replication, and failover.

### Starting

```bash
docker compose -f docker-compose.3node.yml up --build -d

# Run multi-node tests
KISEKI_E2E_COMPOSE=docker-compose.3node.yml pytest tests/e2e/test_multi_node.py
```

### Node configuration

All three nodes share the same Raft peer list and each has a unique
`KISEKI_NODE_ID`:

| Node | Node ID | Data gRPC | Advisory gRPC | S3 | Raft |
|------|---------|-----------|---------------|----|------|
| `kiseki-node1` | 1 | `localhost:9100` | `localhost:9101` | `localhost:9000` | `9300` |
| `kiseki-node2` | 2 | `localhost:9110` | `localhost:9111` | `localhost:9010` | `9300` |
| `kiseki-node3` | 3 | `localhost:9120` | `localhost:9121` | `localhost:9020` | `9300` |

The Raft peer list is configured identically on all nodes:

```
KISEKI_RAFT_PEERS=1=kiseki-node1:9300,2=kiseki-node2:9300,3=kiseki-node3:9300
```

Node 1 is the bootstrap node. Each node has an independent data volume
(`node1-data`, `node2-data`, `node3-data`).

### Verifying cluster health

```bash
# Check all nodes are healthy
for port in 9100 9110 9120; do
  curl -s http://localhost:${port/9100/9090}/health && echo " :$port OK"
done

# View cluster status via the admin dashboard
open http://localhost:9090/ui
```

---

## Bare metal deployment

### Build from source

Prerequisites: Rust stable toolchain, protobuf compiler (`protoc`),
OpenSSL development headers, `pkg-config`.

```bash
# Clone and build
git clone https://github.com/your-org/kiseki.git
cd kiseki

# Release build (all binaries)
cargo build --release

# Binaries produced:
# target/release/kiseki-server      — storage node
# target/release/kiseki-keyserver    — system key manager (HA)
# target/release/kiseki-client-fuse  — FUSE client for compute nodes
# target/release/kiseki-control      — control plane
```

Optional feature flags:

```bash
# Enable CXI/Slingshot transport (requires libfabric)
cargo build --release --features kiseki-transport/cxi

# Enable RDMA verbs transport
cargo build --release --features kiseki-transport/verbs

# Enable tenant opt-in compression
cargo build --release --features kiseki-chunk/compression
```

### Disk layout

Each storage node should follow the recommended disk layout:

```
Server node:
  System partition (RAID-1 on 2x SSD):
    /var/lib/kiseki/raft/log.redb       Raft log entries
    /var/lib/kiseki/keys/epochs.redb    Key epoch metadata
    /var/lib/kiseki/chunks/meta.redb    Chunk extent index
    /var/lib/kiseki/small/objects.redb   Small-file inline content
    /var/lib/kiseki/config/             Node config, TLS certs

  Data devices (JBOD, managed by Kiseki):
    /dev/nvme0n1 -> pool "fast-nvme"
    /dev/nvme1n1 -> pool "fast-nvme"
    /dev/sda     -> pool "bulk-ssd"
    /dev/sdb     -> pool "cold-hdd"
```

JBOD for data devices, RAID-1 for the system partition. Kiseki manages
data durability via EC/replication across JBOD members. The system
partition uses RAID-1 because redb and Raft log must survive a single
disk failure without Kiseki's own repair mechanism.

### systemd unit: kiseki-server

```ini
[Unit]
Description=Kiseki Storage Node
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
User=kiseki
Group=kiseki
ExecStart=/usr/local/bin/kiseki-server
Restart=on-failure
RestartSec=5

# Environment
Environment=KISEKI_DATA_ADDR=0.0.0.0:9100
Environment=KISEKI_ADVISORY_ADDR=0.0.0.0:9101
Environment=KISEKI_S3_ADDR=0.0.0.0:9000
Environment=KISEKI_NFS_ADDR=0.0.0.0:2049
Environment=KISEKI_METRICS_ADDR=0.0.0.0:9090
Environment=KISEKI_DATA_DIR=/var/lib/kiseki
Environment=KISEKI_NODE_ID=1
Environment=KISEKI_RAFT_PEERS=1=node1.example.com:9300,2=node2.example.com:9300,3=node3.example.com:9300
Environment=KISEKI_RAFT_ADDR=0.0.0.0:9300

# TLS
Environment=KISEKI_CA_PATH=/etc/kiseki/tls/ca.crt
Environment=KISEKI_CERT_PATH=/etc/kiseki/tls/server.crt
Environment=KISEKI_KEY_PATH=/etc/kiseki/tls/server.key

# Observability
Environment=OTEL_EXPORTER_OTLP_ENDPOINT=http://jaeger.internal:4317
Environment=OTEL_SERVICE_NAME=kiseki-server
Environment=RUST_LOG=kiseki=info

# Security hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/kiseki
PrivateTmp=yes
MemoryDenyWriteExecute=yes
LimitCORE=0

[Install]
WantedBy=multi-user.target
```

### systemd unit: kiseki-keyserver

```ini
[Unit]
Description=Kiseki System Key Manager
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=kiseki-keys
Group=kiseki-keys
ExecStart=/usr/local/bin/kiseki-keyserver
Restart=on-failure
RestartSec=5

Environment=KISEKI_DATA_DIR=/var/lib/kiseki-keys
Environment=KISEKI_RAFT_PEERS=1=keysrv1:9400,2=keysrv2:9400,3=keysrv3:9400
Environment=KISEKI_RAFT_ADDR=0.0.0.0:9400
Environment=KISEKI_CA_PATH=/etc/kiseki/tls/ca.crt
Environment=KISEKI_CERT_PATH=/etc/kiseki/tls/keyserver.crt
Environment=KISEKI_KEY_PATH=/etc/kiseki/tls/keyserver.key

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/kiseki-keys
PrivateTmp=yes
MemoryDenyWriteExecute=yes
LimitCORE=0

[Install]
WantedBy=multi-user.target
```

### systemd unit: kiseki-client-fuse

```ini
[Unit]
Description=Kiseki FUSE Client
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/kiseki-client-fuse --mountpoint /mnt/kiseki
ExecStop=/bin/fusermount -u /mnt/kiseki
Restart=on-failure
RestartSec=5

Environment=KISEKI_DATA_ADDR=node1.example.com:9100,node2.example.com:9100,node3.example.com:9100
Environment=KISEKI_CA_PATH=/etc/kiseki/tls/ca.crt
Environment=KISEKI_CERT_PATH=/etc/kiseki/tls/client.crt
Environment=KISEKI_KEY_PATH=/etc/kiseki/tls/client.key
Environment=KISEKI_CACHE_MODE=organic
Environment=KISEKI_CACHE_DIR=/var/cache/kiseki
Environment=KISEKI_CACHE_L1_MAX=1073741824
Environment=KISEKI_CACHE_L2_MAX=107374182400

[Install]
WantedBy=multi-user.target
```

---

## Configuration checklist

Before starting a production cluster, verify the following:

### TLS certificates

- [ ] Cluster CA certificate generated and distributed to all nodes
- [ ] Per-node server certificate signed by Cluster CA
- [ ] Per-tenant client certificates signed by Cluster CA
- [ ] Key manager server certificate signed by Cluster CA
- [ ] CRL distribution point configured (if using CRL-based revocation)
- [ ] Certificate SANs include all node hostnames and IP addresses
- [ ] All certificates use ECDSA P-256 or RSA 2048+ keys

### Data directories

- [ ] `KISEKI_DATA_DIR` exists and is owned by the `kiseki` user
- [ ] System partition has sufficient capacity for metadata (see
  [Capacity Planning](../operations/capacity.md))
- [ ] Data devices formatted and accessible (raw block or file-backed)
- [ ] Separate RAID-1 for system partition

### Bootstrap

- [ ] Exactly one node has `KISEKI_BOOTSTRAP=true` on first start
- [ ] After initial bootstrap, set `KISEKI_BOOTSTRAP=false` on the
  bootstrap node (or remove the variable)
- [ ] `KISEKI_RAFT_PEERS` is identical on all nodes
- [ ] `KISEKI_NODE_ID` is unique per node
- [ ] System key manager cluster is started before storage nodes

### Network

- [ ] Data-fabric ports (9100, 9101) reachable between all nodes
- [ ] Raft port (9300) reachable between all nodes
- [ ] Metrics port (9090) accessible to monitoring infrastructure
- [ ] NFS port (2049) accessible to clients
- [ ] S3 port (9000) accessible to clients
- [ ] Management network separated from data fabric (recommended)

### Observability

- [ ] Jaeger or OTLP-compatible collector endpoint configured
- [ ] Prometheus scrape target added for each node's `:9090/metrics`
- [ ] `RUST_LOG` level set appropriately (production: `kiseki=info`)

---

## Health verification

After deployment, verify the cluster is healthy:

### HTTP health endpoint

```bash
# Returns "OK" when the node is ready
curl http://node1:9090/health
```

### Prometheus metrics

```bash
# Verify metrics are being exported
curl -s http://node1:9090/metrics | head -20
```

### Admin dashboard

Open `http://node1:9090/ui` in a browser. The dashboard shows:

- Cluster health (nodes healthy / total)
- Raft entries applied
- Gateway requests served
- Data written and read
- Active transport connections

Any node in the cluster serves the full cluster-wide view by scraping
metrics from its peers.

### Raft consensus

Verify that the Raft cluster has elected a leader:

```bash
# Check the cluster status via the admin API
curl -s http://node1:9090/ui/api/cluster | jq .
```

### S3 connectivity

```bash
# Test S3 access (if a tenant namespace is configured)
aws --endpoint-url http://node1:9000 s3 ls
```

### NFS connectivity

```bash
# Test NFS mount
mount -t nfs node1:/ /mnt/kiseki -o vers=4.2
```

### FUSE client

```bash
# Mount via FUSE (on a compute node)
kiseki-client-fuse --mountpoint /mnt/kiseki
ls /mnt/kiseki
```
