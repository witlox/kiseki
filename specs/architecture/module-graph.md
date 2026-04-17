# Module Graph — Kiseki

**Status**: Architect phase.
**Last updated**: 2026-04-17.

Maps bounded contexts to Rust crates and Go packages. Every module
traces to a spec artifact.

---

## Rust workspace (core)

```
kiseki/
├── Cargo.toml                    (workspace root)
├── crates/
│   ├── kiseki-common/            ← shared types, HLC, errors
│   ├── kiseki-crypto/            ← FIPS AEAD, envelope, key wrapping
│   ├── kiseki-log/               ← Log context: delta, shard, Raft
│   ├── kiseki-chunk/             ← Chunk Storage context: placement, EC, GC
│   ├── kiseki-composition/       ← Composition context: namespace, refcount
│   ├── kiseki-view/              ← View Materialization: stream processors
│   ├── kiseki-gateway-nfs/       ← Protocol Gateway: NFSv4.1
│   ├── kiseki-gateway-s3/        ← Protocol Gateway: S3
│   ├── kiseki-client/            ← Native Client: FUSE, transport, cache
│   ├── kiseki-keymanager/        ← Key Management: system key manager (HA)
│   ├── kiseki-transport/         ← Transport abstraction: TCP, libfabric/CXI
│   ├── kiseki-proto/             ← Generated protobuf/gRPC (Rust side)
│   └── kiseki-audit/             ← Audit log: append-only, export
└── bin/
    ├── kiseki-server/            ← Storage node daemon (composes log+chunk+view+gateway)
    ├── kiseki-keyserver/         ← System key manager daemon
    └── kiseki-client-fuse/       ← FUSE mount binary
```

## Go modules (control plane)

```
control/
├── go.mod
├── cmd/
│   ├── kiseki-control/           ← Control plane API server
│   └── kiseki-cli/               ← Admin CLI
├── pkg/
│   ├── tenant/                   ← Tenancy: org, project, workload
│   ├── iam/                      ← IAM: mTLS CA, access requests
│   ├── policy/                   ← Placement, quota, compliance tags
│   ├── flavor/                   ← Flavor management, best-fit matching
│   ├── federation/               ← Cross-site: config sync, data replication
│   ├── audit/                    ← Audit export: tenant-scoped filtering
│   └── discovery/                ← Fabric-level discovery service
└── proto/                        ← Generated protobuf/gRPC (Go side)
```

## Shared

```
proto/
├── kiseki/v1/
│   ├── common.proto              ← HLC, WallTime, TenantId, ChunkId, etc.
│   ├── log.proto                 ← Delta, DeltaEnvelope, ShardInfo
│   ├── chunk.proto               ← ChunkWriteRequest, ChunkReadResponse
│   ├── key.proto                 ← KeyWrapRequest, KeyRotateRequest
│   ├── composition.proto         ← CompositionMutation, NamespaceOps
│   ├── view.proto                ← ViewDescriptor, ViewStatus
│   ├── control.proto             ← TenantOps, PolicyOps, FederationOps
│   └── audit.proto               ← AuditEvent, AuditExportStream
```

---

## Bounded context → module mapping

| Bounded context | Primary module | Language | Binary |
|---|---|---|---|
| Log | `kiseki-log` | Rust | kiseki-server |
| Chunk Storage | `kiseki-chunk` | Rust | kiseki-server |
| Composition | `kiseki-composition` | Rust | kiseki-server |
| View Materialization | `kiseki-view` | Rust | kiseki-server |
| Protocol Gateway (NFS) | `kiseki-gateway-nfs` | Rust | kiseki-server |
| Protocol Gateway (S3) | `kiseki-gateway-s3` | Rust | kiseki-server |
| Native Client | `kiseki-client` | Rust | kiseki-client-fuse |
| Key Management (system) | `kiseki-keymanager` | Rust | kiseki-keyserver |
| Key Management (tenant KMS integration) | `kiseki-crypto` | Rust | (library) |
| Control Plane | `control/` | Go | kiseki-control |
| Audit | `kiseki-audit` + `control/pkg/audit` | Rust + Go | both |

---

## Dependency graph

```
                    kiseki-common
                    /     |     \
                   /      |      \
            kiseki-crypto  |   kiseki-transport
              /    |    \  |      |
             /     |     \ |      |
     kiseki-log    |   kiseki-keymanager
        |    \     |
        |     \    |
 kiseki-composition kiseki-audit
        |
        |
   kiseki-chunk ← kiseki-crypto
        |
        |
   kiseki-view
      /    \
     /      \
kiseki-gateway-nfs  kiseki-gateway-s3
     \      /
      \    /
   kiseki-client ← kiseki-transport
```

**Dependency rules (acyclic)**:
- `kiseki-common` depends on nothing (types only)
- `kiseki-crypto` depends on `kiseki-common` + aws-lc-rs
- `kiseki-transport` depends on `kiseki-common` + tokio + libfabric-sys (optional)
- `kiseki-log` depends on `kiseki-common` + `kiseki-crypto` + openraft
- `kiseki-audit` depends on `kiseki-common` + `kiseki-crypto`
- `kiseki-keymanager` depends on `kiseki-common` + `kiseki-crypto` + openraft
- `kiseki-chunk` depends on `kiseki-common` + `kiseki-crypto`
- `kiseki-composition` depends on `kiseki-common` + `kiseki-log` + `kiseki-chunk`
- `kiseki-view` depends on `kiseki-common` + `kiseki-log` + `kiseki-chunk` + `kiseki-crypto`
- `kiseki-gateway-nfs` depends on `kiseki-common` + `kiseki-view` + `kiseki-composition` + `kiseki-crypto`
- `kiseki-gateway-s3` depends on `kiseki-common` + `kiseki-view` + `kiseki-composition` + `kiseki-crypto`
- `kiseki-client` depends on `kiseki-common` + `kiseki-view` + `kiseki-composition` + `kiseki-chunk` + `kiseki-crypto` + `kiseki-transport`
- `kiseki-proto` depends on nothing (generated code)

**Cross-language boundary**: `kiseki-proto` (Rust) ↔ `control/proto/` (Go) via gRPC. No direct Rust↔Go FFI for control plane.

**No cycles.** Every dependency is downward in the graph.

---

## Binaries and deployment

| Binary | Contains | Deployment |
|---|---|---|
| `kiseki-server` | log + chunk + composition + view + gateway-nfs + gateway-s3 + audit | Every storage node |
| `kiseki-keyserver` | keymanager | Dedicated HA cluster (3-5 nodes) |
| `kiseki-client-fuse` | client + transport | Compute nodes (workload-side) |
| `kiseki-control` | control plane (Go) | Management network (3+ instances) |
| `kiseki-cli` | admin CLI (Go) | Admin workstations |

---

## Crate feature flags

| Crate | Feature | Purpose |
|---|---|---|
| `kiseki-transport` | `cxi` | Enable libfabric/CXI Slingshot support |
| `kiseki-transport` | `verbs` | Enable RDMA verbs support |
| `kiseki-crypto` | `fips` | FIPS 140-2/3 validated backend (aws-lc-rs) |
| `kiseki-chunk` | `compression` | Enable tenant opt-in compression |
| `kiseki-gateway-nfs` | (always) | NFSv4.1 |
| `kiseki-gateway-s3` | (always) | S3 API subset |
