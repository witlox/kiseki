# System Overview

Kiseki is a distributed storage system designed for HPC and AI workloads.
It provides a unified data fabric with POSIX (FUSE), NFS, and S3 access
paths, two-layer encryption with tenant-controlled crypto-shred, and
pluggable HPC transports (CXI/Slingshot, InfiniBand, RoCEv2).

## Workspace structure

The codebase is a single Rust workspace with 18 crates:

| Crate | Purpose |
|---|---|
| `kiseki-common` | Shared types, HLC, identifiers, errors |
| `kiseki-proto` | Generated protobuf/gRPC code |
| `kiseki-crypto` | FIPS AEAD (AES-256-GCM), envelope encryption, tenant KMS providers |
| `kiseki-raft` | Shared Raft config, redb log store, TCP transport |
| `kiseki-transport` | Transport abstraction: TCP+TLS, RDMA verbs, CXI/libfabric |
| `kiseki-log` | Log context: delta ordering, shard lifecycle, Raft consensus |
| `kiseki-block` | Raw block device I/O, bitmap allocator, superblock (ADR-029) |
| `kiseki-chunk` | Chunk storage: placement, erasure coding, GC, device management |
| `kiseki-composition` | Composition context: namespace, refcount, multipart |
| `kiseki-view` | View materialization: stream processors, MVCC pins |
| `kiseki-gateway` | Protocol gateway: NFS and S3 translation |
| `kiseki-client` | Native client: FUSE, transport selection, client-side cache |
| `kiseki-keymanager` | System key manager with Raft HA |
| `kiseki-audit` | Append-only audit log with per-tenant shards |
| `kiseki-advisory` | Workflow advisory: hints, telemetry, budgets (ADR-020/021) |
| `kiseki-control` | Control plane: tenancy, IAM, policy, federation |
| `kiseki-server` | Storage node binary (composes all server-side crates) |
| `kiseki-acceptance` | BDD acceptance tests (cucumber-rs) |

## Bounded contexts

The domain is organized into eight bounded contexts, each with a distinct
responsibility, failure domain, and scaling concern:

1. **Log** -- Delta ordering, Raft consensus, shard lifecycle
2. **Chunk Storage** -- Encrypted chunk persistence, placement, EC, GC
3. **Composition** -- Tenant-scoped metadata assembly, namespace management
4. **View Materialization** -- Protocol-shaped materialized projections
5. **Protocol Gateway** -- NFS and S3 wire protocol translation
6. **Control Plane** -- Tenancy, IAM, quota, policy, federation
7. **Key Management** -- System DEK/KEK, tenant KMS providers, crypto-shred
8. **Workflow Advisory** -- Client hints, telemetry feedback (cross-cutting)

Additionally, **Native Client** runs on compute nodes as a separate trust
boundary and **Block I/O** handles raw device management underneath chunk
storage.

## Data path

```
Client (plaintext) ──encrypt──► Gateway / Native Client
                                       │
                                       ▼
                                  Composition
                                  (assemble chunks, record delta)
                                       │
                              ┌────────┴────────┐
                              ▼                 ▼
                          Log (Raft)       Chunk Storage
                     (commit delta,      (write encrypted
                      replicate)          chunk to device)
```

**Write path**: The client (native or protocol) encrypts data with the
tenant KEK wrapping a system DEK. The composition layer assembles chunk
references and records a delta. The delta is committed through Raft on
the owning shard. Chunks are written to affinity pools with erasure coding.

**Read path**: The client issues a view lookup (materialized from log
deltas). The view resolves chunk references. Chunks are read from devices,
decrypted, and returned to the client.

## Control path

```
Admin ──► Control Plane (gRPC)
              │
              ├── Tenant / Namespace / Quota / Policy
              ├── Flavor management
              ├── Federation (async cross-site)
              └── Advisory policy (hint budgets, profiles)
```

The control plane manages tenant lifecycle, IAM, quotas, compliance tags,
placement policy, and federation. It communicates with storage nodes via
gRPC on the management network. The control plane depends only on
`kiseki-common` and `kiseki-proto` (crate-graph firewall, ADR-027).

## Advisory path (ADR-020)

```
Client ──hints──► Advisory Runtime ──telemetry──► Client
                      │
                      ├── Route hints to Chunk / View / Composition
                      ├── Emit caller-scoped telemetry feedback
                      └── Audit advisory events
```

The workflow advisory system is a cross-cutting concern (not a bounded
context). It carries two flows over a bidirectional gRPC channel per
declared workflow:

- **Hints** (client to storage): advisory steering signals for prefetch,
  affinity, priority, and phase-adaptive tuning. Never authoritative (I-WA1).
- **Telemetry feedback** (storage to client): caller-scoped signals about
  backpressure, locality, materialization lag, and QoS headroom (I-WA5).

The advisory runtime runs on a dedicated tokio runtime, isolated from the
data path. Advisory failures never block data-path operations (I-WA2).

## Network ports

| Port | Purpose |
|---|---|
| 9100 | Data-path gRPC (Log, Chunk, Composition, View, Discovery) |
| 9101 | Advisory gRPC (WorkflowAdvisoryService) |
| 9000 | S3 HTTP gateway |
| 2049 | NFS server |
| 9090 | Prometheus metrics + health + admin UI |

## Binaries

| Binary | Contents | Deployment |
|---|---|---|
| `kiseki-server` | Log, Chunk, Composition, View, Gateway, Audit, Advisory | Every storage node |
| `kiseki-client-fuse` | Native client with FUSE | Compute nodes |
| `kiseki-control` | Control plane | Management network (3+ instances) |
| `kiseki-keyserver` | System key manager (Raft HA) | Dedicated cluster (3-5 nodes) |
