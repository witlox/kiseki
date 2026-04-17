# Failure Modes — Kiseki

**Status**: Layer 5 — derived from Layers 1-4 interrogation.
**Last updated**: 2026-04-17, Session 2.

Each failure mode has: description, blast radius, detection mechanism,
desired degradation, and severity.

Severity scale: **P0** (cluster-wide outage), **P1** (tenant-wide outage),
**P2** (shard/namespace scoped), **P3** (single component, limited impact).

---

## Infrastructure failures

### F-I1: System key manager quorum loss

| Field | Value |
|---|---|
| **Description** | Internal HA system key manager loses Raft quorum |
| **Blast radius** | Cluster-wide write outage. All chunk encryption blocked. |
| **Detection** | System key manager heartbeat / health check |
| **Degradation** | Reads continue using cached system DEKs (bounded TTL). Writes rejected with retriable error. |
| **Recovery** | Restore system key manager quorum. No data loss if quorum restored before cache TTL expires. |
| **Severity** | **P0** |

### F-I2: Storage node failure

| Field | Value |
|---|---|
| **Description** | A storage node becomes permanently unreachable |
| **Blast radius** | Chunks on failed node's devices; Raft groups with members on this node |
| **Detection** | Heartbeat timeout, device health checks |
| **Degradation** | Chunk repair from EC parity or replicas. Raft groups re-elect if leader was on this node. Shards with members on this node have one fewer replica until replacement. |
| **Recovery** | Replace node. Rebalance chunk placement. Add new Raft member. |
| **Severity** | **P2-P3** (depends on how many shards/chunks affected) |

### F-I3: Network partition (fabric level)

| Field | Value |
|---|---|
| **Description** | Slingshot fabric partition isolates a subset of nodes |
| **Blast radius** | Raft groups split across the partition may lose quorum. Views on one side may stale. |
| **Detection** | Raft heartbeat failures, transport connection timeouts |
| **Degradation** | CP for writes: partitioned minority cannot write. Majority side continues. Reads from stale views continue with staleness warnings. |
| **Recovery** | Fabric repair. Partitioned nodes rejoin, catch up via Raft log replay. |
| **Severity** | **P1-P2** (depends on partition scope) |

### F-I4: Disk/device failure

| Field | Value |
|---|---|
| **Description** | Individual NVMe device fails in an affinity pool |
| **Blast radius** | Chunks with EC fragments or replicas on that device |
| **Detection** | Device health monitoring, I/O errors |
| **Degradation** | EC repair for affected chunks using surviving fragments/replicas. Pool operates at reduced redundancy until repair completes. |
| **Recovery** | Replace device. Rebalance and re-protect affected chunks. |
| **Severity** | **P3** |

---

## Consensus failures

### F-C1: Raft leader loss (per shard)

| Field | Value |
|---|---|
| **Description** | Shard's Raft leader becomes unreachable |
| **Blast radius** | One shard — transient write unavailability during election |
| **Detection** | Raft heartbeat timeout |
| **Degradation** | New leader elected (seconds). In-flight uncommitted writes retried by Composition. Committed writes safe. Reads from views continue (stale during election). |
| **Recovery** | Automatic (Raft election). No operator action needed. |
| **Severity** | **P2** (transient) |

### F-C2: Raft quorum loss (per shard)

| Field | Value |
|---|---|
| **Description** | Majority of Raft members unreachable for a shard |
| **Blast radius** | One shard — all namespaces in that shard lose write availability |
| **Detection** | Raft cannot form majority; write ack timeout |
| **Degradation** | Writes fail with retriable error. Views serve last-known state (potentially stale). No data loss for committed deltas. |
| **Recovery** | Restore at least one more member. If permanent loss: Raft reconfiguration (operator action). |
| **Severity** | **P2** |

### F-C3: Log corruption (per shard)

| Field | Value |
|---|---|
| **Description** | Shard's log cannot be replayed due to SSTable corruption |
| **Blast radius** | Catastrophic for the shard's compositions. All compositions in affected namespaces potentially unrecoverable from this shard. |
| **Detection** | Checksum failure on SSTable read, WAL checksum on replay |
| **Degradation** | Attempt repair from Raft replicas (other members may have uncorrupted copy). If all replicas corrupted: compositions are lost unless views have materialized state that can be used as a recovery point. |
| **Recovery** | Replay from uncorrupted replica. If no uncorrupted replica: partial recovery from materialized views (lossy). Operator-triggered integrity reconstruction (I-O5) with tenant key. |
| **Severity** | **P1** (potentially data loss) |

---

## Key management failures

### F-K1: Tenant KMS temporarily unreachable

| Field | Value |
|---|---|
| **Description** | Tenant's external KMS is unreachable |
| **Blast radius** | One tenant — all operations for that tenant |
| **Detection** | KMS connection failure, unwrap timeout |
| **Degradation** | Cached KEK sustains operations within TTL. After TTL: reads and writes fail for that tenant. Other tenants unaffected. |
| **Recovery** | Restore KMS connectivity. Operations resume automatically when KMS is reachable. |
| **Severity** | **P1** (tenant-scoped) after cache TTL |

### F-K2: Tenant KMS permanently lost

| Field | Value |
|---|---|
| **Description** | Tenant's KMS infrastructure destroyed, no backups |
| **Blast radius** | Total data loss for that tenant — all data is unreadable |
| **Detection** | Prolonged KMS unreachability, tenant admin reports |
| **Degradation** | None. This is unrecoverable by design (I-K11). |
| **Recovery** | None. Tenant is responsible for KMS backups. System-encrypted ciphertext remains on disk (under retention holds) but is permanently unreadable without tenant KEK. |
| **Severity** | **P1** (tenant data loss — permanent) |

### F-K3: Key compromise — tenant KEK exposed

| Field | Value |
|---|---|
| **Description** | Attacker obtains a copy of a tenant KEK |
| **Blast radius** | All data encrypted under that KEK is potentially compromised |
| **Detection** | Out-of-band (audit, threat intel, tenant report) |
| **Degradation** | Immediate key rotation (epoch-based). Tenant admin triggers full re-encryption as admin action. Old KEK invalidated. |
| **Recovery** | Full re-encryption with new KEK. Incident response: audit log review, scope assessment. Crypto-shred old epoch keys after re-encryption completes. |
| **Severity** | **P1** (security incident) |

### F-K4: Key compromise — system KEK exposed

| Field | Value |
|---|---|
| **Description** | Attacker obtains the system KEK |
| **Blast radius** | All system DEKs can be unwrapped. Combined with tenant KEK: full data access. Without tenant KEK: attacker can decrypt system layer but not tenant layer. |
| **Detection** | Out-of-band (security audit, intrusion detection) |
| **Degradation** | System KEK rotation. Full re-encryption of all system DEK wrappings. |
| **Recovery** | Rotate system KEK. Re-wrap all system DEKs with new system KEK. If attacker also has tenant KEK: full re-encryption of affected data. |
| **Severity** | **P0** (security incident) |

---

## Data path failures

### F-D1: Protocol gateway crash

| Field | Value |
|---|---|
| **Description** | Gateway process crashes or is killed |
| **Blast radius** | One tenant's clients on that protocol lose connection |
| **Detection** | Liveness probe, client connection loss |
| **Degradation** | Restart gateway. Clients reconnect. NFS state (opens, locks) lost — clients re-establish. In-flight uncommitted writes lost. Committed writes safe. |
| **Recovery** | Automatic restart. No data loss for committed writes. |
| **Severity** | **P3** |

### F-D2: Native client crash

| Field | Value |
|---|---|
| **Description** | Workload process crashes (takes native client with it) |
| **Blast radius** | One workload's in-flight operations |
| **Detection** | Connection loss at Chunk Storage / Log |
| **Degradation** | Uncommitted writes lost. Committed writes safe. No cluster impact. |
| **Recovery** | Workload restarts, native client re-initializes, rediscovers, resumes. |
| **Severity** | **P3** |

### F-D3: Stream processor falls behind (staleness violation)

| Field | Value |
|---|---|
| **Description** | Stream processor cannot keep up with delta production rate |
| **Blast radius** | One view becomes stale beyond its configured bound |
| **Detection** | Watermark lag exceeds staleness bound |
| **Degradation** | Reads from stale view may include staleness warning header. Alerts to cluster admin (view stalled) and tenant admin (data stale). |
| **Recovery** | Stream processor catches up when load decreases. If persistently behind: scale stream processor resources or relax staleness bound. |
| **Severity** | **P2** |

### F-D4: Compaction storm

| Field | Value |
|---|---|
| **Description** | Background compaction cannot keep up with write rate |
| **Blast radius** | One shard — read amplification grows, tail latency increases |
| **Detection** | SSTable count per shard exceeds threshold |
| **Degradation** | Back-pressure on writes (increased write latency). DeltaFS empirically confirmed this is the tail-latency-defining failure mode for LSM systems. |
| **Recovery** | Write rate decreases or compaction resources increase. Admin-triggered compaction may help. Worst case: temporary write throttling. |
| **Severity** | **P2** |

### F-D5: Chunk loss — unrecoverable

| Field | Value |
|---|---|
| **Description** | EC parity insufficient to recover a lost chunk |
| **Blast radius** | All compositions referencing that chunk |
| **Detection** | EC verification failure during read or scrub |
| **Degradation** | Affected compositions have gaps. Reads for the lost byte range fail. Other byte ranges in the same composition may still be readable. |
| **Recovery** | Data loss acknowledged. If the data exists in another composition (dedup): recoverable from there. Otherwise: permanent loss. |
| **Severity** | **P2** (data loss — localized) |

---

## Operational failures

### F-O1: Control plane unavailability

| Field | Value |
|---|---|
| **Description** | Control Plane service is down |
| **Blast radius** | No new tenants, namespaces, policy changes, or placement decisions |
| **Detection** | Health check, API unavailability |
| **Degradation** | Data path continues with cached config. Quota enforcement approximate. Federation config sync stalls. |
| **Recovery** | Restore Control Plane. Reconcile quota drift. Resume federation sync. |
| **Severity** | **P1** (management outage, data path continues) |

### F-O2: Shard split failure during high write load

| Field | Value |
|---|---|
| **Description** | Shard at hard ceiling, split in progress, high write rate |
| **Blast radius** | Writes to the splitting shard experience latency bump (buffered) |
| **Detection** | Split duration monitoring, write latency metrics |
| **Degradation** | Writes buffered during split. Brief latency increase. No data loss. |
| **Recovery** | Split completes. Write buffering drains. Normal latency resumes. |
| **Severity** | **P3** (transient) |

### F-O3: Federation peer unreachable

| Field | Value |
|---|---|
| **Description** | Async replication to/from a federated site fails |
| **Blast radius** | Cross-site config sync stalls. Data replication falls behind. |
| **Detection** | Replication lag monitoring, peer heartbeat |
| **Degradation** | Local site continues independently. Async replication catches up when peer is reachable. Data residency constraints remain enforced locally. |
| **Recovery** | Restore connectivity. Replication resumes and catches up. |
| **Severity** | **P2** |

---

## Crypto-specific failures

### F-X1: Crypto-shred incomplete (cached key survives)

| Field | Value |
|---|---|
| **Description** | Tenant KEK destroyed in KMS, but cached copy exists in gateway/client memory |
| **Blast radius** | Data accessible from cached key until cache TTL expires |
| **Detection** | Essentially undetectable in real-time |
| **Degradation** | Bounded by cache TTL. After TTL: all cached copies expire. |
| **Mitigation** | Short cache TTLs. Explicit invalidation propagation on crypto-shred. Audit logs of key-material lifecycle. |
| **Severity** | **P2** (bounded window) |

### F-X2: Algorithm deprecation

| Field | Value |
|---|---|
| **Description** | An encryption algorithm in use becomes unsafe (CVE, NIST advisory) |
| **Blast radius** | All data encrypted with the deprecated algorithm |
| **Detection** | External (NIST, CVE database, security advisory) |
| **Degradation** | Envelope format carries algorithm identifiers. System supports multiple algorithms concurrently during migration. |
| **Mitigation** | Background re-encryption to new algorithm. Epoch-based migration. |
| **Recovery** | Full re-encryption. Old-algorithm data replaced over time. |
| **Severity** | **P1** (security, but migration is possible due to crypto-agility) |

### F-X3: Replay attack on encrypted log

| Field | Value |
|---|---|
| **Description** | Attacker replays captured encrypted deltas |
| **Blast radius** | Depends on whether AEAD binds to log position/nonce |
| **Detection** | Sequence number enforcement; AEAD with monotonic nonces |
| **Mitigation** | AEAD nonces tied to log position (sequence_number). Replayed delta has wrong position → authentication fails. |
| **Severity** | **P3** (mitigated by design) |

---

## Workflow advisory failures (ADR-020)

### F-ADV-1: Advisory subsystem outage

| Field | Value |
|---|---|
| **Description** | The Workflow Advisory subsystem becomes unresponsive (crash, overload, network partition to the advisory runtime) on one or more serving nodes. |
| **Blast radius** | Steering quality only. Clients observe `advisory_unavailable` on hint submission and lose telemetry feedback for affected workflows. No data-path operation is blocked, delayed, or reordered (I-WA2). |
| **Detection** | Health probes on the advisory runtime; advisory-channel heartbeats from clients; declare/hint error rate. |
| **Degradation** | In-flight data-path operations succeed with full correctness. New DeclareWorkflow calls return `advisory_unavailable`; native clients fall back to pattern-inference (pre-existing behavior) for prefetch and access-pattern heuristics. |
| **Recovery** | Restart the advisory runtime. Clients redeclare. Prior workflow state is ephemeral and not recovered. |
| **Severity** | **P2** (scoped to advisory steering quality; no correctness or durability impact) |

### F-ADV-2: Advisory audit storm

| Field | Value |
|---|---|
| **Description** | A misbehaving or malicious workload submits hints at or near its budget, with a high rejection rate, driving audit-event volume toward the tenant audit shard's capacity. |
| **Blast radius** | Tenant audit shard throughput and the I-L4 / I-A4 GC-consumer relationship. Without mitigation, could block data-shard GC for the affected tenant (bounded by I-A5 safety valve). |
| **Detection** | Audit write rate per tenant; advisory-audit backpressure counters. |
| **Degradation** | I-WA8 batching/sampling for `hint-accepted` and `hint-throttled` reduces steady-state volume. I-A5 audit GC safety valve permits data GC to proceed past a documented gap when audit stalls >24 h. Budget reductions via control plane reduce the offending workload's hint rate. |
| **Recovery** | Tenant admin or automated policy narrows the offending workload's `hints_per_sec`. Audit shard catches up. |
| **Severity** | **P2** (tenant-scoped; safety valves exist) |

---

## Failure severity summary

| Severity | Count | Examples |
|---|---|---|
| P0 | 2 | System key manager loss, system KEK compromise |
| P1 | 6 | Tenant KMS loss, log corruption, key compromise, algo deprecation, control plane down, network partition (wide) |
| P2 | 9 | Shard quorum loss, compaction storm, stale view, federation peer down, chunk loss, crypto-shred window, network partition (narrow), advisory outage, advisory audit storm |
| P3 | 5 | Gateway crash, client crash, device failure, split latency, replay attack |

Total: **22 failure modes** catalogued with blast radius, detection,
degradation, and recovery.
