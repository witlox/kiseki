# Build Phases — Dependency-Ordered Build Sequence

**Status**: Architect phase.
**Last updated**: 2026-04-25. Added Phase 13 (cluster topology — ADR-033/034/035).

Build order follows the dependency graph. Each phase can be built and
tested independently before the next phase starts. No incremental
releases — all phases must complete before production deployment.

---

## Phase 0: Foundation (no external deps)

**Crates**: `kiseki-common`, `kiseki-proto`

- Shared types: HLC, identifiers, error types, DeltaTimestamp
- Protobuf definitions: compile and generate Rust code (Go removed per ADR-027)
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

## Phase 6.5: Block I/O (ADR-029)

**Crates**: `kiseki-block`
**Depends on**: Phase 0

- `DeviceBackend` trait with two implementations: `RawDevice`, `FileBacked`
- Auto-detection of device characteristics (physical block size, optimal I/O size)
- Bitmap allocator with redb journal (allocation bitmap updates journaled before application)
- Superblock per device (magic, version, device UUID, bitmap offset, capacity)
- Block-aligned I/O (all writes aligned to physical block size)
- File-backed fallback for VMs and CI (same trait, backed by a regular file)
- Periodic scrub: bitmap vs redb `device_alloc` consistency check

**Exit criteria**: raw device write→read round-trip with alignment, bitmap
allocate→free cycle, journal crash recovery (simulate crash between journal
write and bitmap apply), file-backed fallback passes same test suite,
auto-detection returns correct block size for NVMe and file backends.

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

## Phase 11: Control Plane (Rust — ADR-027)

**Crate**: `kiseki-control`
**Binary**: `kiseki-control` (standalone or wired into `kiseki-server`)
**Depends on**: Phase 0 (common + proto only; crate-graph firewall)

- Tenant management (org, project, workload CRUD)
- IAM (access requests, zero-trust boundary)
- Policy (quotas, compliance tags, placement)
- Flavor management (best-fit matching)
- Federation (async config replication, peer registry)
- Namespace management (shard assignment, maintenance mode)
- Retention holds (GC blocking)
- **Advisory policy**: profile allow-list CRUD per scope, hint-budget
  inheritance with validation, opt-out state machine (enabled/draining/
  disabled). Federation replicates policy but NOT workflow state (ADR-021 §6).

**Status**: 32/32 BDD scenarios GREEN (cucumber-rs). Go code removed.

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

## Phase 13: Cluster Topology (ADR-033, ADR-034, ADR-035)

**Crates**: `kiseki-control`, `kiseki-log`, `kiseki-gateway`, `kiseki-client`
**Depends on**: Phase 3 (log), Phase 11 (control plane), Phase 12 (integration)

Three sub-phases, in dependency order:

### Phase 13a: Persistent namespace shard map + initial topology (ADR-033)

- Replace `NamespaceStore` in-memory HashMap with Raft-backed `NamespaceShardMapStore`
- `CreateNamespace` computes `initial_shards`, creates N Raft groups with uniform key ranges
- I-L12 placement: fewest-leaders-for-namespace with NodeId tie-break
- `GetNamespaceShardMap` RPC for gateway/client routing cache
- Gateway routing: replace hardcoded `ShardId::from_u128(1)` with `route_to_shard()`
- Wire `ShardEndpoint` in discovery response
- I-L11 ratio-floor evaluator (background, topology-change triggered)

**Exit criteria**: namespace creation produces N shards distributed across
nodes; writes route to correct shard by hashed_key; shard map survives
process restart; ratio-floor auto-split fires when nodes are added.

### Phase 13b: Shard merge (ADR-034)

- Merge candidate scanner (periodic, 5-min interval)
- Copy-then-cutover merge protocol
- `ShardMerged` event handling in gateway, view stream processor
- `Merging`/`Retiring` shard states
- F-O6 merge/split race exclusion

**Exit criteria**: adjacent under-utilized shards merge after 24h;
merge does not block writes (< 50ms cutover pause); merged shard
serves reads correctly; split during merge is rejected.

### Phase 13c: Node lifecycle + drain (ADR-035)

- `NodeRecord` with state machine in control plane Raft
- `DrainNode` / `CancelDrain` RPCs
- Drain orchestrator: leadership transfer → voter replacement → eviction
- Pre-check: refuse drain if RF would be violated
- `DrainProgress` persistence for crash recovery
- Drain audit events
- CLI: `kiseki-admin node drain|drain-cancel|list|status`

**Exit criteria**: drain moves all leaders and voters off a node without
dropping below RF=3 at any step; drain refused on 3-node cluster without
replacement; cancelled drain returns node to Active with no rollback of
completed replacements; drain survives orchestrator restart.

---

## Phase 15: pNFS layout + Data Server subprotocol (ADR-038)

Required before perf-cluster pNFS benchmarks produce meaningful
numbers. Replaces the stub `LayoutManager` and the malformed
`op_layoutget` body in `kiseki-gateway/src/{pnfs.rs,nfs4_server.rs}`
with a RFC 8435 (Flexible Files) layout and a co-located stateless
DS listener.

### Phase 15a: DS surface

- New `ds_addr` listener in `kiseki-server` config (default `:2052`)
- `pnfs_ds_server.rs` in `kiseki-gateway`: stateless DS dispatcher
  (op subset per I-PN7) reusing the XDR codec from `nfs_xdr.rs`
- `PnfsFileHandle` MAC validate/expiry check (I-PN1)
- DS handler delegates to `GatewayOps::read`/`write` (I-PN3)
- mTLS termination via existing `TlsConfig::server_config`

**Exit criteria**: DS listener answers EXCHANGE_ID + READ/WRITE for
a hand-crafted fh4 with a valid MAC; rejects expired/forged fh4 with
NFS4ERR_BADHANDLE. DS task can be killed and restarted with no
client-visible state loss (I-PN2).

### Phase 15b: MDS layout wire-up

- Replace `crates/kiseki-gateway/src/pnfs.rs::LayoutManager` with
  a `LayoutManagerOps` impl producing `ServerLayout` (RFC 8435 ff_layout4)
- Replace `op_layoutget` body with `ff_layout4` XDR encoder
- Add `op_getdeviceinfo` (op 47) handler with `ff_device_addr4`
- fh4 MAC key derivation via existing `kiseki-crypto` HKDF
- Layout cache keyed by `(composition_id, byte_range)` with TTL
  (I-PN4); membership filter against `GetNamespaceShardMap` (I-PN6)

**Exit criteria**: a Linux 5.4+ pNFS client mounts the export and
`/proc/self/mountstats` shows non-zero `LAYOUTGET`, `GETDEVICEINFO`,
and per-DS `READ` counters after a 1-MiB read.

### Phase 15a (revised): DS surface — exit gate

In addition to the previously-listed items, Phase 15a now requires:

- NFS-over-TLS termination on **both** MDS (`nfs_addr`) and DS
  (`ds_addr`) listeners, using the same `TlsConfig::server_config`
  already in `kiseki-server`.
- Plaintext fallback gated by `[security].allow_plaintext_nfs` AND
  `KISEKI_INSECURE_NFS=true`, with mandatory startup banner +
  `SecurityDowngradeEnabled` audit event + auto-halved TTL +
  multi-tenant refusal (I-PN7).
- fh4 wire encoding per ADR-038 §D4.3 (60-byte payload + 16-byte
  MAC = 76 bytes); MAC input domain-separated with
  `b"kiseki/pnfs-fh/v1\x00"` prefix.

**Revised exit criteria**: a Linux 6.7+ pNFS client with
`xprtsec=mtls` mounts the export and reads 1 MB through one DS;
*and* the same flow with `allow_plaintext_nfs=true` works on a
single-tenant kernel-5.x client (Rocky 9.5 baseline). Both flows
verified via `/proc/self/mountstats`.

### Phase 15d: TopologyEventBus (ADR-038 §D10)

Required before 15c can run reliably (resolves ADV-038-3, -8).

- New `TopologyEventBus` in `kiseki-control` with
  `tokio::sync::broadcast::Sender<TopologyEvent>` (capacity 1024).
- Wire producers:
  - `DrainOrchestrator` — emit `NodeDraining` / `NodeRestored` after
    control-Raft commit of the state transition (ADR-035 §3 §5)
  - Namespace shard-map mutator — emit `ShardSplit` / `ShardMerged`
    after the `NamespaceShardMap` Raft commit (ADR-033, ADR-034)
  - `CompositionStore::delete` — emit `CompositionDeleted` after
    the delete delta is applied (composition crate)
  - Key rotation handler — emit `KeyRotation` after the new fh4 MAC
    key is in service (ADR-005 / `kiseki-keymanager`)
- Add `pnfs_topology_event_lag_total` Prometheus counter.
- Unit test: each producer fires exactly one event per commit (not
  per attempt; aborted Raft commits MUST NOT fire).

**Exit criteria**: integration test starts a fake subscriber, drains
a node via the production code path, observes a `NodeDraining` event
on the bus within 100 ms of the drain audit event being recorded.
Test repeats for split, merge, composition delete, key rotation.

### Phase 15c: LAYOUTRECALL + integration

(Now blocks on 15d, not 15a.)

- `kiseki-gateway::LayoutManager` subscribes to `TopologyEventBus`
  at startup; per `RecallReason`, walks live layouts and fires
  recalls.
- Layout cache eviction (I-PN8) — sweeper task + capacity LRU.
- BDD: `specs/features/pnfs-rfc8435.feature` (multi-DS, drain-recall,
  split-recall, merge-recall, deletion-recall, key-rotation-recall,
  TTL-fallback).
- e2e: `tests/e2e/test_pnfs.py` (Linux 6.7+ client with
  `xprtsec=mtls`, multi-DS read, drain-during-IO).
- Perf benchmark: `infra/gcp/benchmarks/perf-suite.sh` pNFS section
  asserts ≥ 1.5× single-MDS throughput at 3 storage nodes.

**Exit criteria**: I-PN5 SLA met (≤ 1 sec from event commit to
recall send-out under non-lagging subscriber). Perf gate ≥ 1.5×
passes on the GCP cluster. Subscriber-lag-induced cache flush
verified by injection test.

### Phase 15c sub-phase close-out (15c.5 → 15c.10)

After 15c.4 wired `MdsLayoutManager` into `NfsContext`, an
end-to-end Linux pNFS client mount surfaced a chain of latent
bugs across the LAYOUTGET → GETDEVICEINFO → DS-direct READ path.
Fixed in eight sub-phases between 2026-04-28 and 2026-04-29:

- **15c.5 step 1** (`1de407a`) — LAYOUTGET stripe-count cap
  (`max_stripes_per_layout`, default 64). Without it, the kernel's
  `loga_length = u64::MAX` "rest of file" sentinel produced
  `u64::MAX / stripe_size ≈ 281e12` stripes and OOM-killed the
  server.
- **15c.5 step 2** (`16aa0a8`) — `FATTR4_FS_LAYOUT_TYPES`
  (bit 62) added to `SUPPORTED_ATTRS`. Linux clients gate
  LAYOUTGET on this bit; without it the kernel never engaged
  pNFS dispatch and silently degraded to plain NFSv4.1 reads.
- **Phase 16 fabric routing** (`605134e`) — fabric peers now
  connect to `cfg.data_addr.port()` (where `ClusterChunkService`
  binds, default 9100) instead of `cfg.raft_peers` port (9300).
  Pre-fix, every chunk fan-out hit the wrong gRPC server and
  returned `quorum lost: only 1/2 replicas acked`.
- **15c.5 step 3** (`d95b3f3`) — three pNFS dispatch fixes
  bundled: (a) `op_getdeviceinfo` field order swapped to RFC 8435
  §5.1.1 `ffda_netaddrs` first then `ffda_versions`; (b)
  universal-address `host_port_to_resolved_uaddr` does DNS lookup
  so the kernel's RPC parser sees an IP literal; (c) DS server
  advertises `EXCHGID4_FLAG_USE_PNFS_DS` in `EXCHANGE_ID`. Wire-
  level pNFS DS-direct reads now work end-to-end.
- **15c.6** (`972d40f`) — FUSE+remote-http build fix (sandbox
  path used a stale `Box<ChunkStore>` after Phase 16's
  `Arc<dyn AsyncChunkOps>` change). `test_fuse_remote_http_cross_protocol_roundtrip`
  now passes.
- **15c.7** (`758aae5`) — `KisekiFuse::unlink` bridges to
  `gateway.delete()` so the cluster-side composition is removed,
  not just the local inode entry.
- **15c.8** (`472a846`, `1915203`) — fabric size cap raised
  from tonic's 4 MiB default → 64 MiB → 256 MiB to fit the
  envelope around large S3 PUTs (model checkpoints, dataset
  shards). E2E witness:
  `test_s3_large_put_exceeds_64mib_fabric_cap` exercises a
  128 MiB PUT+GET round-trip.
- **15c.9** (`379a524`) — flex-files layout encoding revised:
  one segment + N mirrors instead of N segments × 1 mirror. See
  ADR-039 for the architectural decision; tcpdump witness in
  `specs/findings/phase-15c8-nfs41-perf-investigation.md`.
- **15c.10** (`471a3a5`) — perf-investigation findings (the
  reported 0.3 MB/s NFSv4.1 throughput is a fio `--time_based`
  + page-cache artifact; wire-level throughput is ~400 MB/s once
  the DS session is established). Three optimization attempts
  (`return_on_close=false`, single-mirror pinning,
  `max_stripes_per_layout=1`) all broke pNFS dispatch and were
  reverted.

**Sub-phase exit witness**: full e2e suite goes 31 passed, 0
failed, 3 expected skips against the docker-compose 3-node
cluster. The pNFS dispatch breakthrough — `pnfs=LAYOUT_FLEX_FILES`
in `/proc/self/mountstats` caps + DS-direct READs visible on
port 2052 — is the witness that ADR-038 + ADR-039 are
implemented end-to-end.

---

## Phase 16: Cross-node chunk replication (ADR-005, ADR-026)

**Goal**: A 3-node Replication-3 cluster genuinely tolerates
single-node loss. Pre-Phase 16, each node held its own chunk
store and a PUT on node-1 left node-2 + node-3 with no copy —
breaking the basic HA promise (the B-3 gap from the
single-node→cluster transition).

**Sub-phases**:

- **16a — Replication-N over the cluster fabric.** Ships a
  per-shard Raft `cluster_chunk_state` table (refcount + placement,
  keyed by `(tenant_id, chunk_id)`), the new `kiseki-chunk-cluster`
  crate with `ClusteredChunkStore` (impl `AsyncChunkOps`) + the
  `ClusterChunkService` gRPC surface (PutFragment / GetFragment /
  DeleteFragment / HasFragment), an mTLS-gated SAN-role interceptor
  (`spiffe://cluster/fabric/<node-id>` accepts; tenant SAN
  rejected → I-Auth4 / I-T1), and Prometheus metrics. Per-peer
  channels populate from `cfg.raft_peers`; a 1-node cluster
  degenerates to local-only (D-6).

- **16b — Metadata foundation + repair primitives**.
  - Gateway emits `ChunkAndDelta` proposals — closes the 16a
    Finding 2 gap. `cluster_chunk_state` genuinely populates.
  - Placement plumbing through to `NewChunkMeta`, runtime takes
    `cfg.raft_peers` node-id list verbatim.
  - Composition delete emits `DecrementChunkRefcount` for every
    released chunk so cluster_chunk_state tombstones.
  - Per-cluster-size durability defaults table
    (`defaults_for(size)` → copies + min_acks).
  - Orphan-fragment-scrub (F-D7 mitigation, 24h TTL) and
    under-replication scrub (F-D8 / repair after partition) ship
    as pure-policy + orchestrator-with-trait-objects primitives.
  - EC fragment distribution primitives (`encode_for_placement` /
    `decode_from_responses`) wrapping `kiseki_chunk::ec`.
  - Findings: scrubs not yet wired to a periodic task,
    `DeleteFragment` fan-out on refcount→0 not yet wired, EC
    primitives not yet on the data path, placement is "all peers"
    rather than CRUSH-style. None block production for ≤5-node
    clusters; tracked for 16c.

- **16c — Scrub orchestration + EC data path**. Closes every
  load-bearing 16b finding at the primitive layer:
  - `DeleteFragment` fan-out from leader on refcount→0 (closes
    16b Finding 2). Tombstone signal flows from the per-shard
    Raft state machine through `LogResponse::DecrementOutcome` to
    the gateway's delete loop.
  - CRUSH-style placement (rendezvous hashing) at
    `>target_copies` cluster size (closes 16b Finding 4).
  - `cluster_chunk_state` read API on `LogOps` +
    `LogChunkOracle` adapter for the orphan scrub.
  - `list_chunk_ids` on `ChunkOps` + `AsyncChunkOps` —
    local-store iteration source.
  - `ScrubScheduler::run_once` glues the 16b orphan +
    under-replication primitives to the new iteration plumbing
    (closes 16b Finding 1 at the primitive layer).
  - Fragment-aware `ChunkOps` — per-peer EC fragment storage
    addressed by `(chunk_id, fragment_index)`.
  - `write_chunk_ec` / `read_chunk_ec` on `ClusteredChunkStore`:
    Reed-Solomon 4+2 round-trip with 2 fragments dropped works
    end-to-end (closes 16b Finding 3 at the data-path layer).
  - 16c findings carry over to 16d: scheduler call from runtime,
    `EcStrategy` flag in `ClusterCfg`, server-side multi-
    `fragment_index` PutFragment, `original_len` in
    `cluster_chunk_state`. None block production for ≤5-node
    Replication-3 clusters.

- **16d — Runtime wiring of the EC + scrub data path**. Closes
  every 16c deferred finding at the production wiring layer
  (run in **integrator mode** per the workflow protocol):
  - `EcStrategy` in `ClusterCfg` (default Replication-3) +
    `cluster_nodes` Vec for placement input. `write_chunk` and
    `read_chunk` (the trait surface) dispatch on the strategy.
  - Server-side `PutFragment` / `GetFragment` / `DeleteFragment`
    / `HasFragment` accept any `fragment_index` and route
    index=0 to the legacy whole-envelope path, index>0 to the
    per-fragment store added in 16c step 6.
  - `original_len` threaded through `NewChunkMeta` → state
    machine → `cluster_chunk_state` so EC reads reconstruct
    exactly without the trim-trailing-zeros heuristic.
  - Three production scrub adapters (`LocalChunkDeleter`,
    `FabricAvailabilityOracle`, `FabricRepairer`) +
    `ScrubScheduler::start_periodic` spawn at server startup
    with a 10-minute cadence.
  - 11 RED→GREEN tests across the 5 steps; full integrator
    audit doc at `specs/findings/phase-16d-adversary-audit.md`.
  - Two 16e candidates: EC multi-index repair (FabricRepairer
    today only handles `fragment_index=0`) + scrub task
    graceful shutdown. Neither blocks production for 6+ node
    EC clusters. pNFS DS distinct-fragment parallelism rides
    on the EC switch and is pNFS-team scope.

- **16e — EC primary-feature closure**. Caught during 16d
  close-out: ADR-005 makes EC the **primary** durability mode
  (I-C4 "EC is the default"; I-D1 "repaired from EC parity"),
  not opt-in. 16d's audit had EC repair unimplemented and the
  defaults table still picking Replication-3 even at ≥6 nodes.
  Run in **integrator mode**:
  - `defaults_for(≥6) → EcStrategy::Ec{4, 2}`. Production HPC/AI
    clusters now boot with EC by default (I-C4).
  - `Repairer::repair_ec` decode + re-encode missing fragment.
    Closes I-D1 for production. Deterministic re-encode
    contract verified via RED→GREEN test.
  - `UnderReplicationScrub::with_strategy` + `run_ec` path
    threading `original_len` from `cluster_chunk_state` through.
    Scheduler dispatches based on `cfg.ec_strategy`.
  - Graceful shutdown for the scrub task via
    `tokio::sync::watch` channel. JoinHandle joins cleanly
    within bound on signal.
  - 4 RED→GREEN tests across the steps; full audit at
    `specs/findings/phase-16e-adversary-audit.md`.

- **16f — Cross-node correctness for the gateway path**. Caught by
  `tests/e2e/test_cross_node_replication.py` (added 2026-04-28),
  which had been failing 4-of-4 on every release attempt. Two
  stacked bugs:
  - Inline writes (≤4 KiB) bypassed `ClusteredChunkStore` entirely
    and landed in a per-node `small_store` redb keyed by `chunk_id`.
    The Raft-apply path's inline-data offload uses a different key
    space (`hashed_key XOR seq`), so unifying the two would require
    plumbing on three layers. Fix: in multi-node clusters, disable
    the inline write path (`with_inline_threshold` skipped when
    `fabric_peers > 0`) so every write goes through the chunk /
    fabric path. Single-node clusters keep the optimization. (See
    commit `43bb6a9`.)
  - Even with chunks reaching every node, `CompositionStore` was a
    per-node `HashMap<CompositionId, Composition>` with no
    cross-node visibility, so a follower's S3 GET 404'd at the
    composition layer before consulting the chunk store. Fix:
    `CompositionHydrator` (sibling of the view stream processor)
    polls the Raft delta log every 100 ms and installs leader-
    emitted compositions into the local store via `create_at`.
    Delta payload extended from 16 bytes (just `comp_id`) to
    40 bytes (`comp_id` + `namespace_id` + `bytes_written` LE)
    so followers have everything they need to reconstruct the
    record. Idempotent and crash-safe via `last_applied`. The
    gateway read path acquires a 1 s bounded retry on
    `CompositionNotFound` so a tight PUT-then-GET pattern
    immediately after a leader-side ack absorbs the ~100 ms
    hydrator poll lag. (See commit `d6cfe94`.)
  - Side fix: bootstrap "default" namespace + view installed on
    every node (deterministic UUIDs), not gated on
    `cfg.bootstrap`. Required for the hydrator's `create_at`
    namespace lookup to succeed on followers.
  - Test result: 4-of-4 cross-node scenarios green locally
    against `docker-compose.3node.yml`. Three new unit tests in
    `kiseki-composition::hydrator` cover happy path, idempotency,
    and legacy-payload-skip.

**Phase 16 status: complete for replication and EC.** All findings
from 16a/b/c/d/e closed. Two acknowledged out-of-scope items
(pNFS DS distinct-fragment parallelism — pNFS team; runtime-wide
unified shutdown — runtime-wide concern).

**Phase 17 — Cross-node follow-ups** (in flight). 16f closes
correctness; the architectural debt 16f deliberately deferred is
captured in `specs/implementation/phase-17-cross-node-followups.md`.

- **Item 1 — Update / Delete delta hydration** (commit `7ef59b1`):
  the 16f hydrator only handled `OperationType::Create`; this
  extends it to Update + Delete and wires the gateway's S3 DELETE
  path to actually emit a Delete delta. Encoding: 40-byte Create,
  24-byte Update, 16-byte Delete payloads; op-discriminated by
  the existing delta header field. Lock discipline in the
  gateway's delete path: hold the compositions tokio Mutex
  across emit + local delete to prevent the hydrator from race-
  removing the composition mid-call. Three new hydrator unit
  tests + one new e2e test all pass.
- **Item 4 — Per-shard leader endpoint** (commit `19c4a63`):
  `GET /cluster/shards/{shard_id}/leader` exposes
  `LogOps::shard_health(shard_id)`. The 30-second elapsed-time
  retry in `_put_object` becomes a single attempt + a
  `_wait_for_shard_leader` helper that polls the new endpoint.
  Two new e2e tests (cluster-wide leader agreement; bad-UUID
  rejection) all pass; total cross-node suite is 7/7 green
  locally.
- **Items 2 + 3 — Persistent metadata stores + snapshot
  integration** (architect rev 2 complete; awaiting rev 2
  adversary sign-off):
  ADR-040 (`specs/architecture/adr/040-persistent-metadata-stores.md`)
  through two revisions. Rev 1 (commit `a08e479`) drafted
  redb-backed sibling stores at
  `KISEKI_DATA_DIR/metadata/{compositions,views}.redb`, postcard
  encoding with leading schema-version byte, hot-tail LRU
  cache, sync-only inner locks (never held across await),
  atomic `last_applied + state` redb transactions, two-regime
  snapshot story (D6.1 works today; D6.2 deferred). Rev 1
  adversary review (`specs/findings/adr-040-adversary-review.md`,
  commit `a6eec3c`) flagged 3 Critical + 4 High findings; rev 2
  addresses all seven inline (transient/permanent skip
  algorithm, drops `last_applied_log_index` conflation,
  sequence-comparison gap detection, configurable retry budget,
  typed errors, 13 observability metrics, persistence scope
  limited to `compositions`). Six invariants `I-CP1`..`I-CP6`
  added. **Pending rev 2 adversary sign-off before
  implementation starts.**
- After all items land: auditor pass (Gate 2 step-depth
  verification), integrator pass (cross-context check), final
  adversary pass, then v2026.39+ release.

**Implementation plan**: see
`specs/implementation/phase-16-cross-node-chunks.md` for the
14-step build sequence (proto → Raft state machine → AsyncChunkOps
→ ClusteredChunkStore → gRPC server + interceptor → peer channels
→ runtime wiring → tests → metrics → mTLS SAN → spec docs →
adversary close-out).

**Exit criteria** (16a): cross-node read after leader-only PUT
returns the bytes; read survives single-node failure; 2-of-3
quorum gate surfaces 503 with retry-after when only 1 peer is
reachable; tenant cert presented to the fabric port is rejected
with PermissionDenied; `kiseki_fabric_*` metrics surface in
`/metrics`.

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
  │     │     └── Phase 6 (chunk) ←── Phase 6.5 (block I/O)
  │     │
  ├── Phase 6.5 (block I/O — ADR-029, depends on Phase 0 only)
  ├── Phase 2 (transport)
  │     └── Phase 10 (client)
  │
  ├── Phase 11 (control plane, Rust — ADR-027)
  │
  └── Phase 12 (integration — final)
```

**Parallelism opportunities**:
- Phase 11 (control plane) can be built in parallel with Phases 3-10
- Phase 2 (transport) can be built in parallel with Phases 1, 3, 4
- Phase 5 (audit) can start as soon as Phase 3 is done
- Phase 6.5 (block I/O) depends only on Phase 0 and can be built in parallel with Phases 1-5
- Phase 11.5 (advisory) can start as soon as Phase 5 (audit) and the Phase 11 advisory-policy endpoint are done; it is independent of Phases 6-10 because it does not link against any data-path crate
