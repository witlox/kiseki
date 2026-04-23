# Kiseki

Kiseki is a distributed storage system built for HPC and AI workloads.
It provides a unified data plane that serves files and objects through
multiple protocol gateways (S3, NFS, FUSE) while handling encryption,
replication, and caching transparently.

## Key Features

- **S3 and NFS gateways** -- access the same data through S3-compatible
  HTTP, NFSv3/v4.2, or a native FUSE mount. Protocol gateways translate
  wire protocols into operations on the shared log-structured data model.

- **Client-side cache with staging** -- a two-tier cache (L1 in-memory,
  L2 local NVMe) on compute nodes eliminates repeated fabric traversals.
  Three modes (pinned, organic, bypass) match the dominant workload
  patterns: epoch-reuse training, mixed inference, and streaming ingest.

- **Per-shard Raft consensus** -- every shard is a single-tenant Raft
  group. Deltas (metadata mutations) are totally ordered within a shard
  and replicated to a quorum before acknowledgement.

- **Erasure coding and placement** -- chunks are stored across affinity
  pools with configurable EC profiles. The placement engine distributes
  data across device classes (fast-NVMe, bulk-NVMe) and rebuilds lost
  chunks from parity.

- **FIPS 140-2/3 encryption** -- always-on, two-layer envelope
  encryption. System DEKs (AES-256-GCM via aws-lc-rs) encrypt chunk
  data; tenant KEKs wrap the DEKs for access control. Five tenant KMS
  backends: Kiseki-Internal, HashiCorp Vault, KMIP 2.1, AWS KMS,
  PKCS#11.

- **GPU-direct and fabric transports** -- the native client selects the
  fastest available transport: libfabric/CXI (Slingshot), RDMA verbs,
  or TCP+TLS. Transport selection is automatic based on fabric discovery.

- **Multi-tenant isolation** -- tenant hierarchy
  (organization / project / workload) with per-level quotas, compliance
  tags, and key isolation. Shards are single-tenant. Cross-tenant data
  access is out of scope by design.

- **OIDC and mTLS authentication** -- Keycloak (or any OIDC provider)
  for identity; Cluster CA-signed mTLS certificates for data-fabric
  authentication. Certificates work on the SAN with no control plane
  access needed on the hot path.

- **Workflow advisory** -- a bidirectional advisory channel carries
  workload hints (access pattern, prefetch range, priority) inbound and
  telemetry feedback (backpressure, locality, staleness) outbound. The
  advisory path is side-by-side with the data path -- it never blocks or
  delays data operations.

## Architecture at a Glance

Kiseki is a single-language Rust system organized as 18 crates in a
Cargo workspace:

| Layer | Crates |
|-------|--------|
| Foundation | `kiseki-common`, `kiseki-proto`, `kiseki-crypto`, `kiseki-transport` |
| Data path | `kiseki-log`, `kiseki-block`, `kiseki-chunk`, `kiseki-composition`, `kiseki-view` |
| Protocol | `kiseki-gateway` (NFS + S3) |
| Client | `kiseki-client` (FUSE, FFI, Python via PyO3) |
| Infrastructure | `kiseki-raft`, `kiseki-keymanager`, `kiseki-audit`, `kiseki-advisory`, `kiseki-control` |
| Integration | `kiseki-server`, `kiseki-acceptance` |

The data model is log-structured: mutations are recorded as **deltas**
appended to per-shard Raft logs. **Compositions** describe how
content-addressed, encrypted **chunks** assemble into files or objects.
**Views** are materialized projections of shard state, maintained
incrementally by stream processors and served by protocol gateways.

Four binaries are produced:

| Binary | Role |
|--------|------|
| `kiseki-server` | Storage node (log + chunk + composition + view + gateways + audit + advisory) |
| `kiseki-keyserver` | HA system key manager (Raft-replicated) |
| `kiseki-client-fuse` | Compute-node FUSE mount with native client |
| `kiseki-control` | Control plane (tenancy, IAM, policy, federation) |

## Target Workloads

| Workload | How Kiseki helps |
|----------|-----------------|
| **LLM training** | Tokenized datasets staged once per job, served from local NVMe cache across epochs. Pinned cache mode prevents eviction. |
| **LLM inference** | Model weights cold-started into cache on first load, then served locally for all replicas on the node. |
| **Climate / weather simulation** | Boundary conditions staged with hard deadline via Slurm prolog. Input files cached; checkpoint writes bypass the cache. |
| **HPC checkpoint/restart** | Checkpoint writes go straight to canonical (bypass mode). Restart reads benefit from organic caching if the same node is reused. |

## Quick Links

- [Getting Started](guide/getting-started.md) -- Docker Compose quickstart
- [S3 API](guide/s3-api.md) -- supported operations, examples
- [NFS Access](guide/nfs-access.md) -- NFSv3/v4.2 mount instructions
- [FUSE Mount](guide/fuse-mount.md) -- native client mount
- [Python SDK](guide/python-sdk.md) -- PyO3 bindings
- [Client Cache & Staging](guide/client-cache.md) -- ADR-031 cache modes
