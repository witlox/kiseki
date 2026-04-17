# Build Phases — Dependency-Ordered Build Sequence

**Status**: Architect phase.
**Last updated**: 2026-04-17.

Build order follows the dependency graph. Each phase can be built and
tested independently before the next phase starts. No incremental
releases — all phases must complete before production deployment.

---

## Phase 0: Foundation (no external deps)

**Crates**: `kiseki-common`, `kiseki-proto`

- Shared types: HLC, identifiers, error types, DeltaTimestamp
- Protobuf definitions: compile and generate Rust + Go code
- Unit tests: type serialization, HLC ordering, clock sync

**Exit criteria**: all common types compile, protobuf generates cleanly,
HLC sync tests pass.

---

## Phase 1: Cryptography

**Crates**: `kiseki-crypto`
**Depends on**: Phase 0

- FIPS AEAD (AES-256-GCM via aws-lc-rs)
- Envelope encryption/decryption
- HKDF key derivation (ADR-003)
- Chunk ID derivation (sha256 + HMAC)
- Tenant KEK wrapping/unwrapping
- Compress-then-encrypt with padding (optional)
- zeroize integration for key material

**Exit criteria**: encrypt/decrypt round-trips, HKDF deterministic,
FIPS module validated, key zeroize verified.

---

## Phase 2: Transport

**Crates**: `kiseki-transport`
**Depends on**: Phase 0

- TCP transport with TLS (mTLS, Cluster CA validation)
- Transport abstraction trait
- libfabric/CXI transport (feature-flagged)
- RDMA verbs transport (feature-flagged)
- Connection pooling, keepalive

**Exit criteria**: TCP+TLS works, mTLS handshake validates tenant certs,
transport trait is pluggable.

---

## Phase 3: Log

**Crates**: `kiseki-log`
**Depends on**: Phase 0, 1

- Delta types (header + encrypted payload)
- Raft integration (openraft)
- Shard lifecycle (create, split, maintenance)
- SSTable storage (RocksDB or equivalent)
- Compaction (header-only, encrypted payloads opaque)
- Consumer watermarks and GC
- AppendDelta, ReadDeltas, ShardHealth APIs

**Exit criteria**: Raft consensus works, delta round-trip (write→read),
compaction merges correctly, shard split under load, maintenance mode.

---

## Phase 4: System Key Manager

**Crates**: `kiseki-keymanager`
**Binary**: `kiseki-keyserver`
**Depends on**: Phase 0, 1

- Raft-replicated master key storage (ADR-007)
- HKDF-based DEK derivation
- Epoch management (create, rotate, retain)
- System KEK rotation
- Health API

**Exit criteria**: key derivation is deterministic, Raft replication works,
epoch rotation retains old keys, health endpoint responds.

---

## Phase 5: Audit

**Crates**: `kiseki-audit`
**Depends on**: Phase 0, 1, 3 (uses log shard machinery)

- Per-tenant audit shards (ADR-009)
- Append-only event log
- Watermark tracking (GC integration with kiseki-log)
- Audit event types (key events, access events, system events)

**Exit criteria**: audit events append and replay, watermark tracks correctly,
per-tenant sharding works.

---

## Phase 6: Chunk Storage

**Crates**: `kiseki-chunk`
**Depends on**: Phase 0, 1, 4

- Chunk write (system encryption via key manager)
- Idempotent dedup (chunk ID check → refcount increment)
- Affinity pool management
- EC encoding/decoding
- Placement engine
- GC (refcount + retention hold check)
- Repair (EC rebuild from parity)
- Retention hold management

**Exit criteria**: chunk write→read round-trip with encryption, dedup works,
EC rebuild works, GC respects holds, retention holds block deletion.

---

## Phase 7: Composition

**Crates**: `kiseki-composition`
**Depends on**: Phase 0, 1, 3, 6

- Composition CRUD (create, update, delete)
- Namespace management
- Refcount management (increment/decrement on chunk references)
- Multipart upload (start, part upload, finalize, abort)
- Inline data (below threshold)
- Object versioning
- Cross-shard rename → EXDEV

**Exit criteria**: file create→read round-trip, multipart upload works,
versioning works, refcounts are correct, EXDEV on cross-shard rename.

---

## Phase 8: View Materialization

**Crates**: `kiseki-view`
**Depends on**: Phase 0, 1, 3, 6 (NOT Phase 7 — views read from Log and Chunk, not Composition)

- Stream processor (delta consumption, payload decryption)
- View lifecycle (create, discard, rebuild)
- View descriptor management (pull-based updates)
- MVCC read pins (acquire, release, TTL expiry)
- Staleness tracking and alerting
- Consistency model enforcement (read-your-writes vs bounded-staleness)

**Exit criteria**: stream processor materializes view from log, MVCC works,
staleness violations detected, view rebuild from scratch works.

---

## Phase 9: Protocol Gateways

**Crates**: `kiseki-gateway-nfs`, `kiseki-gateway-s3`
**Depends on**: Phase 0, 1, 7, 8

- NFS gateway: NFSv4.1 server, POSIX ops (ADR-013), lock state
- S3 gateway: S3 API subset (ADR-014), multipart, versioning
- Gateway-side encryption (plaintext from client over TLS → encrypt → write)
- Protocol-specific error mapping

**Exit criteria**: NFS read/write works, S3 PutObject/GetObject works,
multipart upload works, encryption at gateway boundary verified.

---

## Phase 10: Native Client

**Crates**: `kiseki-client`
**Binary**: `kiseki-client-fuse`
**Depends on**: Phase 0, 1, 2, 6, 7, 8

- FUSE mount (fuser crate)
- Native Rust API
- Fabric discovery (seed-based, ADR-008)
- Transport selection (CXI → verbs → TCP)
- Client-side encryption
- Access pattern detection and prefetch
- Client-side cache with invalidation

**Exit criteria**: FUSE mount works, read/write through FUSE, native API
works, transport fallback works, discovery works without control plane.

---

## Phase 11: Control Plane (Go)

**Module**: `control/`
**Binary**: `kiseki-control`, `kiseki-cli`
**Depends on**: Phase 0 (protobuf definitions)

- Tenant management (org, project, workload CRUD)
- IAM (Cluster CA, cert issuance, access requests)
- Policy (quotas, compliance tags, placement)
- Flavor management (best-fit matching)
- Federation (async config replication)
- Audit export (tenant-scoped filtering)
- Discovery service support
- **Advisory policy** (`control/pkg/advisory`): profile allow-list CRUD per scope, hint-budget CRUD with inheritance and validation (`ChildExceedsParentCeiling`, `ProfileNotInParent`), opt-out state machine (enabled/draining/disabled) with Raft-backed persistence, effective-policy computation endpoint. Federation replicates policy but NOT workflow state. (ADR-021 §6)

**Exit criteria**: tenant CRUD works, quota enforcement works, compliance
tags inherit correctly, federation peer registration works, advisory
policy inheritance computes correctly and rejects parent-exceeding
updates.

---

## Phase 11.5: Workflow Advisory runtime (Rust)

**Crates**: `kiseki-advisory`
**Depends on**: Phase 0 (common + proto), Phase 5 (audit), Phase 11 (control plane for policy fetching)

- `WorkflowAdvisoryService` gRPC server (separate listener, isolated tokio runtime per ADR-021 §1)
- Workflow table / effective-hints table / prefetch ring (ADR-021 §4)
- Budget enforcer (token buckets per workload for hints/sec and declare/sec)
- Audit emitter → `kiseki-audit` (bounded queue, drop-and-audit on overflow, batched hint accept/throttle per I-WA8)
- k-anonymity bucketing for aggregate telemetry (ADR-021 §7)
- Covert-channel hardening helper: bucketed response timing + size padding (ADR-021 §8, I-WA15)
- Phase-history ring + `PhaseSummary` rollup (ADR-021 §9)
- `AdvisoryLookup` hot-path read surface (arc-swap snapshot, ≤500 µs deadline)
- No data-path code in this crate; no `kiseki-advisory` dependency in data-path crates (I-WA2)

**Exit criteria**:
- Property test: data-path outcome equivalence with/without advisory
  annotations (I-WA1, sampled across chunk/view/composition paths).
- Property test: `AdvisoryLookup::lookup()` returns within deadline
  under synthetic advisory-runtime overload, always safely returning
  `None` past deadline (I-WA2).
- Property test: `ScopeNotFound` response timing/size distributions
  are statistically indistinguishable between unauthorized and absent
  targets (I-WA6, I-WA15).
- Unit tests: budget enforcement, hint payload size bounds, declare
  rate, phase monotonicity, opt-out state transitions.
- Integration test with `kiseki-audit`: event batching guarantees
  (I-WA8).

---

## Phase 12: Integration (kiseki-server binary)

**Binary**: `kiseki-server`
**Depends on**: All Rust phases (3-10), Phase 11.5 (advisory)

- Compose all Rust crates into single server binary
- Process management for per-tenant stream processors (ADR-012)
- Discovery responder
- Node health reporting (clock quality, device health)
- Maintenance mode
- **Advisory runtime wiring**: instantiate second tokio runtime, bind separate gRPC listener for `WorkflowAdvisoryService`, pass `AdvisoryLookup` handle to each data-path context's constructor, fetch and refresh effective policy from `ControlService`, start advisory-audit emitter (ADR-021 §1)

**Exit criteria**: end-to-end write→read through server binary,
multi-tenant isolation verified, key rotation mid-traffic works,
advisory runtime overload does not block data path (F-ADV-1 simulated
and data-path SLOs maintained).

---

## Phase dependencies (visual)

```
Phase 0 (common, proto)
  ├── Phase 1 (crypto)
  │     ├── Phase 3 (log)
  │     │     ├── Phase 5 (audit)
  │     │     ├── Phase 7 (composition) ←── Phase 6
  │     │     │     ├── Phase 8 (view)
  │     │     │     │     ├── Phase 9 (gateways)
  │     │     │     │     └── Phase 10 (client) ←── Phase 2
  │     │     │     │
  │     ├── Phase 4 (key manager)
  │     │     └── Phase 6 (chunk)
  │     │
  ├── Phase 2 (transport)
  │     └── Phase 10 (client)
  │
  ├── Phase 11 (control plane, Go — can build in parallel with Rust phases)
  │
  └── Phase 12 (integration — final)
```

**Parallelism opportunities**:
- Phase 11 (Go control plane) can be built in parallel with Phases 3-10
- Phase 2 (transport) can be built in parallel with Phases 1, 3, 4
- Phase 5 (audit) can start as soon as Phase 3 is done
- Phase 11.5 (advisory) can start as soon as Phase 5 (audit) and the Phase 11 advisory-policy endpoint are done; it is independent of Phases 6-10 because it does not link against any data-path crate
