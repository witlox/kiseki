# Kiseki — Distributed Storage for HPC/AI

Distributed storage system for HPC / AI workloads on Slingshot and
commodity fabrics. Production-grade, multi-tenant, encryption-native.

**Project name**: Kiseki (軌跡 — Japanese: locus, trajectory, trace)

---

## Status

**Phase**: **Phase 0 in progress** — workspace scaffold, shared types,
protobuf. See `specs/architecture/build-phases.md` for the full 13-phase
plan.

| Stage | State |
|---|---|
| Design (analyst + architect + adversary + backpass) | Complete |
| Phase 0 — `kiseki-common`, `kiseki-proto`, CI scaffold | In progress |
| Phases 1 – 12 | Pending |

### Workspace

```
crates/
├── kiseki-common/      # shared types: HLC, identifiers, advisory surface, errors
└── kiseki-proto/       # generated prost/tonic from specs/architecture/proto/
control/                # Go control plane scaffold (Phase 11)
```

### Pre-commit

```
make           # fmt + clippy + test + go vet + go test (local)
make verify    # strict CI-equivalent (adds cargo-deny + golangci-lint)
```

---

## Architecture overview

```
Compute nodes                    Storage nodes                     Management
┌─────────────────┐   ┌──────────────────────────────────┐   ┌──────────────┐
│ kiseki-client   │   │ kiseki-server                    │   │kiseki-control│
│  FUSE + native  │   │  Log (Raft per shard)            │   │  Tenancy     │
│  Client encrypt │   │  Chunk Storage (EC, placement)   │   │  IAM         │
│  Transport sel. │──▶│  Composition (namespace, refcount)│   │  Policy      │
│  Pattern detect │   │  View Materialization (streams)  │   │  Federation  │
│  Cache          │   │  Gateway NFS + S3                │   │  Audit export│
└─────────────────┘   │  Audit                           │   └──────────────┘
                      └──────────┬───────────────────────┘
                                 │
                      ┌──────────▼───────────────────────┐
                      │ kiseki-keyserver (HA, Raft)      │
                      │  System master keys              │
                      │  Epoch management                │
                      └──────────────────────────────────┘
```

### Languages and boundaries

- **Rust** (core): log, chunks, views, gateways, native client, key manager, crypto
- **Go** (control plane): tenancy, IAM, policy, federation, audit export, CLI
- **gRPC/protobuf**: Rust ↔ Go boundary
- **FIPS 140-2/3**: aws-lc-rs (AES-256-GCM, HKDF-SHA256)

### Key architectural decisions

- **Log-as-truth**: ordered, replicated log of deltas per shard (Raft)
- **Two-layer encryption (model C)**: system DEK encrypts data (HKDF-derived,
  never stored individually); tenant KEK wraps access. No plaintext at rest
  or in flight.
- **Multi-tenancy**: org → [project] → workload hierarchy. Zero-trust
  boundary: cluster admin cannot access tenant data without approval.
- **Multi-protocol**: NFS (ADR-013: POSIX subset) + S3 (ADR-014: HPC/AI
  subset) via gateways, native Rust client + FUSE
- **Content-addressed chunks**: cross-tenant dedup by default (sha256);
  tenant opt-out via HMAC for full isolation
- **Federated multi-site**: async replication, per-site consistency, shared
  tenant KMS across sites
- **Dual clock model**: HLC for ordering/causality, wall clock for
  retention/staleness/audit (adapted from taba)
- **Client-side resilience**: multi-endpoint resolution (DNS, seed list),
  automatic failover on node failure. No master node.

### Target hardware

HPE Cray ClusterStor E1000/E1000F — all-NVMe, Slingshot-attached.
Architecture is hardware-neutral; ClusterStor is the initial substrate.

### Regulatory compliance

HIPAA, GDPR, revFADP (Swiss Federal Act on Data Protection). Shapes
encryption, audit, retention, and data residency decisions throughout.

---

## Repository structure

```
docs/
├── analysis/
│   └── design-conversation.md        # Distilled 16-turn design conversation
└── prior-art/
    └── deltafs-mochi-evaluation.md   # DeltaFS + Mochi comparison

specs/
├── SEED.md                           # Original analyst seed
├── ubiquitous-language.md            # 45+ confirmed domain terms
├── domain-model.md                   # 8 bounded contexts
├── invariants.md                     # 56 invariants
├── assumptions.md                    # 50+ tracked assumptions
├── failure-modes.md                  # 20 failure modes (P0-P3)
├── adversarial-findings.md           # 17 analyst adversarial findings
├── cross-context/
│   └── interactions.md               # Data paths, contracts, cascades
├── features/
│   ├── log.feature                   # 18 scenarios
│   ├── chunk-storage.feature         # 18 scenarios
│   ├── key-management.feature        # 17 scenarios
│   ├── composition.feature           # 16 scenarios
│   ├── view-materialization.feature  # 16 scenarios
│   ├── protocol-gateway.feature      # 16 scenarios
│   ├── native-client.feature         # 20 scenarios
│   ├── control-plane.feature         # 23 scenarios
│   ├── authentication.feature        # 13 scenarios (mTLS, IdP, SPIFFE)
│   └── operational.feature           # 28 scenarios (integrity, versioning, compression)
├── findings/
│   └── architecture-review.md        # 8 adversary architecture findings
└── architecture/
    ├── module-graph.md               # 12 Rust crates + Go module
    ├── api-contracts.md              # Per-context commands/events/queries
    ├── enforcement-map.md            # 56 invariants → enforcement points
    ├── build-phases.md               # 13 dependency-ordered phases
    ├── error-taxonomy.md             # Typed errors per context
    ├── data-models/
    │   ├── common.rs                 # HLC, identifiers, base errors
    │   ├── crypto.rs                 # AEAD, envelope, CryptoOps trait
    │   ├── log.rs                    # Delta, Shard, LogOps trait
    │   ├── chunk.rs                  # Chunk, AffinityPool, ChunkOps trait
    │   ├── composition.rs            # Composition, Namespace
    │   ├── view.rs                   # ViewDescriptor, StreamProcessor
    │   ├── key.rs                    # KeyManager, TenantKMS
    │   └── tenant.rs                 # Organization, Flavor, Federation
    ├── proto/kiseki/v1/
    │   ├── common.proto              # Shared types
    │   ├── control.proto             # ControlService (Go)
    │   ├── key.proto                 # KeyManagerService (Rust)
    │   └── audit.proto               # AuditExportService (Go)
    └── adr/
        ├── 001-pure-rust-no-mochi.md
        ├── 002-two-layer-encryption-model-c.md
        ├── 003-system-dek-derivation.md
        ├── 004-schema-versioning-and-upgrade.md
        ├── 005-ec-and-chunk-durability.md
        ├── 006-inline-data-threshold.md
        ├── 007-system-key-manager-ha.md
        ├── 008-native-client-discovery.md
        ├── 009-audit-log-sharding.md
        ├── 010-retention-hold-enforcement.md
        ├── 011-crypto-shred-cache-ttl.md
        ├── 012-stream-processor-isolation.md
        ├── 013-posix-semantics-scope.md
        ├── 014-s3-api-scope.md
        ├── 015-observability.md
        ├── 016-backup-and-dr.md
        ├── 017-dedup-refcount-access-control.md
        ├── 018-runtime-integrity-monitor.md
        └── 019-gateway-deployment-model.md
```

---

## Build phases (summary)

| Phase | Module | Depends on | Parallelizable with |
|---|---|---|---|
| 0 | Common types + protobuf | — | — |
| 1 | Crypto (FIPS AEAD) | 0 | 2 |
| 2 | Transport (TCP/TLS, CXI) | 0 | 1 |
| 3 | Log (Raft, delta, shard) | 0, 1 | — |
| 4 | System key manager | 0, 1 | 3, 5 |
| 5 | Audit | 0, 1, 3 | 4 |
| 6 | Chunk storage | 0, 1, 4 | 5, 7 |
| 7 | Composition | 0, 1, 3, 6 | 8 |
| 8 | View materialization | 0, 1, 3, 6 | 7, 9 |
| 9 | Protocol gateways (NFS, S3) | 0, 1, 7, 8 | 10 |
| 10 | Native client (FUSE) | 0, 1, 2, 6, 7, 8 | 9 |
| 11 | Control plane (Go) | 0 | 1-10 (fully parallel) |
| 12 | Integration | All | — |

See `specs/architecture/build-phases.md` for full details.

---

## Prior art

- **DeltaFS** (CMU PDL / LANL): log-structured metadata, serverless,
  per-job. Kiseki differs in: persistence, multi-tenancy, standard
  protocols, first-class encryption.
- **Mochi** (Argonne / LANL / CMU): composable HPC data services.
  Kiseki borrows patterns but builds in pure Rust (ADR-001).
- **DAOS, Ceph, Lustre, VAST**: evaluated as comparison points.

See `docs/prior-art/deltafs-mochi-evaluation.md` for detailed analysis.

---

## License

TBD
