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

---

# Adversary Gate 1 — ADR-038 (pNFS layout + DS subprotocol)

**Date**: 2026-04-27
**Scope**: Architecture mode — ADR-038, `data-models/pnfs.rs`, invariants I-PN1..I-PN7, enforcement-map updates, build-phases Phase 15a/b/c
**Stance**: Skeptical. Everything guilty until verified against spec + existing code.

## Finding: ADV-038-1 — DS mTLS contradicts existing NFS server (no TLS today)
Severity: **Critical**
Category: Security > Trust boundaries
Location: ADR-038 §D4, I-PN7 (`specs/invariants.md`); contradicted by `crates/kiseki-gateway/src/nfs_server.rs:42-54`
Spec reference: I-PN7, ADR-009 (Cluster CA mTLS), ADR-038 §D4

**Description**: I-PN7 mandates `ds_addr` terminate "the same mTLS as the data-fabric (Cluster CA per ADR-009)". The supporting prose in ADR-038 §D4 references `TlsConfig::server_config`. But the existing NFS server (`run_nfs_server_with_peers` at `nfs_server.rs:42`) is **plaintext TCP** — `TcpListener::bind` with no TLS wrapper. There is no NFS-over-TLS code path in this codebase, and standard Linux pNFS clients do not commonly support NFS-over-TLS (RFC 9289 is recent; mainline kernel support is partial as of kernel 6.x and not the default).

So I-PN7 as written is unimplementable without one of:
- (a) Adding NFS-over-TLS to both MDS and DS listeners (large new surface, client compatibility risk)
- (b) Network-level isolation: VPC + tenant ACLs + firewall rules instead of mTLS
- (c) Front-proxy (stunnel-style) terminating TLS to a localhost plaintext NFS server

ADR-038 must commit to one of these. Without it, any leaked fh4 (5-min validity) is exploitable from the network because there's no transport-layer authentication of the calling process.

**Evidence**: `crates/kiseki-gateway/src/nfs_server.rs:42` binds plain `TcpListener`. No `tls_acceptor`/`TlsStream` in any NFS source file (verified via grep). `kiseki-transport/src/tcp_tls.rs` exists but is consumed by the gRPC data path, not the NFS path.

**Suggested resolution**: Architect must amend §D4. Most likely (b) — declare ADR-038 explicitly relies on tenant-scoped network isolation (and update I-PN7 wording to reflect this), with mTLS as a future-work item gated on Linux client adoption. If (a) is chosen, NFS-over-TLS must become a separate ADR with its own gate, *before* Phase 15a, since both MDS and DS depend on it.

---

## Finding: ADV-038-2 — fh4 MAC field encoding underspecified (canonicalization risk)
Severity: **High**
Category: Security > Cryptographic correctness
Location: ADR-038 §D4, I-PN1, `data-models/pnfs.rs:PnfsFileHandle`
Spec reference: I-PN1

**Description**: §D4 specifies the MAC input as `tenant_id ‖ namespace_id ‖ composition_id ‖ stripe_index ‖ expiry_hlc`. No field encoding is stated — neither widths, nor endianness, nor whether length-prefixes are used. As written, two different implementations of the MAC input could produce different byte sequences for the same logical inputs, breaking interop on key rotation or cross-version upgrade.

This *happens* to be unambiguous because (verified at `crates/kiseki-common/src/ids.rs:22,79,86`) `OrgId`, `NamespaceId`, and `CompositionId` are all `uuid::Uuid` (16 bytes fixed) — but the spec doesn't *say* this. A future ID change to variable-length string IDs would silently introduce canonicalization ambiguity (length-extension confusion: `(tenant=ab, ns=cdef, ...)` and `(tenant=abcd, ns=ef, ...)` could share a MAC input), allowing fh4 forgery across tenants without breaking the MAC.

§D4 also says `expiry_hlc` (an HLC, with `physical_ms` + `logical` + `node_id`); `data-models/pnfs.rs:PnfsFileHandle` says `expiry_ms: u64`. These don't match. If the MAC input is HLC and the validation reads expiry_ms, the MAC fails for legitimate fh4s.

**Evidence**:
- ADR-038 §D4: "tenant_id ‖ namespace_id ‖ composition_id ‖ stripe_index ‖ expiry_hlc"
- `data-models/pnfs.rs:31`: `pub expiry_ms: u64`
- `crates/kiseki-common/src/ids.rs:22,79,86`: ID types are uuid-backed today

**Suggested resolution**: Architect must add to §D4:

```
MAC input = tenant_id_bytes(16) || namespace_id_bytes(16) ||
            composition_id_bytes(16) || stripe_index_be(4) ||
            expiry_ms_be(8)
Total = 60 bytes, big-endian for integers, raw UUID bytes for IDs.
HMAC-SHA256, truncated to leftmost 16 bytes.
```

And reconcile `expiry_hlc` vs `expiry_ms` (recommend `expiry_ms` — HLC is for ordering not duration policy, see I-T5).

---

## Finding: ADV-038-3 — drain → LAYOUTRECALL hook does not exist as a subscribable channel
Severity: **High**
Category: Correctness > Implicit coupling
Location: I-PN5, ADR-038 §D6/D7, `crates/kiseki-control/src/node_lifecycle.rs:64-106`
Spec reference: I-PN5, ADR-035

**Description**: I-PN5 mandates "LAYOUTRECALL must fire within 1 sec on any of: ADR-035 node-state transition into Drain, ADR-033 split commit, ADR-034 merge commit". Phase 15c's exit criteria says "Subscribe to ADR-035 NodeStateChanged{Drain} events".

But ADR-035's drain emits `NodeAuditEvent::DrainRequested` *into the audit log* (`crates/kiseki-control/src/node_lifecycle.rs:64-106`) — not into a generic event channel. There is no `NodeStateChanged` watch/broadcast surface that the gateway crate can subscribe to today. Same for ADR-033/034 split/merge: those write into the namespace shard map but don't emit a fan-out event.

So I-PN5 is structurally undefined: the architect has handwaved a wiring point that doesn't exist. This makes I-PN5's 1-sec SLA unverifiable and Phase 15c's "subscribe" step a non-starter without separate plumbing work.

**Evidence**: `grep -rn "NodeStateChanged" crates/` returns zero hits. Drain code uses `NodeAuditEvent` only. ADR-033/034 commit hooks are not in the gateway crate's dependency closure (would create a cycle: gateway → control → log).

**Suggested resolution**: ADR-038 must add a §D10 ("Event channel introduction") specifying:
- A new pub-sub channel (e.g., `tokio::sync::broadcast::Sender<TopologyEvent>`) owned by the control plane
- Producer side: drain orchestrator + shard-split/merge committers post `TopologyEvent::{NodeDraining, ShardSplit, ShardMerge}` after committing to control-Raft
- Subscriber side: `kiseki-gateway` `LayoutManagerOps` impl subscribes at startup
- Failure semantics on subscriber lag (slow MDS misses an event): I-PN4 TTL is the safety net (5 min), already specified — explicitly state I-PN5 degrades to I-PN4 on subscriber lag

This is meaningful new wiring (~150 LoC + tests) and likely warrants a separate sub-phase, **15d**, before Phase 15c can complete.

---

## Finding: ADV-038-4 — Existing op dispatcher does not skip XDR for unsupported ops
Severity: **High**
Category: Robustness > Error handling quality
Location: `crates/kiseki-gateway/src/nfs4_server.rs:334-339`; inherited by ADR-038 DS dispatcher
Spec reference: ADR-038 §D2 (DS op subset), I-PN7

**Description**: The current MDS dispatcher's default arm returns NFS4ERR_NOTSUPP **without consuming the op's arguments from the XDR reader**:

```rust
_ => {
    let mut w = XdrWriter::new();
    w.write_u32(op_code);
    w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
    (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
}
```

This means in a multi-op COMPOUND containing an unsupported op followed by a supported op, the supported op parses garbage from the wrong byte offset — at best malformed-args errors, at worst silent corruption.

ADR-038's DS dispatcher (per I-PN7) restricts the allowed op set to 8 codes; everything else returns NFS4ERR_NOTSUPP. A malicious or buggy client sending a COMPOUND like `[PUTFH, ALLOCATE, READ]` (ALLOCATE is op 59, not in the DS subset) gets:
- PUTFH consumes its args ✓
- ALLOCATE returns NFS4ERR_NOTSUPP, args NOT consumed
- READ tries to parse from inside ALLOCATE's args bytes — undefined behavior

Per RFC 5661 §15.2, COMPOUND is supposed to abort on the first error. Looking at `dispatch_compound:276-279`, the loop does `break` on non-OK status, so the COMPOUND aborts and the bad parse never happens. That makes this a **dormant bug** in the current MDS — but ADR-038's DS only handles a subset, so its NFS4ERR_NOTSUPP rate is far higher, increasing exposure to clients that don't honor abort semantics. Also, the spec lists this break as compound-status, and ADR-038 doesn't acknowledge the assumption.

**Evidence**:
- `nfs4_server.rs:334-339` (no reader.skip)
- `nfs4_server.rs:276-279` (compound aborts on first error)
- I-PN7 expected behavior: NFS4ERR_NOTSUPP for any non-allowed op

**Suggested resolution**: ADR-038 must state explicitly: "DS dispatcher relies on COMPOUND abort-on-first-error semantics (RFC 5661 §15.2). No pre-error op parses past the first NFS4ERR_NOTSUPP." Implementer can cite this in step-defs. Optionally: add a defense-in-depth requirement that the DS terminates the connection (sends NFS4ERR_BADXDR + closes TCP) on any unsupported op, since the connection is forfeit anyway.

---

## Finding: ADV-038-5 — DS-side rate limiting absent (insider-tenant DoS amplification)
Severity: **Medium**
Category: Robustness > Resource exhaustion
Location: ADR-038 §D2, §D7 (failure semantics)
Spec reference: ADV-038 question Q3

**Description**: pNFS's design point is that clients bypass the MDS for data — including bypassing any MDS-side rate limiting. A legitimate tenant (or a credential thief inside the tenant) can issue arbitrary parallel reads at full DS bandwidth for up to `layout_ttl_seconds` (5 min) without MDS observation.

ADR-038 doesn't specify any DS-side rate limiting. Without it, a single tenant can saturate the DS port and starve other tenants whose layouts also point at the same DS. The MDS-side budgets that the workflow advisory enforces (ADR-021 §7) live in the MDS path — they are explicitly bypassed by pNFS.

This is a real concern: a legitimate Slurm job with a misconfigured worker count can flood the DS by accident. With 5 storage nodes serving 100 GPUs, a thundering-herd checkpoint read can take down a DS in seconds.

**Evidence**: ADR-038 §D7 lists failure modes but none address DS-side overload. `kiseki-advisory` lives in the MDS path only (verified by `grep advisory crates/kiseki-gateway/src/`).

**Suggested resolution**: Add §D11 ("DS rate limiting") specifying:
- Per-`(tenant_id, fh4)` token bucket on each DS, default `1 GiB/s` per fh4
- Per-tenant aggregate bucket on each DS
- 429-equivalent return (NFS4ERR_DELAY) on bucket exhaustion
- Buckets reset on layout TTL expiry (no need to persist)

Or — declare DS rate limiting out of scope for Phase 15 with a known-issue note, and gate production deployment on it.

---

## Finding: ADV-038-6 — Drained node serves I/O for up to 5 min after drain commit
Severity: **Medium**
Category: Correctness > Failure cascades
Location: I-PN4, I-PN5; intersects with ADR-035 invariants
Spec reference: I-PN4, I-PN5, I-N1..I-N6 (ADR-035)

**Description**: I-PN4 says layouts are valid for 5 min and "LAYOUTRECALL is best-effort acceleration". Combined with I-PN5's 1-sec recall SLA, the *intent* is: drained node stops serving in ≤1 sec.

But the I-PN4 fallback says: "Recall failure does not violate safety; TTL bounds staleness." This means: if a recall is missed (subscriber lag, broker bug, network blip), a drained node continues serving I/O for the remainder of the layout TTL — up to 5 min.

ADR-035's drain semantics (I-N3, I-N4) imply the drained node should stop accepting new writes immediately on entering Drain. Five minutes of write traffic continuing past drain-commit is operationally surprising to anyone who reads ADR-035 in isolation and is a real failure cascade for ops scenarios like "drain to kernel-patch reboot": you may be rebooting under live I/O.

This is not a *safety* violation (data still goes to the right shard via the gateway re-routing), but it's an operational invariant violation between two ADRs.

**Evidence**: ADR-038 I-PN4 ("Stale-routing risk after split/merge/drain is bounded by this TTL"); ADR-035 §3 (no exception for in-flight pNFS layouts).

**Suggested resolution**: Either:
- (a) Reduce default `layout_ttl_seconds` to 30s for the drain-bound case (acceptable if recall path is reliable; conservative if not); ADR-038 §D9 marks this as a tunable.
- (b) Add an explicit "drain hold" mode where ADR-035 drain orchestrator waits for `max(in-flight layout TTLs)` before declaring the node Evicted. Updates I-N6 (drain completion) and I-PN5.

(b) is cleaner; (a) is faster to ship.

---

## Finding: ADV-038-7 — Layout cache eviction policy unspecified (memory leak)
Severity: **Medium**
Category: Robustness > Resource exhaustion
Location: ADR-038 §D6, `data-models/pnfs.rs:ServerLayout`, I-PN4
Spec reference: I-PN4

**Description**: I-PN4 specifies a 5-min TTL but neither the ADR nor `LayoutManagerOps` specifies what evicts cache entries. The current `LayoutManager` (in `crates/kiseki-gateway/src/pnfs.rs`) holds a `HashMap<u64, Layout>` with no eviction at all — entries accumulate forever. ADR-038 doesn't fix this.

For a busy MDS issuing 1000 LAYOUTGETs/sec, with 5-min TTL, the cache holds 300,000 entries. At ~200 bytes each plus 1024 stripe segments × 64 bytes per fh4, this can balloon to ~6 GB per MDS over 5 min — and never frees memory if entries aren't actively evicted.

**Evidence**: `crates/kiseki-gateway/src/pnfs.rs:60-69` — HashMap with no eviction. `LayoutManagerOps::layout_return` exists but is only called by client LAYOUTRETURN, which clients aren't required to send.

**Suggested resolution**: Add an explicit eviction mechanism to `LayoutManagerOps`:
- Background sweeper task running every `layout_ttl_seconds / 4` (default 75s)
- Removes entries where `now_ms > issued_at_ms + ttl_ms`
- Optional cap on total entries (default 100k); LRU evict on insert when cap hit
- New invariant I-PN8: "Layout cache is bounded by N entries (default 100k) and a sweeper evicts expired entries every TTL/4 interval."

---

## Finding: ADV-038-8 — Composition deletion → LAYOUTRECALL pathway not specified
Severity: **Medium**
Category: Security > Tenant isolation
Location: ADR-038 §D6/D7; `data-models/pnfs.rs:RecallReason::CompositionDeleted`
Spec reference: I-PN5

**Description**: `RecallReason::CompositionDeleted` exists in the data model but no producer is specified. A composition deletion currently goes through `kiseki-composition::CompositionStore::delete` → emits a delta on the log → eventually the shard's view materializer notices. Nowhere does the gateway-resident `LayoutManagerOps` get notified.

If a composition is deleted while a layout is outstanding, the client can continue writing to a "dead" composition for up to 5 min via the DS path. Worse, if the namespace permits ID reuse (currently it does not — UUIDs — but if it ever did), this would be a cross-allocation data leak.

**Evidence**: `crates/kiseki-composition/src/composition.rs:DeleteResult` returns `Removed(chunks)` for refcount; no event emitted to gateway. `data-models/pnfs.rs:RecallReason::CompositionDeleted` has no documented producer.

**Suggested resolution**: ADR-038 must specify the producer: composition deletion path emits `TopologyEvent::CompositionDeleted{comp_id}` (same channel as ADV-038-3 fix). LayoutManagerOps subscribes. This is small if ADV-038-3 is resolved with a unified topology event bus.

---

## Finding: ADV-038-9 — fh4 forgery threat model assumes attacker cannot observe valid fh4s
Severity: **Low**
Category: Security > Cryptographic correctness
Location: ADR-038 §D4, ADV-038 question Q1
Spec reference: I-PN1

**Description**: HMAC-SHA256 truncated to 16 bytes is sufficient against blind forgery (128-bit security). But the threat model implicitly assumes an attacker cannot observe valid fh4s. With pNFS, fh4s travel in plaintext over the wire (at minimum NFS-on-TCP, see ADV-038-1). An on-path observer can capture a valid fh4 and replay it for the rest of its 5-min TTL.

This is RFC-standard pNFS behavior: fh4s ARE bearer tokens, not secrets. The mitigation is transport-layer auth (mTLS — see ADV-038-1) plus short TTLs. ADR-038 should state this threat model explicitly so implementers don't assume MAC-strength means replay-resistance.

**Evidence**: ADR-038 §D4 talks about MAC strength but not replay.

**Suggested resolution**: Add to §D4 threat model:
> "fh4 is a bearer token, not a secret. An attacker who observes a valid fh4 in transit can replay it until expiry. This is RFC-standard pNFS behavior. Mitigation: transport auth (mTLS, see ADV-038-1) and short TTLs (default 5 min, tunable down). The MAC prevents *forgery without observation*; it does not prevent replay."

---

## Finding: ADV-038-10 — Linux pNFS client conformance test is the only safety net; not in build phases
Severity: **Low**
Category: Correctness > Specification compliance
Location: ADR-038 §"Mitigated risks"; Phase 15c exit criteria
Spec reference: build-phases.md Phase 15c

**Description**: ADR-038 acknowledges Linux client conformance ("ADR-038 is considered unimplemented regardless of unit tests"). Phase 15c lists `tests/e2e/test_pnfs.py` as the gate. But Phase 15a's exit criteria mention only "hand-crafted fh4 with valid MAC" — meaning Phase 15a can pass while still being unmountable by a real client. Phase 15b's exit criteria mentions client mountstats but only after layout wire-up.

So the order is correct (15a→15b→15c), but a sub-step gate — "any real Linux client mounts and serves at least 1 byte through pNFS" — should appear at the end of 15b, not buried in 15c. Otherwise architects/implementers can declare 15b done with a happy-path unit test that doesn't catch RFC-fidelity bugs.

**Evidence**: build-phases.md Phase 15a/b/c exit criteria.

**Suggested resolution**: Tighten Phase 15b exit: "Linux 5.4+ pNFS client (mount.nfs4 with `minorversion=1`) successfully reads 1 MB through one DS, verified by `/proc/self/mountstats` showing non-zero per-DS READ counters. Failure in this gate blocks 15c." Move the multi-DS / multi-stripe / drain-recall tests to 15c.

---

## ADR-038 summary

| Finding | Title | Severity | Category | Blocking? |
|---|---|---|---|---|
| ADV-038-1 | DS mTLS contradicts plaintext NFS server | **Critical** | Security | **Yes — must resolve before 15a** |
| ADV-038-2 | fh4 MAC field encoding underspecified | **High** | Security | **Yes — must resolve before 15a** |
| ADV-038-3 | drain → recall hook is undefined channel | **High** | Correctness | **Yes — must add 15d before 15c** |
| ADV-038-4 | XDR-skip on unsupported op | High | Robustness | No — implementer can address |
| ADV-038-5 | DS rate limiting absent | Medium | Robustness | No — track as known issue |
| ADV-038-6 | 5-min stale serving on drain | Medium | Correctness | Recommend (b) before production |
| ADV-038-7 | Layout cache eviction unspecified | Medium | Robustness | Yes — small addition to ADR §D6 |
| ADV-038-8 | Composition deletion → recall path | Medium | Security | Folds into ADV-038-3 fix |
| ADV-038-9 | fh4 replay threat model implicit | Low | Security | No — doc-only |
| ADV-038-10 | Real-client gate moved earlier | Low | Correctness | No — small build-phases edit |

**Blocking gate-clear**: ADV-038-1, ADV-038-2, ADV-038-3, ADV-038-7. Architect must amend ADR-038 to address these four before implementer may proceed to Phase 15a.

**Non-blocking but should be tracked**: ADV-038-4, ADV-038-5, ADV-038-6, ADV-038-8, ADV-038-9, ADV-038-10.

**Highest risk**: ADV-038-1. The mTLS contradiction is fundamental — the design assumes a security control that does not exist in the codebase. Fix or downgrade I-PN7's claims.

**Recommendation**: **Gate 1 NOT cleared.** Send back to architect for rev 2 of ADR-038 addressing the four blocking findings. Estimate 2-4 hours of architect time. After rev 2, this gate can re-run (most findings should resolve cleanly).

---

## Gate 1 re-review (rev 2) — 2026-04-27

Architect produced rev 2 the same day. Re-checking each blocking finding:

**ADV-038-1 (DS mTLS contradiction) — RESOLVED.**
ADR-038 §D4.1 commits to NFS-over-TLS (RFC 9289) for both listeners
using the existing `TlsConfig::server_config` from
`crates/kiseki-transport/src/config.rs:94` (verified — returns
`rustls::ServerConfig`). §D4.2 introduces a *both-flags-required*
plaintext fallback (`[security].allow_plaintext_nfs=true` AND
`KISEKI_INSECURE_NFS=true`) with mandatory startup banner,
per-boot `SecurityDowngradeEnabled` audit event, auto-halved layout
TTL (300s → 60s), and refusal to start if any served namespace has
`tenant_count > 1`. I-PN7 rewritten in `specs/invariants.md` to
reflect this.

The dual-flag requirement is the right shape: env-var-only and
config-only enablement are both insufficient, so a leaked config or
a typo'd env doesn't accidentally downgrade. The audit event being
*per boot* (not once at first opt-in) ensures the downgrade
remains visible across restarts. Acceptable risk for the
single-tenant perf cluster; production multi-tenant is structurally
prevented.

**ADV-038-2 (fh4 MAC encoding) — RESOLVED.**
§D4.3 spells out the wire layout: 60-byte fixed payload with
declared field widths (16+16+16+4+8), big-endian for integers, raw
UUID bytes for IDs, 16-byte truncated HMAC-SHA256. MAC input is
domain-separated with `b"kiseki/pnfs-fh/v1\x00"` — prevents
cross-purpose `K_layout` use. `data-models/pnfs.rs:PnfsFileHandle`
docstring updated with the same layout; `PNFS_FH_BYTES = 76`
matches. `expiry_hlc` references reconciled to `expiry_ms`
throughout. The text now also constrains future ID-type changes
("if those types ever change to variable-length strings, this ADR
must be revised") — good defensive language.

**ADV-038-3 (drain → recall hook) — RESOLVED.**
§D10 introduces `TopologyEventBus` in `kiseki-control` (in the
gateway's existing dependency closure — no new cycle).
`tokio::sync::broadcast::Sender<TopologyEvent>` capacity 1024.
Producers emit *after* control-Raft commit (correct ordering —
aborted commits don't fire). Subscriber lag handled by full layout
cache flush + `pnfs_topology_event_lag_total` Prometheus counter.
Phase 15d added to `build-phases.md` with explicit exit criteria
(integration test verifying each producer fires exactly one event
per commit). I-PN9 added.

**ADV-038-7 (cache eviction) — RESOLVED.**
§D11 specifies both a capacity cap (default 100k entries, LRU-on-issuance
on overflow) and a background sweeper (default 75s = `ttl/4`).
I-PN8 added to `specs/invariants.md`. Memory bound at the cap is
made explicit (~6.4 GiB worst-case), with operator-tuning guidance
for large-file workloads.

### Cross-cutting check on rev 2

§D4.4 row 6 ("Compromised tenant credential floods DS") referenced
§D11 for rate limiting, but rate limiting moved to §D12 in rev 2;
§D11 became cache eviction. Spotted and fixed during re-review.
No other dangling refs.

### Residual concerns (non-blocking)

- **ADV-038-4 (XDR-skip)**: still inherited from current dispatcher.
  Implementer must add a Rust property test asserting that the DS
  COMPOUND loop terminates on first `NFS4ERR_NOTSUPP` (as the MDS
  loop does) and never tries to parse subsequent op args. Not a
  blocker — the abort-on-error behavior is already present in
  `nfs4_server.rs:276-279`; just needs explicit DS-side test
  coverage.
- **ADV-038-5 (DS rate limiting)**: §D12 declares this out of scope
  for Phase 15 with a structurally-enforced startup check (refuse
  `pnfs.enabled=true` ∧ `tenant_count>1` ∧ `ds_rate_limit_enabled=false`).
  Acceptable for the single-tenant perf cluster.
- **ADV-038-6 (5-min stale on drain)**: TTL auto-halves to 60s in
  plaintext fallback (covers the high-risk path). Default 300s
  remains for TLS path; recommend operators set `layout_ttl_seconds=60`
  if drain frequency is high. Not a hard blocker.
- **ADV-038-9 (replay threat model)**: now explicit in §D4.4 table.
  RESOLVED.
- **ADV-038-10 (real-client gate moved earlier)**: Phase 15a
  exit-criteria revision now requires a real Linux 6.7+ pNFS mount
  with `xprtsec=mtls` reading 1 MB through one DS, *and* a Rocky 9.5
  baseline plaintext-mode mount. RESOLVED.

### Summary table (rev 2)

| Finding | rev 1 verdict | rev 2 verdict |
|---|---|---|
| ADV-038-1 | Critical / blocking | **Resolved** |
| ADV-038-2 | High / blocking | **Resolved** |
| ADV-038-3 | High / blocking | **Resolved** (Phase 15d added) |
| ADV-038-4 | High / non-blocking | Implementer-tracked |
| ADV-038-5 | Medium / non-blocking | §D12 placeholder; structurally enforced |
| ADV-038-6 | Medium / non-blocking | Partially mitigated (TTL halving in plaintext) |
| ADV-038-7 | Medium / blocking | **Resolved** |
| ADV-038-8 | Medium / non-blocking | **Resolved** (folded into §D10) |
| ADV-038-9 | Low / non-blocking | **Resolved** |
| ADV-038-10 | Low / non-blocking | **Resolved** |

**Recommendation**: **ADV-038 cleared.** Implementer may proceed to
Phase 15a → 15b → 15d → 15c. Phase order matters: the previously
listed 15a→15b→15c is wrong post-rev-2 — 15d (TopologyEventBus)
must complete before 15c (recall integration). Build-phases doc
updated accordingly.

ADV-038-4, -5, -6 are tracked for the implementer/auditor steps and
do not block code starting now.

---

# Adversary Gate 1 — Protocol RFC compliance (originally ADR-039; folded into ADR-023 rev 2)

**Date**: 2026-04-27
**Scope**: Architecture mode — `specs/architecture/protocol-compliance.md` catalog + the test-discipline content originally drafted as ADR-039.
**Stance**: Skeptical. Catalog completeness + ordering checked against the actual code surface.

**Note**: This review's first finding (ADV-039-1) caught that ADR-023
already existed with overlapping scope. The architect responded by
folding ADR-039's content into ADR-023 rev 2 and deleting ADR-039.
Findings retain their original `ADV-039-N` IDs for traceability;
they apply to ADR-023 rev 2.

## Finding: ADV-039-1 — ADR-023 already exists; ADR-039 must cite it
Severity: **Critical**
Category: Correctness > Specification compliance
Location: `specs/architecture/adr/039-layer-1-rfc-compliance-discipline.md`; `specs/architecture/adr/023-protocol-rfc-compliance.md` (accepted 2026-04-20)

**Description**: ADR-023 ("Protocol RFC Compliance Scope") was
accepted on 2026-04-20 — about a week ago. It enumerates which
NFSv3/v4.2/S3 ops are implemented and explicitly defines a
"compliance testing approach" using BDD scenarios. ADR-039 was
written without referencing it. The two are complementary
(ADR-023 = scope, ADR-039 = test discipline) but ADR-039's
"Decision" section makes no acknowledgment, and the catalog
duplicates parts of ADR-023's tables without cross-referencing.

Worse: ADR-023 §"Compliance testing approach" §1 says "BDD feature
files map to RFC sections" as the compliance mechanism. ADR-039
is, in effect, replacing that mechanism — it explicitly tightens
the auditor's gate-2 to require Layer 1 reference decoders. ADR-039
must say so out loud, or future readers will think the two ADRs
contradict.

**Suggested resolution**: ADR-039 must add a "Relationship to
ADR-023" subsection that:
- Cites ADR-023 as the prior art that established protocol scope.
- Acknowledges ADR-039 SUPERSEDES ADR-023's "Compliance testing
  approach" section while preserving the implementation-scope tables.
- Marks ADR-023's status as "Superseded by ADR-039 on test
  discipline; scope tables remain authoritative" — or moves the
  implementation-scope tables into the catalog and marks ADR-023
  as fully superseded.

The catalog must add a "Prior art" cross-reference to ADR-023.

---

## Finding: ADV-039-2 — RFC 8881 supersedes RFC 5661 as canonical NFSv4.1
Severity: **High**
Category: Correctness > Specification currency
Location: `protocol-compliance.md` "RFC 5661" row; ADR-039 every reference to RFC 5661

**Description**: IETF published **RFC 8881** in August 2020 as
"Network File System (NFS) Version 4 Minor Version 1 Protocol".
It obsoletes RFC 5661. Every modern Linux kernel client
implementation references RFC 8881 (with backward-compatible RFC
5661 wire format). Kiseki's catalog cites RFC 5661 throughout —
which is technically obsolete. Tests written against "RFC 5661 §
18.35.4" should cite RFC 8881 §18.35.4 instead (same content,
authoritative spec).

The wire format is byte-identical between 5661 and 8881; the
errata in 8881 is mostly editorial. So this is not a code change,
but the doc references must be right or future readers will
chase a dead spec.

**Suggested resolution**: catalog row for NFSv4.1 cites "RFC 8881
(obsoletes RFC 5661)". ADR-039 references update similarly. Test
doc-comments cite 8881 with a note that 5661 is the predecessor.

---

## Finding: ADV-039-3 — RFC 5662 (NFSv4.1 XDR) folded into 5661 — no separate row
Severity: **Low**
Category: Correctness > Catalog completeness
Location: `protocol-compliance.md` (folded note in 5661 row)

**Description**: RFC 5662 is the companion XDR description for
NFSv4.1. The catalog folds it into the 5661 row with the note
"(folded into 5661 module)". That's reasonable — most
implementations treat them together — but the row's
**reference-decoder location** still has to import the XDR types
defined in 5662 (rpcgen-generated). If 5662 has its own errata
(it does — RFC 8434 errata for 5662), the catalog won't capture
that.

**Suggested resolution**: add a one-line "Companion specs:
RFC 5662 (XDR) + applicable errata" to the 5661/8881 row's notes.
Same treatment for RFC 7863 (XDR companion to RFC 7862).

---

## Finding: ADV-039-4 — RFC 7204 / RFC 5403 / RFC 2203 (RPCSEC_GSS) missing
Severity: **Medium**
Category: Correctness > Catalog completeness
Location: `protocol-compliance.md`

**Description**: Catalog covers AUTH at the TLS level (RFC 8446)
but doesn't catalog the AUTH flavors that ride inside ONC RPC.
`crates/kiseki-gateway/src/nfs_auth.rs` references AUTH_SYS,
AUTH_NONE, and "Kerberos principals" (RPCSEC_GSS). The catalog
should explicitly list:

- **RFC 1057** — ONC RPC v1 AUTH flavors (AUTH_NONE / AUTH_SYS).
  Implemented today.
- **RFC 2203 / RFC 5403 / RFC 7204** — RPCSEC_GSS (Kerberos for
  NFS). NOT implemented today, but referenced in `nfs_auth.rs`'s
  doc comment as "future". Catalog should list with status ❌ and
  critical-path N (until enterprise tenants need Kerberos).

Without these rows, a future reader looks at AUTH_SYS in the code,
asks "is this RFC-compliant?", and finds no row to consult.

**Suggested resolution**: add three rows under "Foundation":
RFC 1057, RFC 2203, RFC 5403, RFC 7204. Mark RPCSEC_GSS as ❌
not-implemented (ADR-009 covers what we DO use for auth).

---

## Finding: ADV-039-5 — Wire-sample fixture provenance is hand-wavy
Severity: **Medium**
Category: Robustness > Test maintainability
Location: ADR-039 §D3, "Cross-implementation seed"

**Description**: §D3 says "captured wire sample from a known-good
independent implementation … `.pcap` fixtures". Two unspecified
problems:

1. **Chicken-and-egg**: capturing a known-good NFSv4.1 mount
   trace requires a working mount — exactly what we couldn't do
   today. The fix landed via inspection + RFC reading + tcpdump
   of a *failed* attempt. Where does the first sample come from?

2. **Repo policy**: `.pcap` files are binary blobs that bloat git
   history. ~1 MB per sample × 18 specs × multiple per spec = 50–
   200 MB of binary in a repo that's currently tiny. Git LFS? A
   separate repo? Not addressed.

**Suggested resolution**: amend §D3 with:
- **Source priority**: (1) RFC examples (text — copy as bytes),
  (2) public test vectors (e.g. AWS SigV4 official test suite —
  text), (3) capture from a known-good independent implementation
  AFTER we have a baseline, (4) hand-crafted from spec for
  obscure paths.
- **Storage**: text fixtures in-repo; large binaries (`.pcap`) go
  under `tests/wire-samples/<rfc>/` with `.gitattributes` LFS
  pointer-only AND a recorded SHA in the test source so a missing
  LFS object fails loudly rather than silently skipping.

---

## Finding: ADV-039-6 — RFC 5663 (Block Layout) + RFC 8154 (SCSI Layout) missing as explicit-rejected
Severity: **Low**
Category: Correctness > Catalog completeness
Location: `protocol-compliance.md`; ADR-038 §D1 rejected these but the catalog doesn't show them at all

**Description**: ADR-038 §D1 explicitly rejects Block Layout
(RFC 5663) and SCSI Layout (RFC 8154) for our pNFS implementation.
The catalog should list them with status "Rejected — see ADR-038
§D1" so a future reader doesn't propose adding them or
mistakenly thinks "no row = not considered".

**Suggested resolution**: add two rows under "NFS data path":

| Spec | Status |
|---|---|
| RFC 5663 — pNFS Block Layout | Rejected (ADR-038 §D1) |
| RFC 8154 — pNFS SCSI Layout | Rejected (ADR-038 §D1) |

---

## Finding: ADV-039-7 — Internal cluster protocols (Raft messages, gRPC services) absent
Severity: **Medium**
Category: Correctness > Scope completeness
Location: `protocol-compliance.md`

**Description**: Catalog covers external client-facing protocols
but not the cluster-internal ones:

- **gRPC** (RFC-less, but a published spec) — every kiseki gRPC
  service (LogService, ControlService, KeyManagerService,
  WorkflowAdvisoryService, StorageAdminService — see ADR-021 §1
  and ADR-025) carries production traffic. Schema lives in
  `specs/architecture/proto/kiseki/v1/*.proto`. There's no
  "compliance" row.
- **openraft / Raft RPC** — kiseki-raft's TCP transport. Custom
  framing.
- **HKDF / HMAC / AES-GCM** — `kiseki-crypto` is FIPS-validated
  via aws-lc-rs but the catalog doesn't list crypto primitives.

These don't need RFC-compliance tests in the same Layer-1 sense
(no third-party clients consume them), but the catalog should
acknowledge them for completeness AND because cross-cutting bugs
(e.g. wrong Length-prefix in Raft RPC) have the same shape as the
NFSv4 wire bugs we just fixed.

**Suggested resolution**: add a separate top-level section
"Internal protocols" listing: gRPC schemas, Raft RPC, FIPS crypto
primitives. Mark each with appropriate scope (✅ for FIPS — already
verified by aws-lc-rs's certification; 🟡 for gRPC — protobuf
gives us schema validation but not semantic validation; ❌ for
Raft RPC framing).

---

## Finding: ADV-039-8 — POSIX scope — IEEE Std 1003.1-2024 supersedes 2017
Severity: **Low**
Category: Correctness > Specification currency
Location: `protocol-compliance.md` POSIX row; ADR-013 (POSIX semantics scope)

**Description**: IEEE published the **2024 revision of POSIX.1**
in mid-2024. The catalog cites POSIX-1.2017. Practically the
filesystem subset hasn't changed materially, but a reader looking
up "POSIX-1.2017" will find a superseded reference. Same fix as
RFC 8881 → 5661.

**Suggested resolution**: cite "POSIX.1-2024 (IEEE Std 1003.1-2024)
— filesystem subset" in the catalog row. Reference ADR-013 for
implementation scope.

---

## Finding: ADV-039-9 — Order: RFC 8881 cannot be done before RFC 4506 + RFC 5531
Severity: **Low**
Category: Correctness > Build-phase ordering
Location: ADR-039 §D4

**Description**: §D4 lists order as Foundation (4506+5531) →
Critical-path (5661/8881). Good. But the §D4 list buries the
ordering inside prose. A small visual ordering table would help
readers (and the implementer) not mis-read.

**Suggested resolution**: §D4 ends with a "Phase ordering
visual" — ASCII-tree like the build-phases doc has. Optional but
high-value cosmetic.

---

## Finding: ADV-039-10 — `@happy-path` BDD downgrade is a process change requiring tooling
Severity: **Medium**
Category: Robustness > Process enforceability
Location: ADR-039 §D5, §D7

**Description**: §D5 says "until [Layer 1 lands], scenarios are
tagged `@happy-path` and the BDD's RFC references are
documentation, not assertions." §D7 makes the auditor enforce
this. But:

1. There's no actual `@happy-path` tag in any feature file today.
2. The cucumber harness in `crates/kiseki-acceptance/tests/acceptance.rs`
   doesn't filter on `@happy-path`. If we add the tag, what does
   it mean operationally? Just a marker for the auditor?
3. Renaming every NFSv4 BDD scenario from `@integration` to
   `@happy-path` is itself a sweep that touches dozens of feature
   files and no compliance tests are written yet — chicken and
   egg with the catalog rollout.

**Suggested resolution**: amend §D5 with a transition plan:

- Phase A (this ADR): introduce the `@happy-path` tag *as a
  superset* of `@integration` with no semantic difference yet
  (cucumber treats them the same).
- Phase B (per-RFC): when an RFC's row goes ✅, the corresponding
  feature file is allowed to keep `@integration`. Until then, the
  tag stays both (so existing CI behavior unchanged).
- Phase C (catalog all ✅): drop the dual-tag scaffold. Auditor
  gate-2 enforces: every `@integration` scenario maps to a ✅ row.

This unblocks Layer-1 work without an organization-wide rename.

---

## ADR-039 summary

| Finding | Title | Severity | Blocking? |
|---|---|---|---|
| ADV-039-1 | ADR-023 not cited | **Critical** | **Yes — must fix before adversary clears** |
| ADV-039-2 | RFC 5661 → RFC 8881 | High | Yes — references must be current |
| ADV-039-3 | RFC 5662 / 7863 (XDR companions) | Low | No — clarification |
| ADV-039-4 | RPCSEC_GSS family missing | Medium | Yes — incomplete inventory |
| ADV-039-5 | Wire-sample provenance | Medium | Yes — without this, §D3 is unimplementable |
| ADV-039-6 | RFC 5663 / 8154 explicit-rejected | Low | No — completeness |
| ADV-039-7 | Internal protocols absent | Medium | Yes — catalog scope must include them |
| ADV-039-8 | POSIX-1.2024 supersedes 2017 | Low | No — currency |
| ADV-039-9 | §D4 order — visual | Low | No — cosmetic |
| ADV-039-10 | `@happy-path` transition plan | Medium | Yes — process unimplementable without it |

**Blocking gate-clear**: ADV-039-1, -2, -4, -5, -7, -10. Six
must-fix items before implementer may begin. Estimated 1-2 hours
of architect time to amend ADR-039 + the catalog.

**Recommendation**: **Gate 1 NOT cleared.** Send back to architect
for rev 2. Strong path forward; no fundamental redesign required.

---

## Gate 1 re-review (rev 2) — 2026-04-27

Architect produced rev 2 the same day, folding ADR-039 into
ADR-023 (now ADR-023 rev 2) per the user's decision. Re-checking
each blocking finding:

**ADV-039-1 (ADR-023 not cited) — RESOLVED.**
ADR-039 was deleted; its content is now §D2-D6 of ADR-023 rev 2.
Rev 2's revision-history block at the top documents the rev-1 →
rev-2 transition explicitly. The catalog's "Prior art" section
links to ADR-023 (and ADR-013/014). No supersedes-arrow needed.

**ADV-039-2 (RFC 5661 → RFC 8881) — RESOLVED.**
Catalog row for NFSv4.1 cites "RFC 8881 (Obsoletes RFC 5661)".
ADR-023 rev 2 references RFC 8881 throughout, with the rev-1 bug
descriptions explicitly using `RFC 7530/8881 §15.1` (NULL) and
`RFC 8881 §18.35.4` (EXCHANGE_ID flags). Companion XDR specs
RFC 5662 (NFSv4.1) and RFC 7863 (NFSv4.2) noted in the catalog
row. ADV-039-3 also resolved (RFC 5662/7863 noted).

**ADV-039-4 (RPCSEC_GSS family missing) — RESOLVED.**
Catalog "Foundation" section adds: RFC 1057 (AUTH_NONE/AUTH_SYS,
implemented today), RFC 2203 (RPCSEC_GSS, ❌ not implemented),
RFC 5403 (RPCSEC_GSS Version 2, ❌), RFC 7204 (folded into 2203/
5403). All marked critical-path N until enterprise tenants need
Kerberos. Sufficient for completeness.

**ADV-039-5 (wire-sample provenance) — RESOLVED.**
ADR-023 rev 2 §D2.3.1 and §D2.3.2 spell out:
- 4-tier source priority: (1) RFC text, (2) public test suites,
  (3) captured `.pcap`, (4) hand-crafted from spec.
- Storage policy: text fixtures in-repo under
  `tests/wire-samples/<rfc>/`, binary `.pcap` via Git LFS with
  embedded SHA-256 sentinels, 200 KiB threshold for LFS,
  reproduction script per capture.
The chicken-and-egg concern is addressed: most fixtures come from
RFC examples (text, no live mount needed); captures are tier-3
and used only after a baseline exists.

**ADV-039-6 (RFC 5663/8154 explicit-rejected) — RESOLVED.**
Catalog adds two ⛔ rows under "NFS data path": RFC 5663 (Block
Layout) and RFC 8154 (SCSI Layout), each with "Rejected (ADR-038
§D1)" pointer.

**ADV-039-7 (internal protocols absent) — RESOLVED.**
Catalog adds "Internal protocols" section: gRPC + Protobuf
(🟡 schema enforced via `tonic`/`prost`, semantic validation
unpinned), openraft Raft RPC (❌ custom framing), FIPS 140-2/3
crypto primitives (✅ aws-lc-rs upstream certified, 🟡 our usage
parameters need section tests). Critical-path Y for all three.

**ADV-039-8 (POSIX-1.2024 supersedes 2017) — RESOLVED.**
Catalog row for FUSE backend cites "POSIX.1-2024 (IEEE Std
1003.1-2024) — supersedes POSIX.1-2017".

**ADV-039-9 (visual ordering) — RESOLVED.**
ADR-023 rev 2 §D3 includes an ASCII-tree showing Phase A → B → C
→ D in sequence + E and F parallelizable + G as cleanup tail.

**ADV-039-10 (`@happy-path` transition plan) — RESOLVED.**
ADR-023 rev 2 §D4.1 specifies a three-phase rollout:
- Phase A (now): `@happy-path` introduced as a *superset* of
  `@integration` — cucumber treats them the same, no semantic
  change; new BDD scenarios use both side-by-side.
- Phase B (per-RFC): when an RFC ✅, the corresponding feature
  may keep `@integration` alone.
- Phase C (catalog all ✅): drop the dual-tag scaffold; auditor
  enforces the catalog mapping.

CI behavior is unchanged throughout. No organization-wide rename
required.

### Cross-cutting check

Catalog rev 2 contains every blocking-finding keyword (verified by
grep — 14 hits in catalog, 17 in ADR-023). Phase ordering visual
matches the catalog's structural sections. ADR-023 rev 2 cites the
two motivating commits (`5f6fece`, `7b1b4f6`) for traceability.

### Residual concerns (non-blocking, tracked in ADR-023 §"Open")

- **Versioned spec compliance** — RFC 8881 errata tracking
  policy. Default "8881 + applicable errata as of test write
  time" is fine for now; revisit when an errata changes a wire
  format.
- **Per-section coverage measurement** — no automated lint
  cross-references doc-comment `§ X.Y.Z` against the spec's TOC.
  Future work; not blocking Phase A.

### Summary table (rev 2)

| Finding | rev 1 verdict | rev 2 verdict |
|---|---|---|
| ADV-039-1 | Critical / blocking | **Resolved** (ADR-039 folded into ADR-023 rev 2) |
| ADV-039-2 | High / blocking | **Resolved** (RFC 5661 → RFC 8881) |
| ADV-039-3 | Low / non-blocking | **Resolved** (XDR companions noted) |
| ADV-039-4 | Medium / blocking | **Resolved** (RPCSEC_GSS family added) |
| ADV-039-5 | Medium / blocking | **Resolved** (provenance + LFS policy) |
| ADV-039-6 | Low / non-blocking | **Resolved** (rejected layouts as ⛔ rows) |
| ADV-039-7 | Medium / blocking | **Resolved** (internal protocols section) |
| ADV-039-8 | Low / non-blocking | **Resolved** (POSIX.1-2024) |
| ADV-039-9 | Low / non-blocking | **Resolved** (ASCII visual) |
| ADV-039-10 | Medium / blocking | **Resolved** (3-phase transition) |

**Recommendation**: **ADR-023 rev 2 cleared.** Implementer may
proceed to Phase A: RFC 4506 (XDR) + RFC 5531 (ONC RPC v2) +
RFC 1057 (AUTH flavors) reference decoders.

The Phase 15 e2e remains paused per the user's "pause e2e" call
2026-04-27. It resumes once the critical-path RFCs (8881, 7862,
8435) are at least 🟡 — at which point we'll have proper
diagnostics for whatever blocks the mount next.
