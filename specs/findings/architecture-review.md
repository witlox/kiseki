# Adversary Review: Architecture Phase

**Date**: 2026-04-17
**Scope**: Architecture mode — specs/architecture/ only
**Stance**: Skeptical. Everything guilty until verified.

---

## Finding: ADV-ARCH-01 — HKDF key derivation leaks chunk identity to key manager
Severity: **Medium**
Category: Security > Key management
Location: specs/architecture/adr/003-system-dek-derivation.md
Spec reference: I-K4 (system enforces without reading plaintext)

**Description**: ADR-003 derives system DEK as `HKDF(master_key, chunk_id, epoch)`. The key manager receives the chunk_id on every derive request. Since chunk_id = sha256(plaintext), the key manager now has a record of every chunk_id it has derived a key for. This is the same co-occurrence data we tried to protect in ADR-017.

**Evidence**: The key manager can build a complete index of all chunk_ids it has ever served. Combined with timing (which tenant requested which chunk_id), this reconstructs the per-tenant refcount data we explicitly decided NOT to store in chunk metadata.

**Suggested resolution**: Two options:
1. **Cache-and-derive locally**: the kiseki-server process caches the system master key (fetched from keyserver at startup, refreshed on rotation) and derives DEKs locally. The keyserver never sees individual chunk_ids. This is the cleaner approach — the master key is already in memory on the server for HKDF; no per-chunk RPC needed.
2. **Batch derive**: derive DEKs in batches without per-chunk logging. Less clean but reduces RPC count.

Option 1 is strongly recommended. The keyserver's role becomes: store master keys, serve them to authorized server processes, manage epochs. It never sees chunk-level operations.

---

## Finding: ADV-ARCH-02 — No protobuf definitions provided
Severity: **Low**
Category: Correctness > Completeness
Location: specs/architecture/proto/ (empty)
Spec reference: module-graph.md references proto/kiseki/v1/*.proto

**Description**: The module graph promises 8 protobuf files in proto/kiseki/v1/. None were written. API contracts reference gRPC services but no actual .proto definitions exist. The Go↔Rust boundary is specified in prose but not in the contract language (protobuf) that actually enforces it.

**Suggested resolution**: Write the protobuf files before implementation begins. At minimum: common.proto, control.proto, key.proto, audit.proto (the gRPC services). Intra-Rust interfaces don't need protobuf.

---

## Finding: ADV-ARCH-03 — kiseki-server is a monolith composing 8 crates
Severity: **Medium**
Category: Robustness > Resource exhaustion
Location: specs/architecture/module-graph.md, build-phases.md#Phase12
Spec reference: F-D1 (gateway crash), ADR-012 (stream processor isolation)

**Description**: kiseki-server composes log + chunk + composition + view + gateway-nfs + gateway-s3 + audit into one binary, with per-tenant stream processors as child processes (ADR-012). This means:
- A bug in the NFS gateway can crash the entire server process
- Memory leak in compaction affects chunk reads
- The blast radius of a server crash is everything on that node

ADR-012 addresses stream processor isolation (separate processes per tenant). But the core server (log, chunk, composition, gateways) is a single process.

**Evidence**: F-D1 says "gateway crash disconnects one tenant's clients." But if the gateway is in-process with everything else, a gateway crash = server crash = ALL tenants on that node disconnected.

**Suggested resolution**: Consider process-level separation for gateways:
- Core server: log + chunk + composition (shared infrastructure)
- Gateway processes: per-protocol or per-tenant (separate process, can crash independently)
- This aligns with the stream processor isolation model (ADR-012)

Alternative: accept the monolith for simplicity but document that gateway crashes are server-wide (update failure-modes.md accordingly).

---

## Finding: ADV-ARCH-04 — Master key in kiseki-server memory is the highest-value target
Severity: **High**
Category: Security > Key management
Location: ADV-ARCH-01 resolution (cache master key locally)
Spec reference: I-K8 (keys never logged/printed), threat model (malicious insider)

**Description**: If ADV-ARCH-01 is resolved by caching the system master key in kiseki-server, then every storage node holds the master key in memory. A malicious insider with root on a storage node can extract the master key (ptrace, /proc/pid/mem, core dump). Combined with tenant KEKs cached in stream processors (same node), this gives full access to all data on that node.

**Evidence**: The threat model is "malicious insider." A root-level attacker on a storage node is within that threat model.

**Mitigations** (not complete solutions — this is inherent to the architecture):
- mlock + MADV_DONTDUMP on key material pages
- seccomp on server process (restrict ptrace)
- Core dumps disabled for kiseki processes
- Short-lived master key cache with re-fetch on rotation
- Future: TEE/SGX/SEV for key material (noted in ADR-012 as future work)

**Assessment**: This is an accepted risk inherent to any software encryption system. Hardware key stores (HSM/TPM) can reduce it but add latency. Document as an accepted risk with mitigations.

---

## Finding: ADV-ARCH-05 — Composition depends on both Log and Chunk — ordering hazard
Severity: **Medium**
Category: Correctness > Implicit coupling
Location: specs/architecture/data-models/composition.rs, api-contracts.md
Spec reference: I-L5 (chunks durable before visibility)

**Description**: The Composition context coordinates chunk writes and delta appends. The happy path is: write chunk → confirm durable → append delta. But:
- What if the server crashes between chunk-durable and delta-commit?
  → Orphan chunk (refcount 0, no composition references it). GC will eventually clean it.
  → The composition.feature covers this ("Delta commit fails after chunk write succeeds").
  → **OK, handled.**

- What if chunk write returns success but the data hasn't actually reached EC parity?
  → The chunk write is acknowledged by Chunk Storage — if Chunk Storage lies about durability, I-L5 is violated.
  → This is a trust boundary within the server. Chunk Storage must not ack until EC/replication is complete.

**Suggested resolution**: No architecture change needed. But the enforcement map should note that I-L5 depends on Chunk Storage not lying about durability. Add an integration test: kill a storage node mid-EC-write, verify chunk is not reported as durable.

---

## Finding: ADV-ARCH-06 — No data model for discovery protocol
Severity: **Low**
Category: Completeness
Location: specs/architecture/adr/008-native-client-discovery.md
Spec reference: I-O4

**Description**: ADR-008 specifies seed-based discovery but no data model exists for the discovery request/response messages. The client needs: shards, views, gateways, tenant auth requirements. None of these are in the data models.

**Suggested resolution**: Add `discovery.rs` to data-models/ with DiscoveryRequest, DiscoveryResponse types. Or defer to protobuf definition (the discovery service is gRPC-exposed).

---

## Finding: ADV-ARCH-07 — Audit log as GC consumer creates circular dependency risk
Severity: **Medium**
Category: Correctness > Implicit coupling
Location: specs/architecture/adr/009-audit-log-sharding.md, enforcement-map.md#I-L4
Spec reference: I-L4, I-A4

**Description**: The audit log uses "log shard machinery" (Phase 5 depends on Phase 3). Audit shards are Raft-replicated, same as data shards. But audit shards are also GC consumers for data shards. This creates:
- Data shard GC blocked until audit shard watermark advances
- Audit shard watermark advances when audit events are consumed and exported
- If audit export stalls → audit watermark stalls → data shard GC blocked

ADR-009 mitigates this by sharding audit per-tenant (one tenant's stalled export doesn't block others). But within a tenant, a stalled audit export blocks that tenant's data shard GC.

**Suggested resolution**: Add a safety valve: if audit watermark is stalled for > N hours, alert but allow GC to proceed (with an audit gap recorded). The alternative (data storage fills up because audit export is stalled) is worse than a gap in the audit trail. This should be a configurable policy.

---

## Finding: ADV-ARCH-08 — build-phases.md Phase 8 (View) depends on Phase 7 (Composition) — but View reads from Log, not Composition
Severity: **Low**
Category: Correctness > Dependency graph
Location: specs/architecture/build-phases.md#Phase8

**Description**: Phase 8 (View Materialization) lists dependency on Phase 7 (Composition). But the View context reads deltas from the Log (Phase 3) and chunks from Chunk Storage (Phase 6). It does not call CompositionOps. The dependency on Composition is indirect (views materialize state that compositions created), not a code dependency.

**Suggested resolution**: Remove Phase 7 as a hard dependency of Phase 8. View Materialization can be built and tested with synthetic deltas injected into the Log, without the Composition crate existing. This enables more parallelism.

---

## Summary

| # | Finding | Severity | Category | Blocks implementation? |
|---|---|---|---|---|
| ADV-ARCH-01 | HKDF leaks chunk_id to key manager | Medium | Security | Yes — resolve before Phase 4 |
| ADV-ARCH-02 | No protobuf definitions written | Low | Completeness | Yes — needed for Go↔Rust boundary |
| ADV-ARCH-03 | Monolith blast radius | Medium | Robustness | No — accept or redesign |
| ADV-ARCH-04 | Master key in server memory | High | Security | No — accepted risk with mitigations |
| ADV-ARCH-05 | Chunk durability trust boundary | Medium | Correctness | No — integration test needed |
| ADV-ARCH-06 | No discovery data model | Low | Completeness | No — defer to protobuf |
| ADV-ARCH-07 | Audit GC stall safety valve | Medium | Correctness | No — policy decision |
| ADV-ARCH-08 | View→Composition false dependency | Low | Dependency | No — parallelism improvement |

**Blocking**: ADV-ARCH-01 (must resolve HKDF → local derivation before key manager is built), ADV-ARCH-02 (protobuf definitions needed before Go work starts).

**Highest risk**: ADV-ARCH-04 (master key in memory — inherent to software encryption, mitigations listed, accepted risk).

**Recommendation**: Address ADV-ARCH-01 and ADV-ARCH-02 now. Accept ADV-ARCH-03 and ADV-ARCH-04 with documented mitigations. Add ADV-ARCH-07 safety valve as a policy decision. Fix ADV-ARCH-08 dependency. ADV-ARCH-05 and ADV-ARCH-06 are implementation-phase items.
