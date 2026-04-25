# Assumptions Log — Kiseki

**Status**: Active — updated for ADR-028 (External KMS Providers) and ADR-033/034/035 (cluster topology, shard merge, node lifecycle — *spec-only, enforcement deferred*).
**Last updated**: 2026-04-25.

Statuses: **Validated** (confirmed true), **Rejected** (confirmed false),
**Accepted** (acknowledged risk, proceeding), **Unknown** (needs investigation).

---

## Existential / framing

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-E1 | Kiseki's differentiators from DeltaFS are real: persistence, multi-tenancy, standard protocols, first-class encryption | **Validated** | Domain expert confirmed. These are the design primitives that differ, not post-hoc rationalization. |
| A-E2 | Target maturity is production-grade (replaces Lustre on ClusterStor hardware) | **Validated** | Domain expert selected option (c). Changes rigor bar for all failure modes, observability, and operational concerns. |
| A-E3 | Pure Rust core, no Mochi dependency | **Validated** | Mochi has not been deployed in regulated environments. C/C++ FFI creates compliance surface risk. Domain expert confirmed pure Rust, learn from Mochi's patterns. |
| A-E4 | Threat model: malicious insider + physical theft + network observer + full regulatory (HIPAA, GDPR, revFADP) | **Validated** | Strongest threat model. Cluster admin is untrusted with plaintext. |

## Regulatory

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-R1 | FIPS 140-2/3 validated crypto modules required | **Accepted** | HIPAA safe harbor requires FIPS. Rust options: aws-lc-rs (FIPS-validated), ring (FIPS-capable). If FIPS not actually required, algorithm choices widen. |
| A-R2 | Post-quantum crypto readiness is future work, not initial build | **Accepted (risk)** | Envelope format MUST carry algorithm identifiers from day one to support PQ migration. If a regulatory requirement lands during build, this becomes urgent. |
| A-R3 | Data residency constraints apply (revFADP cross-border transfer rules) | **Accepted** | Multi-site federation is in scope; data residency enforced per site. If no actual cross-border deployments planned, this simplifies. |

## Architecture

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A1 | Workloads dominated by large sequential reads, bulk writes, object-access | **Accepted** | Domain expert confirmed. Small-random-write POSIX is bounded-tolerance (hint, not hard commitment). If actual workloads differ, architecture is wrong for the target. |
| A2 | Current DAOS still has reliability problems | **Unknown** | If false, build-vs-adopt calculus changes. Domain expert committed to building regardless (production-intent), but should be re-evaluated. |
| A4 | ClusterStor E1000/E1000F hardware is sufficient for envisioned tenant count | **Unknown** | If false, scale targets are wrong. |
| A5 | Tenants will tolerate bounded-staleness cross-protocol reads | **Accepted** | CP for writes confirmed. Bounded-staleness for reads confirmed. If a tenant needs strict cross-protocol read-your-writes, the consistency model needs rework. |
| A6 | Raft-per-shard is operationally acceptable | **Accepted (risk)** | If shard count grows into thousands, Raft overhead may dominate. Shard split/merge thresholds are configurable to manage this. |
| A7 | FUSE overhead is acceptable for the POSIX path | **Accepted (risk)** | If workloads are latency-sensitive on POSIX, FUSE may be too slow. No kernel modules per design commitment. |
| A8 | Reactive tiering within declarative bounds is stable | **Unknown** | If false, auto-tiering thrashing could recur. |
| A9 | Slingshot CXI provider in libfabric is mature enough for production | **Unknown** | If false, transport strategy for Slingshot path needs rework. TCP fallback exists. |
| A10 | Rust async ecosystem (tokio) supports storage-system workload patterns | **Accepted** | If async contention is a bottleneck, may need blocking threads for some paths. |

## Encryption

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-K1 | Two-layer encryption model (C): system encrypts data, tenant wraps access via key wrapping | **Validated** | Domain expert confirmed. Single data encryption pass + key wrapping. No double encryption. |
| A-K2 | Epoch-based key rotation with full re-encryption as admin action | **Validated** | Default: new data uses new epoch, background migration for old. Admin can force full re-encryption on key compromise. |
| A-K3 | Cross-tenant dedup enabled by default, tenant opt-out via HMAC chunk IDs | **Validated** | Opt-out tenants get HMAC-derived chunk IDs (per-tenant keyed hash). Opt-out is effectively one-way — re-opting-in requires data re-ingestion or background migration. |
| A-K4 | Content-address hash (chunk ID) computed on plaintext before encryption | **Validated** | Co-occurrence leak (existence confirmation with candidate plaintext) accepted as bounded risk. Mitigated by: tenant-scoped chunk ID namespace for opted-out tenants, chunk IDs stored within system-encrypted layer, dedup ratios subject to tenant access controls. |
| A-K5 | Crypto-shred destroys tenant KEK only; system encryption remains | **Validated** | Physical GC runs separately on refcount=0 + no retention hold. Retention holds must be set before crypto-shred. |
| A-K6 | NIC-offloaded wire encryption on Slingshot Cassini is not production-ready | **Unknown** | If false, one-sided RDMA with encryption becomes more viable. If true, CPU-encrypt or design around it. |
| A-K7 | Tenants choose external KMS provider at onboarding. Five options: Kiseki-Internal (default), Vault, KMIP 2.1, AWS KMS, PKCS#11. Provider cannot be changed without full re-wrapping (ADR-028). | **Validated** | ADR-028 accepted 2026-04-22. Internal provider is zero-config default. |
| A-K7b | Each external KMS provider's availability is the tenant's responsibility. Kiseki provides cached-key window (5s-300s TTL, I-K15) but no SLA harmonization across providers. | **Accepted (risk)** | Provider connectivity varies; tenant designs for multi-region failover if required. |
| A-K7c | Tenant manages KMS provider credentials (Vault AppRole rotation, AWS IAM role rotation, KMIP cert renewal, PKCS#11 PIN). Kiseki does not automate credential lifecycle. Credential expiry = key access loss (same as F-K1). | **Accepted** | Documented in ADR-028 security considerations. |

## Tenancy

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-T1 | Tenant hierarchy: org → [project] → workload | **Validated** | Project is optional. Minimum: org → workload. |
| A-T2 | Compliance regime tags attach at any level, inherit downward, union-of-constraints | **Validated** | Strongest requirement wins per dimension. A namespace can carry HIPAA + GDPR simultaneously. |
| A-T3 | Cluster admin cannot access tenant config/logs/data without tenant admin approval | **Validated** | Zero-trust infra/tenant boundary. Approval workflow required. |
| A-T4 | Network isolation is VLAN-based; tenants can share VLANs | **Validated** | VLAN is not the isolation primitive — crypto + IAM enforce isolation. KMS/IAM accessible on tenant's VLAN. |

## Deployment

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-D1 | Multi-site is federated-async | **Validated** | Independent clusters per site. Async replication. No cross-site Raft. |
| A-D2 | CP for writes, bounded-staleness for reads | **Validated** | Regulatory data integrity drives write-consistency requirement. |
| A-D3 | Full feature build in phases, no incremental releases | **Validated** | Feature completeness at launch. Build phases are internal sequencing. No public v1/v2 split — spec covers the complete system. |
| A-D4 | Small-random-write POSIX I/O: bounded tolerance, not target | **Accepted (hint)** | Leaning toward "works for metadata-heavy ops but not data-heavy random I/O." Not a hard commitment yet; firm up during behavioral spec. |

---

## Escalation points for architect

These emerged during interrogation and are explicitly NOT the analyst's
decisions:

1. ~~KMS deployment topology~~ — **RESOLVED** by ADR-028: pluggable TenantKmsProvider trait with 5 backends
2. ~~Shard split/merge threshold tuning: who configures (cluster admin, tenant admin, both)?~~ — **RESOLVED** by analyst layer 2 update 2026-04-25: cluster admin sets cluster-wide defaults and per-tenant min/max envelope; tenant admin may override per namespace within that envelope. See A-N5.
3. System DEK granularity: per-chunk vs. per-group
4. FIPS module boundary: aws-lc-rs vs. ring vs. other
5. Flavor best-fit matching algorithm
6. Inline data threshold for deltas (suggested 4-8KB)
7. System key manager HA mechanism (internal Raft? Paxos?)
8. Native client bootstrap/discovery mechanism on data fabric
9. Cache TTL defaults for tenant KEK and system DEK
10. EC parameters (k, m) per pool type
11. MVCC pin TTL defaults

---

## New assumptions from Layers 3-5

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-B1 | Chunk writes are idempotent (same chunk_id = increment refcount, not reject) | **Validated** | Domain expert confirmed. More performant than first-writer-wins. |
| A-B2 | Write buffering during shard split causes brief latency bump (acceptable) | **Validated** | Domain expert preferred clean split over misplaced deltas with migration. |
| A-B3 | Stalled consumer alerts go to both cluster admin and tenant admin | **Validated** | Different concerns: cluster admin (GC blocked), tenant admin (view stale). |
| A-B4 | System key manager is an internal Kiseki service (not external dependency) | **Validated** | Must be at least as available as the Log. P0 if it fails. |
| A-B5 | Maintenance mode is the mechanism for deferring shard splits | **Validated** | Read-only mode removes write pressure; splits defer naturally. |
| A-B6 | Log compaction and truncation are both auto and admin-triggerable | **Validated** | Operators need explicit control for debugging and maintenance. |
| A-O1 | Shard split does not block writes (buffering during transition) | **Validated** | Brief latency bump acceptable; no write failures during split. |
| A-T-1 | Dual clock model (HLC for ordering, wall clock for duration) adapted from taba | **Validated** | HLC authoritative for causality; wall clock for retention/staleness/audit. |
| A-T-2 | mTLS with Cluster CA for data-fabric auth; optional tenant IdP second stage | **Validated** | SPIFFE/SPIRE as alternative. Cluster admin via Control Plane IAM. |
| A-T-3 | Compression off by default, tenant opt-in with padding | **Validated** | CRIME/BREACH side-channel mitigated. Compliance tags may prohibit. |

## Workflow advisory assumptions (ADR-020)

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-ADV-1 | Clients self-declare their workload profile accurately. A dishonest profile declaration does not unlock capabilities the workload is not already authorized for (I-WA14), but it may cause steering to be suboptimal for that caller. | **Accepted (risk)** | Lying about profile can only harm the caller's own QoS (e.g., declaring `interactive` when actually `batch` may cause the caller's own hints to be rejected by its policy). If a future incentive structure rewards lying (e.g., billing discounts per profile), this assumption needs revisiting with cryptographic attestation of profile choice. |
| A-ADV-2 | k=5 neighbour anonymity is sufficient to prevent single-tenant inference from cluster-level telemetry aggregates. | **Accepted (risk)** | Standard k-anonymity threshold from literature. If neighbour workloads correlate (e.g., all HPC checkpoint at the same time), k=5 may be inadequate; policy allows raising k per pool. |
| A-ADV-3 | 1 hour default TTL and 64-deep phase history are adequate for typical HPC and AI workflows. | **Accepted (risk)** | Most HPC jobs fit in 1-24 h; most AI training phases number in tens (epochs, eval, stage-in/out). Long-running (multi-day) workflows can extend TTL up to 24 h per workload policy. If typical workflows exceed these bounds, defaults should be raised. |
| A-ADV-4 | The advisory subsystem can be physically isolated from the data path sufficiently (separate runtime, separate buffers) that even a severe advisory overload never back-propagates to data-path latency. | **Accepted (risk)** | Architect must validate on the target runtime (tokio task-scheduler starvation, shared memory allocator pressure). If not physically separable, the isolation guarantee of I-WA2 cannot be upheld without per-process separation (a larger structural change). |
| A-ADV-5 | Hint audit batching (I-WA8) preserving per-second per-workflow counts is adequate for tenant security auditors. | **Accepted (risk)** | If auditors require per-event granularity for hint-accepted/throttled events, batching must be disabled. Declare/end/phase/policy-violation events remain per-event. |

## Client-side cache assumptions (ADR-031)

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-CC1 | Compute-node local NVMe is in the same trust domain as process memory for the purpose of storing decrypted plaintext. | **Accepted (risk)** | Standard HPC assumption: compute nodes are tenant-allocated and isolated. If nodes are shared across tenants without adequate isolation (e.g., bare-metal multi-tenant without containers), the plaintext cache is a cross-tenant leak vector. Mitigated by file permissions (0600) and per-tenant pool directories. |
| A-CC2 | Software zeroize on NVMe/SSD provides adequate erasure for the threat model. Physical block remapping by the FTL may leave residual data on flash. | **Accepted (risk)** | Consistent with I-K8 (zeroize for heap memory). For environments requiring physical erasure, OPAL/SED NVMe with per-boot key rotation is recommended as operational hardening. |
| A-CC3 | 5-second metadata TTL default is acceptable staleness for the target workload mix. Training runs (epoch reuse) and inference (static model) tolerate minutes of staleness. Climate simulations with hard deadlines use pinned mode (no TTL dependency). | **Accepted (risk)** | If workloads require sub-second freshness, they should use bypass mode or set `metadata_ttl_ms` lower. |
| A-CC4 | `kiseki-cache-scrub` service will be deployed on compute nodes. If not deployed, crashed-process plaintext cleanup depends on the next kiseki process starting on the same node for the same tenant. | **Accepted (risk)** | Scrub service is a one-shot systemd unit or cron entry. Straightforward to deploy. If omitted, the security posture degrades to "plaintext persists until next same-tenant process or node reboot." |

## Cluster topology and node-lifecycle assumptions (ADR-033, ADR-034, ADR-035)

| # | Assumption | Status | Evidence / risk if false |
|---|---|---|---|
| A-N1 | Default initial shard count of `max(min(3 × node_count_at_creation, 64), 3)` gives day-one multi-leader distribution while bounding Raft-group overhead. | **Accepted (risk)** | The 3× multiplier is a starting point picked to give every node ~3 shards on average. The 64 cap is a soft limit chosen because each shard = 1 Raft group; 64 groups per cluster is well-tested in similar systems (Ceph PGs scale higher, but with batched heartbeats kiseki-raft does not yet support). If kiseki-raft adds Multi-Raft-style heartbeat batching, the cap can be raised. If the 3× starting ratio proves too sparse for typical workloads, the cluster admin may raise the multiplier without spec change. |
| A-N2 | Ratio floor of 1.5× is sufficient to avoid stranding nodes idle when one shard is hot. | **Accepted (risk)** | At ratio 1.5, every node hosts on average 1.5 shards; even if one shard is hot enough to dominate a single node's resources, every other node has at least one alternative leader. Below 1.5 (e.g., 1.0), a single hot shard can monopolize a node. The floor is cluster-admin configurable; if observed workloads need higher headroom, raise it. |
| A-N3 | Best-effort round-robin leader placement at creation/split/merge is sufficient for steady-state balance; explicit periodic rebalance is not required for correctness. | **Accepted (risk)** | No invariant enforces post-hoc balance. Drift between events is permitted. If observed drift consistently strands nodes, an explicit rebalance ADR may follow. Operators have a manual rebalance lever (out of scope for ADR-033). |
| A-N4 | The drain-refusal-at-RF-floor policy (I-N4) is the correct safety stance — refusing the drain is preferable to allowing temporary RF<3 with alarms. | **Validated** | Confirmed by domain expert 2026-04-25 (Q1=a). Trades operator inconvenience for durability invariant preservation; aligned with I-CS1 (CP for writes) and the regulatory threat model A-E4. |
| A-N5 | Threshold ownership: cluster admin sets cluster-wide split/merge defaults and per-tenant min/max envelopes; tenant admin may override per namespace within the envelope. | **Validated** | Confirmed by analyst inference 2026-04-25 (resolves prior open escalation #2). Prevents one tenant from saturating Raft-group budget; tenant admin retains tuning control for their workloads' characteristics. |
| A-N6 | The merge interval default of 24 hours is long enough to avoid thrash from transient utilization dips but short enough to reclaim Raft-group overhead within an operationally acceptable window. | **Accepted (risk)** | Tunable per cluster admin. If observed workloads have day-cycle utilization patterns (e.g., quiet overnight, busy during day), the interval may need raising to a multiple of 24 h to avoid daily merge/split oscillation. |
| A-N7 | Drain cancellation (I-N7) is operationally desirable and does not compromise correctness — completed voter replacements are not rolled back, which is the simplest safe behavior. | **Accepted (risk)** | Inferred 2026-04-25; not explicitly confirmed by domain expert. Alternative behaviors considered (auto-rollback, no cancellation at all) are more complex; if user feedback indicates rollback is required for operational reasons, revisit. **Architect should confirm before ADR-035 implementation.** |
