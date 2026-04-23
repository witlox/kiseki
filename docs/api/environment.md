# Environment Variables

All Kiseki configuration is done via environment variables. No
configuration files are used for runtime settings (I-K8: keys are never
stored in configuration files).

---

## Server configuration

| Variable | Type | Default | Description |
|---|---|---|---|
| `KISEKI_DATA_ADDR` | `SocketAddr` | `0.0.0.0:9100` | Data-path gRPC listener address |
| `KISEKI_ADVISORY_ADDR` | `SocketAddr` | `0.0.0.0:9101` | Advisory gRPC listener address (isolated runtime) |
| `KISEKI_S3_ADDR` | `SocketAddr` | `0.0.0.0:9000` | S3 HTTP gateway listener address |
| `KISEKI_NFS_ADDR` | `SocketAddr` | `0.0.0.0:2049` | NFS server listener address |
| `KISEKI_METRICS_ADDR` | `SocketAddr` | `0.0.0.0:9090` | Prometheus metrics and admin UI listener address |
| `KISEKI_DATA_DIR` | `PathBuf` | (none) | Persistent storage directory for redb databases. If unset, runs in-memory only. |
| `KISEKI_NODE_ID` | `u64` | `0` | Raft node ID. 0 = single-node mode. |
| `KISEKI_BOOTSTRAP` | `bool` | `false` | Create a well-known bootstrap shard on startup. Set to `true` or `1` for development/testing. |

---

## TLS configuration

TLS is enabled when all three path variables are set. Otherwise the server
runs in plaintext mode (development only, logged as a warning).

| Variable | Type | Default | Description |
|---|---|---|---|
| `KISEKI_CA_PATH` | `PathBuf` | (none) | Cluster CA certificate PEM file |
| `KISEKI_CERT_PATH` | `PathBuf` | (none) | Node certificate chain PEM file |
| `KISEKI_KEY_PATH` | `PathBuf` | (none) | Node private key PEM file |
| `KISEKI_CRL_PATH` | `PathBuf` | (none) | Optional CRL PEM file for certificate revocation |

---

## Raft configuration

| Variable | Type | Default | Description |
|---|---|---|---|
| `KISEKI_RAFT_ADDR` | `SocketAddr` | (none) | Raft RPC listen address. Required for multi-node clusters. |
| `KISEKI_RAFT_PEERS` | `String` | (empty) | Comma-separated peer list in `id=addr` format, e.g. `1=10.0.0.1:9200,2=10.0.0.2:9200,3=10.0.0.3:9200` |

---

## Metadata capacity (ADR-030)

| Variable | Type | Default | Description |
|---|---|---|---|
| `KISEKI_META_SOFT_LIMIT_PCT` | `u8` | `50` | Soft limit percentage for system disk metadata usage. Exceeding triggers inline threshold reduction. |
| `KISEKI_META_HARD_LIMIT_PCT` | `u8` | `75` | Hard limit percentage for system disk metadata usage. Exceeding forces inline threshold to INLINE_FLOOR and emits alert (I-SF2). |
| `KISEKI_RAFT_INLINE_MBPS` | `u32` | `10` | Per-shard Raft inline throughput cap in MB/s. Prevents inline data from starving metadata-only Raft operations (I-SF7). |

---

## Client cache configuration

| Variable | Type | Default | Description |
|---|---|---|---|
| `KISEKI_CACHE_MODE` | `String` | `organic` | Cache mode: `organic` (LRU), `pinned` (staging-driven), or `bypass` (no caching) |
| `KISEKI_CACHE_DIR` | `PathBuf` | `/tmp/kiseki-cache` | L2 cache pool directory on local NVMe |
| `KISEKI_CACHE_L2_MAX` | `u64` | 53687091200 (50 GB) | Maximum L2 cache size in bytes |
| `KISEKI_CACHE_POOL_ID` | `String` | (generated) | Adopt an existing L2 pool (128-bit hex). Used for staging handoff between processes. |

---

## Transport configuration

| Variable | Type | Default | Description |
|---|---|---|---|
| `KISEKI_IB_DEVICE` | `String` | (auto-detect) | InfiniBand device name for RDMA verbs transport. If unset, auto-detects the first available device. |

---

## Observability

Standard Rust/tokio observability variables:

| Variable | Type | Default | Description |
|---|---|---|---|
| `RUST_LOG` | `String` | `info` | Log filter directive (e.g., `kiseki_log=debug,kiseki_raft=trace`) |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `String` | (none) | OpenTelemetry collector endpoint for distributed tracing |

---

## Example: single-node development

```bash
export KISEKI_DATA_DIR=/var/lib/kiseki
export KISEKI_BOOTSTRAP=true
kiseki-server
```

## Example: three-node cluster

```bash
# Node 1
export KISEKI_NODE_ID=1
export KISEKI_DATA_DIR=/var/lib/kiseki
export KISEKI_RAFT_ADDR=10.0.0.1:9200
export KISEKI_RAFT_PEERS=1=10.0.0.1:9200,2=10.0.0.2:9200,3=10.0.0.3:9200
export KISEKI_CA_PATH=/etc/kiseki/ca.pem
export KISEKI_CERT_PATH=/etc/kiseki/node1.pem
export KISEKI_KEY_PATH=/etc/kiseki/node1-key.pem
export KISEKI_BOOTSTRAP=true
kiseki-server
```
