# Adversarial Findings — Analyst Completeness Pass

**Status**: Complete.
**Last updated**: 2026-04-17, Session 2.

17 findings from adversarial pass on spec completeness. 3 closed during
analyst interrogation. 14 escalated to architect with context.

---

## Closed during analyst phase

### A-ADV-1: Authentication mechanism (CLOSED)

**Finding**: Authentication was completely unspecified. Without it, tenant
isolation (I-T1) is unenforceable.

**Resolution**: Two-tier authentication model confirmed:
- System level: mTLS with per-tenant certificates signed by Cluster CA.
  Works on SAN without control plane access. SPIFFE/SPIRE as alternative.
- Tenant level (optional): second-stage auth via tenant's own IdP/key
  manager for workload identity.
- Cluster admin: IAM via Control Plane on management/control network
  (separate from data fabric).

Invariants: I-Auth1 through I-Auth4. Terms added to ubiquitous language.

### A-ADV-3: Time handling (CLOSED)

**Finding**: Staleness bounds depend on time. No time model specified.
Clock skew makes 2s HIPAA floor meaningless without a shared time model.

**Resolution**: Dual clock model adapted from taba:
- HLC (Hybrid Logical Clock) for ordering and causality
- Wall clock for duration-based policies (retention, staleness, audit)
- Clock quality self-reporting per node (Ntp/Ptp/Gps/Unsync)
- Drift detection via HLC/wall-clock pairs
- Intra-shard: Raft sequence numbers. Cross-shard: HLC. Cross-site: HLC
  with async sync.

Invariants: I-T5 through I-T7. Types defined in ubiquitous language.

### A-ADV-6: Compression and crypto side channels (CLOSED)

**Finding**: Compression-before-encryption opens CRIME/BREACH-style side
channels. Never discussed.

**Resolution**: Tenant-configurable compression:
- Default off (safest)
- Tenant opt-in: compress-then-encrypt with fixed-size padding
- Compliance tags may prohibit compression
- Residual risk accepted by tenant on opt-in

Invariant: I-K14.

---

## Escalated to architect

### A-ADV-2: Upgrade and schema evolution

**Finding**: Zero coverage of how a running production cluster upgrades.
Delta envelope format, wire protocol versions, view materialization
format, chunk envelope structure — all may change across versions.

**Architect must specify**:
- Delta envelope versioning (version field in header?)
- Wire protocol version negotiation (native client ↔ storage)
- Rolling upgrade strategy (mixed-version cluster support window)
- Stream processor backward compatibility with old-format deltas
- Chunk envelope evolution path

**Risk if unaddressed**: First upgrade is a flag day (cluster-wide
downtime). Unacceptable for production-grade.

### A-ADV-4: POSIX semantics depth

**Finding**: Which POSIX operations does the FUSE mount support?

**Architect must specify**:
- `mmap()` — supported? (hardest operation for distributed FS)
- Hard links — within namespace only? (across namespaces = EXDEV)
- Extended attributes — full POSIX xattr semantics?
- ACLs — POSIX ACLs or Unix permissions only?
- Sparse files — how does a composition represent holes?
- `O_DIRECT` — bypass client cache?
- `flock` / `fcntl` locks — semantics via FUSE?

**Risk if unaddressed**: Users discover unsupported operations in
production. Needs explicit compatibility matrix.

### A-ADV-5: S3 API compatibility scope

**Finding**: Which S3 operations are supported?

**Architect must specify**:
- Versioning APIs (confirmed in scope — but which endpoints?)
- Lifecycle policies (transition, expiration)
- Event notifications
- SSE-S3/SSE-KMS/SSE-C mapping to Kiseki's encryption model
- Presigned URLs
- Bucket policies, CORS
- Multipart upload details (abort, part listing — partially covered)

**Risk if unaddressed**: Clients expect S3 compatibility and hit
unsupported operations.

### A-ADV-7: Observability contract

**Finding**: Operational observability (metrics, traces, structured logs)
is unmentioned. Audit logs cover security events but not operational
health.

**Architect must specify**:
- Metrics per context (latency, throughput, error rates, queue depths)
- Distributed tracing across contexts (write spans 5 contexts)
- Log format (structured, JSON?)
- Metrics export (Prometheus, OpenTelemetry?)
- Observability vs. zero-trust: cluster admin sees system metrics but
  per-tenant latency could leak access patterns

**Risk if unaddressed**: Production cluster is opaque to operators.

### A-ADV-8: Backup and disaster recovery

**Finding**: Raft replication and EC provide durability, but external
backup and site-level DR are unspecified.

**Architect must specify**:
- External backup mechanism (snapshot cluster state to external system?)
- Site destruction recovery path (federation provides async replica —
  is that the backup strategy?)
- What is backed up: log state, chunk data, control plane config, tenant config?
- Backup encryption (HIPAA requires encrypted backups)
- Recovery time objectives (RTO) and recovery point objectives (RPO)

**Risk if unaddressed**: Site loss with no recovery path.

### B-ADV-1: Audit log scalability

**Finding**: Audit log is a GC consumer (I-L4). If the audit log stalls
or has capacity issues, it blocks delta GC cluster-wide. Single point
of livelock.

**Architect must specify**:
- Audit log's own archival/GC strategy
- Maximum audit log backlog before alerting
- Whether the audit log can be sharded (per-tenant?)
- What happens if audit export to tenant VLAN stalls

**Risk if unaddressed**: Audit log growth blocks GC → storage exhaustion.

### B-ADV-2: Cross-tenant dedup refcount metadata access control

**Finding**: The metadata "chunk X is referenced by tenant A and tenant B"
exists somewhere. A malicious cluster admin who can query this confirms
cross-tenant data co-occurrence.

**Architect must specify**:
- Where refcount-per-tenant metadata is stored
- Access controls on this metadata (same as tenant data?)
- Whether refcount queries are available to cluster admin
- Whether dedup metadata is aggregated (total refcount only, no
  tenant attribution)

**Risk if unaddressed**: Co-occurrence leak via refcount metadata,
despite chunk-level dedup being architecturally sound.

### B-ADV-3: System DEK count at scale

**Finding**: Per-chunk system DEKs at petabyte scale = billions of keys.
System key manager must store all of them.

**Architect must specify**:
- System DEK granularity (per-chunk vs. per-group/per-shard)
- System key manager storage capacity planning
- Key derivation as alternative (derive per-chunk DEK from master +
  chunk_id — avoids storing individual DEKs)

**Risk if unaddressed**: System key manager becomes a scale bottleneck.

### B-ADV-4: Retention hold ordering enforcement

**Finding**: "Set hold before crypto-shred" is stated (I-C2b) but
enforcement mechanism unspecified. If tenant admin crypto-shreds
before hold is set, data may be GC'd despite regulatory requirement.

**Architect must specify**:
- Is there a mandatory compliance review gate before crypto-shred?
- Can compliance tags automatically create retention holds?
- Is crypto-shred blocked if a namespace has compliance tags that
  imply retention requirements?
- Or is this tenant's responsibility (liability risk)?

**Risk if unaddressed**: Regulatory violation if hold/shred ordering
is not enforced.

### B-ADV-5: Crypto-shred propagation — maximum acceptable cache TTL

**Finding**: After crypto-shred, cached tenant KEKs in gateways/clients
survive until TTL expires. During that window, data is technically
readable. Explicit invalidation may not reach temporarily unreachable
clients.

**Architect must specify**:
- Maximum cache TTL (this is a compliance contract)
- Invalidation broadcast mechanism (best-effort or guaranteed?)
- Whether crypto-shred waits for invalidation confirmation
- Acceptable window for GDPR/HIPAA (probably seconds, not minutes)

**Risk if unaddressed**: "Deleted" data readable for an undefined window.

### B-ADV-6: Stream processor isolation

**Finding**: Stream processors cache tenant KEKs and run on storage
infrastructure. Two tenants' stream processors on the same node:
container escape exposes one tenant's keys to another.

**Architect must specify**:
- Stream processor isolation mechanism (containers, VMs, processes?)
- Whether stream processors for different tenants can share a node
- Key material protection in memory (mlock, guard pages?)
- Whether hardware isolation (SEV-SNP, TDX) is in scope

**Risk if unaddressed**: Tenant key exposure via co-located stream
processor compromise.

### C-ADV-1: EXDEV cross-shard rename — application compatibility

**Finding**: EXDEV is standard but HPC frameworks (MPI-IO, some job
schedulers) may not handle it gracefully.

**Architect must specify**:
- Document EXDEV behavior in deployment guide
- Test with target HPC workload frameworks
- Consider whether the native client can transparently handle
  cross-shard rename as copy+delete (hiding EXDEV from the workload)

**Risk**: Low — most applications handle EXDEV. But needs testing.

### C-ADV-2: Federated KMS latency

**Finding**: Cross-border KMS access (Switzerland → Frankfurt) adds
10-20ms per cold key fetch. Cache TTL expiry during peak load causes
latency spike.

**Architect must specify**:
- KMS connection pooling and warm-up strategy
- Cache pre-refresh (refresh before TTL expires, not after)
- Cold-start latency budget for new stream processors at remote sites
- Whether tenant can deploy KMS replicas at each site (weakens
  single-source-of-truth but reduces latency)

**Risk**: Performance degradation at remote sites on cache miss.

### C-ADV-3: Content-defined chunking vs. RDMA alignment

**Finding**: Variable-size chunks from Rabin fingerprinting may not
align with RDMA transfer boundaries. One-sided RDMA works best with
known, aligned sizes.

**Architect must specify**:
- Storage layout alignment (pad chunks to alignment boundary on disk?)
- RDMA transfer mapping (scatter-gather for variable chunks?)
- Whether RDMA path uses a different storage layout than TCP path

**Risk**: RDMA performance degradation from misaligned transfers.

---

## Summary

| Status | Count | IDs |
|---|---|---|
| Closed by analyst | 3 | A-ADV-1, A-ADV-3, A-ADV-6 |
| Escalated to architect | 14 | A-ADV-2,4,5,7,8 B-ADV-1-6 C-ADV-1-3 |
| **Total** | **17** | |

The three closed findings added 7 new invariants and 8 new ubiquitous
language terms. The 14 escalated findings are documented with enough
context for the architect to make decisions without re-interrogating.
