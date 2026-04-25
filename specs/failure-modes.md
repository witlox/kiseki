# Failure Modes — Kiseki

**Status**: Layer 5 — derived from Layers 1-4 interrogation.
**Last updated**: 2026-04-22. Added ADR-028 per-provider failure modes and ADR-029 block I/O failure modes.

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

### F-I5: Bitmap corruption on data device (ADR-029)

| Field | Value |
|---|---|
| **Description** | Allocation bitmap on a raw block device is corrupted (bit flip, partial write) |
| **Blast radius** | One device's chunks — space accounting incorrect for that device |
| **Detection** | Checksum mismatch on bitmap read (superblock carries bitmap checksum) |
| **Degradation** | Device marked degraded. Reads for existing chunks unaffected (chunk_meta in redb is intact). New allocations on this device suspended. |
| **Recovery** | Rebuild bitmap from redb `chunk_meta` reverse scan (`device_alloc` table). All extents recorded in redb are marked allocated; all others freed. Automatic on detection. |
| **Severity** | **P3** |

### F-I6: Extent leak — blocks allocated but chunk_meta not written (ADR-029)

| Field | Value |
|---|---|
| **Description** | Blocks allocated in bitmap and journaled in redb, but chunk_meta entry never written (crash between allocation and chunk write completion) |
| **Blast radius** | Wasted space on one device — orphan extents consume capacity but hold no valid data |
| **Detection** | Periodic scrub comparing bitmap allocations vs redb `chunk_meta` entries. Orphan extents have bitmap bits set but no corresponding `chunk_meta` record. |
| **Degradation** | Space waste only. No data loss. No read impact. |
| **Recovery** | Scrub frees orphan extents (clear bitmap bits, remove `device_alloc` journal entry). Scrub runs periodically (default: every 6 hours) and on device startup. |
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

### F-K1a: Vault provider — sealed or token expired (ADR-028)

| Field | Value |
|---|---|
| **Description** | Vault is sealed, storage backend offline, or auth token expired |
| **Blast radius** | One tenant using Vault provider |
| **Detection** | Wrap/unwrap returns 503 (sealed) or 403 (token expired) |
| **Degradation** | Cached KEK sustains reads within TTL. Circuit breaker opens after 5 failures. |
| **Recovery** | Unseal Vault or refresh auth token. Circuit breaker half-open probe resumes. |
| **Severity** | **P1** (tenant-scoped) |

### F-K1b: KMIP provider — protocol or certificate failure (ADR-028)

| Field | Value |
|---|---|
| **Description** | KMIP server offline, protocol version incompatible, or client cert revoked |
| **Blast radius** | One tenant using KMIP provider |
| **Detection** | TTLV decode failure, connection refused, or TLS handshake error |
| **Degradation** | Cached KEK sustains reads within TTL. |
| **Recovery** | Restore KMIP server or renew client certificate. |
| **Severity** | **P1** (tenant-scoped) |

### F-K1c: AWS KMS — rate limit or region outage (ADR-028)

| Field | Value |
|---|---|
| **Description** | AWS KMS ThrottlingException, region unavailable, or IAM role expired |
| **Blast radius** | One tenant using AWS KMS provider |
| **Detection** | HTTP 429 (throttle) or 503 (region). IAM: 403. |
| **Degradation** | Cached derivation params sustain reads within TTL. Rate limiting triggers backoff. |
| **Recovery** | Wait for rate limit reset, region recovery, or IAM role refresh. |
| **Severity** | **P1** (tenant-scoped) |

### F-K1d: PKCS#11 — HSM disconnect or PIN lockout (ADR-028)

| Field | Value |
|---|---|
| **Description** | HSM device disconnected, session expired, or PIN lockout after failed attempts |
| **Blast radius** | One tenant using PKCS#11 provider |
| **Detection** | C_WrapKey/C_UnwrapKey returns CKR_TOKEN_NOT_PRESENT or CKR_PIN_LOCKED |
| **Degradation** | Cached derivation params sustain reads within TTL. No fallback for PIN lockout. |
| **Recovery** | Reconnect HSM or reset PIN via HSM admin console (out-of-band). |
| **Severity** | **P1** (tenant-scoped) |

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

### F-O4: Drain interrupted mid-flight (ADR-035, spec-only)

| Field | Value |
|---|---|
| **Description** | Drain orchestrator crashes, network partitions, or operator cancels partway through a node drain. Some shards have completed voter replacement; others are in mid-promotion or have not started. |
| **Blast radius** | One node — leader/voter placements may be partially shifted. RF=3 is preserved at every observable state because voter replacement is "add new + catch up + promote + remove old" sequenced (I-N3). |
| **Detection** | Drain orchestrator heartbeat absence; cluster admin observes `Draining` state with stalled progress; audit log shows incomplete transition. |
| **Degradation** | The node remains in `Draining` state. Cluster operates correctly under the partial placement. Reads and writes proceed. New leader assignments avoid the draining node (I-N2). |
| **Recovery** | Restart drain orchestrator: it resumes per-shard from the persisted state (which voters are pending promotion, which have not started). Or: operator cancels drain (I-N7) and the node returns to Active; completed voter replacements are not rolled back. |
| **Severity** | **P3** (transient; correctness preserved) |

### F-O5: Drain refused — RF cannot be maintained (ADR-035, spec-only)

| Field | Value |
|---|---|
| **Description** | Operator requests `DrainNode(X)` on a cluster where completing the drain would leave at least one shard with insufficient surviving voters to satisfy RF=3 (I-N3, I-N4). |
| **Blast radius** | None — the request is refused before any state change. |
| **Detection** | Up-front validation in the control plane checks every shard's voter set against the post-drain node count. |
| **Degradation** | The drain is rejected with `DrainRefused: insufficient capacity to maintain RF=N`. Node X stays in `Active`. |
| **Recovery** | Operator adds a replacement node first, then re-issues the drain. The audit log records both the refusal and the eventual successful drain (I-N6). |
| **Severity** | **P3** (operator workflow — no data path impact) |

### F-O6: Merge / split race on the same shard (ADR-033, ADR-034, spec-only)

| Field | Value |
|---|---|
| **Description** | A shard becomes simultaneously eligible for both merge (utilization low for merge interval) and split (one of the I-L6 ceilings exceeds threshold), or two competing operations arrive at the orchestrator near the same time (e.g., split fires from a write spike while a merge is mid-execution). |
| **Blast radius** | One shard — risk of inconsistent shard state if both proceed. Mitigated by ordering rule below. |
| **Detection** | Orchestrator pre-check: any in-flight membership operation on the candidate shard. |
| **Degradation** | Ordering rule: an in-flight merge wins — concurrent split is rejected with `shard busy: merge in progress`. An in-flight split wins — concurrent merge is rejected with `shard busy: split in progress`. The losing operation may be re-evaluated against the resulting shard topology after the in-flight operation completes. |
| **Recovery** | Automatic — orchestrator re-evaluates after the winning operation completes. Periodic scan re-checks merge eligibility on stable shards. |
| **Severity** | **P3** (transient; correctness preserved by ordering rule) |

### F-O7: Node added during in-flight namespace operations (ADR-033, spec-only)

| Field | Value |
|---|---|
| **Description** | Cluster admin adds a new node while a namespace has an in-flight split (from a ceiling trigger) or while a ratio-floor split is pending evaluation. The freshly added node could have been used as a placement target for the in-flight split's voter set. |
| **Blast radius** | Placement quality of in-flight operations — newly added node is initially under-utilized. No correctness impact. |
| **Detection** | Control plane observes node-add event during ongoing membership changes. |
| **Degradation** | In-flight operations complete with their pre-add placement (best-effort round-robin used the node count at the time of decision). The ratio floor evaluator (I-L11) re-runs after the node-add and may trigger additional splits to redistribute load onto the new node. |
| **Recovery** | Automatic via I-L11 re-evaluation. Operator may also trigger explicit rebalance (out of scope for ADR-033). |
| **Severity** | **P3** (placement quality only) |

### F-O8: Control plane Raft quorum loss (ADV-033-4)

| Field | Value |
|---|---|
| **Description** | The control plane's Raft group (which stores namespace shard maps, node records, and drain progress) loses quorum. All topology mutations are blocked: no namespace creation, no shard split/merge map updates, no drain initiation/completion, no `GetNamespaceShardMap` responses. |
| **Blast radius** | Topology management only. Data-path reads and writes continue unaffected — data shards have their own Raft groups. Gateways and clients serve traffic using cached shard maps. |
| **Detection** | Control plane health check. `GetNamespaceShardMap` RPCs time out; drain progress stalls; `CreateNamespace` returns error. |
| **Degradation** | Data path unaffected. Topology mutations queued until quorum restored. Splits that fire during the outage cannot update the shard map — new Raft groups are not committed. In-progress drains stall (no voter replacements committed). Namespace creations fail. |
| **Recovery** | Restore control plane Raft quorum. Queued mutations resume. No data loss. Production clusters should size the control plane Raft group at 5 voters (not 3) to survive 2 simultaneous failures. |
| **Severity** | **P2** (topology management blocked; data path unaffected) |

### F-O9: Merge convergence timeout (ADV-034-2)

| Field | Value |
|---|---|
| **Description** | During shard merge, the tail-chase (Phase 2 step 7) cannot converge because write rate to input shards exceeds the copy rate to the merged shard. Or the cutover pause exceeds the 50ms budget (> 200 remaining deltas). |
| **Blast radius** | One namespace — the merge is aborted. No data loss. Input shards continue serving normally. |
| **Detection** | Merge orchestrator tracks tail-chase progress; timeout (60s) or cutover budget (50ms / 200 deltas) exceeded. |
| **Degradation** | Merge aborted. Input shards restored to state=Healthy. `MergeAborted` event emitted. The merge candidate scanner re-evaluates on its next scan (5 minutes). If write rate has subsided, merge may be re-attempted. |
| **Recovery** | Automatic re-evaluation. If workload remains too hot for merge, the shards stay split (correct behavior — they're not actually under-utilized). |
| **Severity** | **P3** (operational; no data impact) |

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

## Client-side cache failures (ADR-031)

### F-CC1: Client crash leaves plaintext on NVMe

| Field | Value |
|---|---|
| **Description** | Client process crash (SIGKILL, OOM kill, kernel panic) skips the clean exit wipe path. Decrypted plaintext chunk files remain on local NVMe in the L2 cache pool directory. |
| **Blast radius** | Single compute node, single tenant's cached data. Plaintext is file-permission protected (0600) but not zeroized. |
| **Detection** | Next kiseki process start: orphan scavenger detects pool with no live flock holder. `kiseki-cache-scrub` service detects orphaned pools on node boot and every 60s. |
| **Degradation** | Plaintext persists until scavenger or scrub service runs. No data-path impact — canonical data unaffected. |
| **Recovery** | Scavenger or scrub service wipes orphaned pool (zeroize + delete). For stronger guarantees, use OPAL/SED NVMe with per-boot key rotation. |
| **Severity** | **P3** |

### F-CC2: L2 NVMe corruption serves bad data

| Field | Value |
|---|---|
| **Description** | Bit-flip or filesystem error corrupts an L2 chunk file. Client serves corrupted plaintext. |
| **Blast radius** | Single client process, single chunk read. |
| **Detection** | CRC32 trailer verification on every L2 read (I-CC13). |
| **Degradation** | CRC mismatch → bypass to canonical, delete corrupt L2 entry (I-CC7). Transparent to caller. |
| **Recovery** | Automatic: chunk re-fetched from canonical on next access. |
| **Severity** | **P3** |

### F-CC3: Crypto-shred with cached plaintext

| Field | Value |
|---|---|
| **Description** | Tenant admin destroys KEK (crypto-shred) while clients have decrypted plaintext in cache. Cached data must be wiped but detection is not instantaneous. |
| **Blast radius** | All client processes for the shredded tenant on all compute nodes. |
| **Detection** | Periodic key health check (default 30s), advisory channel notification, or KMS error on next operation. Unreachability falls through to disconnect timer wipe (I-CC6). |
| **Degradation** | Cached plaintext may be served for up to `min(key_health_interval, max_disconnect_seconds)` after the shred event (default 30s). After detection: immediate wipe with zeroize. |
| **Recovery** | Automatic: cache wiped on detection. No recovery action needed — data is intentionally destroyed. |
| **Severity** | **P1** (tenant-wide, time-bounded: max 30s default exposure window) |

### F-CC4: Staging exhausts compute-node NVMe

| Field | Value |
|---|---|
| **Description** | Multiple concurrent staging requests from Slurm prolog fill the compute-node NVMe hosting `$KISEKI_CACHE_DIR`, impacting other applications using local scratch. |
| **Blast radius** | Single compute node, all processes using local NVMe (not just Kiseki). |
| **Detection** | Per-node capacity check (`max_node_cache_bytes`, default 80% of filesystem). Disk-pressure backstop at 90% utilization. |
| **Degradation** | Staging rejected with `CacheCapacityExceeded` when node limit reached. Existing cached data remains servable. Other applications' NVMe use is protected by the 80%/90% limits. |
| **Recovery** | Release staged datasets (`kiseki-client stage --release`). Reduce `max_cache_bytes` per process or `max_node_cache_bytes` per node via policy. |
| **Severity** | **P3** |

---

## Failure severity summary

| Severity | Count | Examples |
|---|---|---|
| P0 | 2 | System key manager loss, system KEK compromise |
| P1 | 7 | Tenant KMS loss, log corruption, key compromise, algo deprecation, control plane down, network partition (wide), crypto-shred with cached plaintext (F-CC3) |
| P2 | 10 | Shard quorum loss, compaction storm, stale view, federation peer down, chunk loss, crypto-shred window, network partition (narrow), advisory outage, advisory audit storm, control plane quorum loss (F-O8) |
| P3 | 15 | Gateway crash, client crash, device failure, split latency, replay attack, bitmap corruption, extent leak, cache crash plaintext (F-CC1), L2 NVMe corruption (F-CC2), staging NVMe exhaustion (F-CC4), drain interrupted (F-O4), drain refused (F-O5), merge/split race (F-O6), node-add mid-operation (F-O7), merge convergence timeout (F-O9) |

Total: **34 failure modes** catalogued with blast radius, detection,
degradation, and recovery.
