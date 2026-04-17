# Kiseki — Distributed Storage for HPC/AI

Distributed storage system for HPC / AI workloads on Slingshot and
commodity fabrics. Production-grade, multi-tenant, encryption-native.

**Project name**: Kiseki (軌跡 — Japanese: locus, trajectory, trace)

---

## Status

**Phase**: Architect (analyst phase complete)

The analyst phase produced a complete spec tree through structured
interrogation:

- 8 bounded contexts: Log, Chunk Storage, Composition, View
  Materialization, Protocol Gateway, Native Client, Key Management,
  Control Plane
- 51 invariants across 12 categories
- 132 Gherkin scenarios across all contexts
- 20 failure modes (P0-P3) with blast radius and mitigation
- 50+ tracked assumptions
- 17 adversarial findings (3 closed, 14 escalated to architect)

The architect phase will produce module boundaries, data structures,
API contracts, enforcement maps, build phase ordering, and ADRs.

---

## Architecture overview

- **Core language**: Rust (log, chunks, views, native client, hot paths)
- **Control plane**: Go (declarative API, operators, CLI)
- **Boundary**: gRPC / protobuf
- **Client bindings**: Rust native + C FFI, Python (PyO3), C++ wrapper

### Key architectural commitments

- **Log-as-truth**: ordered, replicated log of deltas per shard (Raft)
- **Two-layer encryption**: system DEK encrypts data, tenant KEK wraps
  access. FIPS 140-2/3 validated. No plaintext at rest or in flight.
- **Multi-tenancy**: org → [project] → workload hierarchy with
  zero-trust boundary between cluster admin and tenant admin
- **Multi-protocol**: NFS + S3 via gateways, native Rust client + FUSE
- **Federated multi-site**: async replication, per-site consistency
- **Content-addressed chunks**: cross-tenant dedup by default, tenant
  opt-out via HMAC for full isolation
- **Dual clock model**: HLC for ordering/causality, wall clock for
  retention/staleness/audit (adapted from taba)

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
│   └── design-conversation.md    # Distilled 16-turn design conversation
└── prior-art/
    └── deltafs-mochi-evaluation.md    # DeltaFS + Mochi comparison

specs/
├── SEED.md                       # Original analyst seed
├── ubiquitous-language.md        # 45+ confirmed domain terms
├── domain-model.md               # 8 bounded contexts
├── invariants.md                 # 51 invariants
├── assumptions.md                # 50+ tracked assumptions
├── failure-modes.md              # 20 failure modes (P0-P3)
├── adversarial-findings.md       # 17 adversarial findings
├── cross-context/
│   └── interactions.md           # Data paths, contracts, cascades
└── features/
    ├── log.feature               # 16 scenarios
    ├── chunk-storage.feature     # 18 scenarios
    ├── key-management.feature    # 18 scenarios
    ├── composition.feature       # 17 scenarios
    ├── view-materialization.feature  # 16 scenarios
    ├── protocol-gateway.feature  # 14 scenarios
    ├── native-client.feature     # 15 scenarios
    └── control-plane.feature     # 18 scenarios
```

---

## Prior art

- **DeltaFS** (CMU PDL / LANL): log-structured metadata, serverless,
  per-job. Kiseki differs in: persistence, multi-tenancy, standard
  protocols, first-class encryption.
- **Mochi** (Argonne / LANL / CMU): composable HPC data services.
  Kiseki borrows patterns but builds in pure Rust (no Mochi dependency).
- **DAOS, Ceph, Lustre, VAST**: evaluated as comparison points.

See `docs/prior-art/deltafs-mochi-evaluation.md` for detailed analysis.
