# Getting Started

This guide walks through running a single-node Kiseki stack with Docker
Compose, verifying the deployment, and performing basic S3 operations.

## Prerequisites

- **Docker** 24+ with Compose V2 (`docker compose`)
- **curl** (for health checks)
- **aws-cli** (optional, for S3 operations)

If building from source instead of Docker:

- **Rust** 1.78+ (stable)
- **Protobuf compiler** (`protoc`)

## Quick Start with Docker Compose

The repository includes a `docker-compose.yml` that brings up a
single-node Kiseki server with supporting services:

| Service | Port | Purpose |
|---------|------|---------|
| `kiseki-server` | 9000 | S3 HTTP gateway |
| `kiseki-server` | 2049 | NFS (v3 + v4.2) |
| `kiseki-server` | 9090 | Prometheus metrics |
| `kiseki-server` | 9100 | Data-path gRPC |
| `kiseki-server` | 9101 | Advisory gRPC |
| `jaeger` | 16686 | Tracing UI |
| `jaeger` | 4317 | OTLP gRPC receiver |
| `vault` | 8200 | HashiCorp Vault (dev mode, tenant KMS) |
| `keycloak` | 8080 | Keycloak (OIDC identity provider) |

Start the stack:

```bash
docker compose up --build -d
```

Wait for all services to become healthy:

```bash
docker compose ps
```

The `kiseki-server` container sets `KISEKI_BOOTSTRAP=true`, which
creates an initial shard for immediate use.

## Verify the Deployment

### Health Check

The data-path gRPC port responds to TCP connections when the server is
ready:

```bash
# TCP probe on the data-path port
timeout 1 bash -c 'echo > /dev/tcp/127.0.0.1/9100'
echo $?  # 0 = healthy
```

### Prometheus Metrics

```bash
curl -s http://localhost:9090/metrics | head -20
```

### Jaeger Tracing

Open [http://localhost:16686](http://localhost:16686) in a browser to
view distributed traces. The server exports traces via OTLP to Jaeger
automatically.

### Vault (Dev Mode)

Vault runs in dev mode with root token `kiseki-e2e-token`:

```bash
curl -s http://localhost:8200/v1/sys/health | python3 -m json.tool
```

### Keycloak

Keycloak is available at [http://localhost:8080](http://localhost:8080)
with admin credentials `admin` / `admin`.

## S3 Operations

With `aws-cli` configured to point at the local S3 gateway:

```bash
# Configure a local profile (no real AWS credentials needed)
export AWS_ACCESS_KEY_ID=kiseki
export AWS_SECRET_ACCESS_KEY=kiseki
export AWS_DEFAULT_REGION=us-east-1

# Create a bucket (maps to a Kiseki namespace)
aws --endpoint-url http://localhost:9000 s3 mb s3://test-bucket

# Upload a file
echo "hello kiseki" > /tmp/hello.txt
aws --endpoint-url http://localhost:9000 s3 cp /tmp/hello.txt s3://test-bucket/hello.txt

# Download and verify
aws --endpoint-url http://localhost:9000 s3 cp s3://test-bucket/hello.txt /tmp/hello-back.txt
cat /tmp/hello-back.txt
```

Or with `curl` directly:

```bash
# List buckets
curl -s http://localhost:9000/

# PUT an object
curl -X PUT http://localhost:9000/test-bucket/greeting.txt \
     -d "hello from curl"

# GET it back
curl -s http://localhost:9000/test-bucket/greeting.txt
```

## Multi-Node Cluster

A three-node cluster configuration is also provided:

```bash
docker compose -f docker-compose.3node.yml up --build -d
```

This starts three `kiseki-server` instances that form Raft groups for
shard replication.

## Building from Source

```bash
# Clone and build
git clone https://github.com/your-org/kiseki.git
cd kiseki
cargo build --release

# Run the server
KISEKI_BOOTSTRAP=true \
KISEKI_DATA_DIR=/tmp/kiseki-data \
KISEKI_S3_ADDR=0.0.0.0:9000 \
KISEKI_NFS_ADDR=0.0.0.0:2049 \
KISEKI_DATA_ADDR=0.0.0.0:9100 \
KISEKI_METRICS_ADDR=0.0.0.0:9090 \
  ./target/release/kiseki-server
```

## Next Steps

- [S3 API](s3-api.md) -- full list of supported S3 operations
- [NFS Access](nfs-access.md) -- mount via NFS
- [FUSE Mount](fuse-mount.md) -- native client mount on compute nodes
- [Python SDK](python-sdk.md) -- use Kiseki from Python workloads
- [Client Cache & Staging](client-cache.md) -- pre-stage datasets for
  training jobs
