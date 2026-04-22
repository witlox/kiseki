# Module Graph — Kiseki

**Status**: Architect phase.
**Last updated**: 2026-04-17.

Maps bounded contexts to Rust crates. Every module traces to a spec
artifact. Go removed per ADR-027 — single-language Rust.

---

## Rust workspace (core)

```
kiseki/
├── Cargo.toml                    (workspace root)
├── crates/
│   ├── kiseki-common/            ← shared types, HLC, errors
│   ├── kiseki-crypto/            ← FIPS AEAD, envelope, TenantKmsProvider (ADR-028)
│   ├── kiseki-raft/              ← Shared Raft: config, log store, transport
│   ├── kiseki-log/               ← Log context: delta, shard, Raft
│   ├── kiseki-block/             ← Raw block device I/O: DeviceBackend, bitmap allocator (ADR-029)
│   ├── kiseki-chunk/             ← Chunk Storage: placement, EC, device, GC
│   ├── kiseki-composition/       ← Composition context: namespace, refcount
│   ├── kiseki-view/              ← View Materialization: stream processors
│   ├── kiseki-gateway/           ← Protocol Gateway: NFS3, NFSv4.2, S3
│   ├── kiseki-client/            ← Native Client: FUSE, transport, cache
│   ├── kiseki-keymanager/        ← Key Management: system key manager (HA)
│   ├── kiseki-transport/         ← Transport abstraction: TCP, libfabric/CXI
│   ├── kiseki-proto/             ← Generated protobuf/gRPC
│   ├── kiseki-audit/             ← Audit log: append-only, export
│   ├── kiseki-advisory/          ← Workflow Advisory: runtime, router, budgets
│   ├── kiseki-control/           ← Control Plane: tenancy, IAM, policy (ADR-027)
│   ├── kiseki-server/            ← Storage node binary
│   └── kiseki-acceptance/        ← BDD tests (cucumber-rs)
```

## Control plane (Rust — ADR-027)

```
crates/
  └── kiseki-control/             ← Control plane: tenancy, IAM, policy,
                                    flavor, federation, namespace, retention,
                                    maintenance, advisory policy
```

Depends only on `kiseki-common` + `kiseki-proto` (crate-graph firewall).

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
│   ├── audit.proto               ← AuditEvent, AuditExportStream
│   └── advisory.proto            ← WorkflowAdvisoryService (ADR-021)
```

---

## Bounded context → module mapping

| Bounded context | Primary module | Language | Binary |
|---|---|---|---|
| Log | `kiseki-log` | Rust | kiseki-server |
| Block I/O | `kiseki-block` | Rust | kiseki-server |
| Chunk Storage | `kiseki-chunk` | Rust | kiseki-server |
| Composition | `kiseki-composition` | Rust | kiseki-server |
| View Materialization | `kiseki-view` | Rust | kiseki-server |
| Protocol Gateway (NFS) | `kiseki-gateway-nfs` | Rust | kiseki-server |
| Protocol Gateway (S3) | `kiseki-gateway-s3` | Rust | kiseki-server |
| Native Client | `kiseki-client` | Rust | kiseki-client-fuse |
| Key Management (system) | `kiseki-keymanager` | Rust | kiseki-keyserver |
| Key Management (tenant KMS integration) | `kiseki-crypto` | Rust | (library) |
| Control Plane | `kiseki-control` | Rust | kiseki-server (or standalone) |
| Audit | `kiseki-audit` | Rust | kiseki-server |
| Workflow Advisory (cross-cutting) | `kiseki-advisory` | Rust | kiseki-server |

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
- `kiseki-block` depends on `kiseki-common` (raw device I/O, bitmap allocator, redb journal)
- `kiseki-chunk` depends on `kiseki-common` + `kiseki-crypto` + `kiseki-block`
- `kiseki-composition` depends on `kiseki-common` + `kiseki-log` + `kiseki-chunk`
- `kiseki-view` depends on `kiseki-common` + `kiseki-log` + `kiseki-chunk` + `kiseki-crypto`
- `kiseki-gateway-nfs` depends on `kiseki-common` + `kiseki-view` + `kiseki-composition` + `kiseki-crypto`
- `kiseki-gateway-s3` depends on `kiseki-common` + `kiseki-view` + `kiseki-composition` + `kiseki-crypto`
- `kiseki-client` depends on `kiseki-common` + `kiseki-view` + `kiseki-composition` + `kiseki-chunk` + `kiseki-crypto` + `kiseki-transport`
- `kiseki-proto` depends on nothing (generated code)
- `kiseki-advisory` depends on `kiseki-common` + `kiseki-audit` + `kiseki-proto`. Notably: **no data-path crate depends on `kiseki-advisory`** (ADR-021 §1). Shared advisory domain types (`WorkflowRef`, `OperationAdvisory`, the hint enums, **`PoolHandle` and `PoolDescriptor`**) live in `kiseki-common` and are passed by value to data-path operations. `PoolHandle` is an opaque 16-byte tenant-scoped token — the cluster-internal `AffinityPoolId` stays in `kiseki-chunk` and is translated by `kiseki-advisory` when a hint is consumed, preserving the no-cycle rule. The advisory runtime is wired at the `kiseki-server` binary level only.

**Control-plane boundary** (ADR-027): `kiseki-control` depends only on
`kiseki-common` + `kiseki-proto`. The crate-graph firewall replaces the
former language wall. Enforced by `make arch-check`.

**No cycles.** Every dependency is downward in the graph.

---

## Binaries and deployment

| Binary | Contains | Deployment |
|---|---|---|
| `kiseki-server` | log + chunk + composition + view + gateway-nfs + gateway-s3 + audit | Every storage node |
| `kiseki-keyserver` | keymanager | Dedicated HA cluster (3-5 nodes) |
| `kiseki-client-fuse` | client + transport | Compute nodes (workload-side) |
| `kiseki-control` | control plane (Rust, ADR-027) | Management network (3+ instances) |
| `kiseki-cli` | admin CLI (Rust) | Admin workstations (future) |
| `kiseki-keyserver` | system key manager (HA) | Dedicated cluster (future) |

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
| `kiseki-advisory` | (always) | Workflow Advisory & Client Telemetry |

---

## Advisory runtime isolation (ADR-021 §1)

`kiseki-advisory` is compiled into `kiseki-server` but runs on a
**dedicated tokio runtime** separate from the data-path runtime. The
`kiseki-server` binary is responsible for:

1. Instantiating the advisory runtime at process start.
2. Binding a separate gRPC listener for `WorkflowAdvisoryService`.
3. Passing the advisory router's `lookup_handle()` to each data-path
   context so they can resolve `WorkflowRef` → `OperationAdvisory`
   on the hot path (pull-based, non-blocking).
4. Wiring `kiseki-advisory` to `kiseki-audit` for advisory-audit
   event emission (bounded queue, drop-and-record on overflow).
5. Refreshing effective policy from `control/pkg/advisory` via
   `ControlService`.

The `kiseki-client-fuse` binary does **not** host the advisory
runtime — it only consumes the `WorkflowAdvisoryService` client
surface (`DeclareWorkflow`, etc.) and attaches `workflow_ref`
headers to data-path RPCs.
