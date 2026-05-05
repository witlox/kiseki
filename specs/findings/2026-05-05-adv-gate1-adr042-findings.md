# Adversary Gate-1 Findings — ADR-042 Native Gateway Data Service

**Type**: Adversary → Architect (gate-1)
**Date**: 2026-05-05
**Reviewer**: adversary (architecture mode)
**Mode**: pre-implementation review against the architect's draft.
**Verdict**: **CHANGES REQUESTED** — 2 CRITICAL, 6 HIGH, 8 MEDIUM, 4 LOW. The two CRITICAL findings must be resolved before implementer touches code; HIGH findings should be addressed in the ADR amendment or the proto schema before the implementer commits to a wire-incompatible shape.
**Status (2026-05-05)**: All CRITICAL + HIGH + MEDIUM findings resolved in place per the user's "fix all" directive. ADR-042 + `gateway_data.proto` amended; new I-NG15..I-NG24, A-NG21..A-NG24, F-NG12..F-NG13 added. Resolution table at the bottom of this file.

The architect's pre-emptive list in §13 of ADR-042 covered some of these, but several are new — the architect named the *symptom* (e.g., "TrustedCompute mode flip") without specifying the *mechanism* (which key signs the ticket?). The adversary's job is to nail down the unspecified mechanisms.

## Summary

| Severity | Count |
|---|---|
| Critical | 2 |
| High     | 6 |
| Medium   | 8 |
| Low      | 4 |

ADR-042's overall shape is **structurally sound** — the analyst-validated decisions (object+POSIX dual surface, hybrid leader routing, Raft-replicated dedup, lease fencing, hybrid encryption boundary) compose into a coherent protocol. Gate-1 issues are concentrated in three areas:
1. **Cryptographic key derivation for handle tokens and DEK-fetch tickets** — vague in the ADR, security-critical.
2. **Multi-shard atomicity for PUTs whose chunks span multiple shards** — undefined.
3. **Authentication-token lifetime vs cert lifetime** — handle tokens can outlive the cert that minted them.

---

## CRITICAL findings (block implementer)

### F-C1: Handle token signing key — "system DEK" doesn't exist as a singleton

**Severity**: Critical
**Category**: Security > Cryptographic correctness, Semantic drift
**Location**: ADR-042 §9 ("HMAC-SHA256 under the system DEK"); §8 (DEK-fetch ticket "HMAC-signed under the system DEK"); §1 (DEK-fetch ticket comment)
**Spec reference**: ADR-002 (two-layer encryption model), ADR-003 (system DEK derivation), I-K3 (crypto-shred propagation), I-K8 (keys never persisted in config)

**Description**: ADR-042 says handle tokens (§9) and DEK-fetch tickets (§8) are HMAC-SHA256-signed "under the system DEK." But per ADR-003 there is **no single system DEK** — the system DEK is *derived per-chunk* via HKDF(master_key, salt=chunk_id). The system *master* key exists; the system *DEKs* are ephemeral, per-chunk values.

This is not a typo; it's a missing design decision. Three concrete questions the architect must answer:

1. **Which key signs the handle token?** Options:
   - (a) The system **master** key directly. Simple, but the master key never leaves the keymanager today (per ADR-007); having the gateway HMAC with the master key requires routing every Open through the keymanager (latency cost).
   - (b) A *gateway signing key* derived from the master key once at startup via `HKDF(master_key, salt="kiseki-gateway-signing-v1")`. Stays in gateway memory. Handle tokens become forgeable if the gateway process is compromised — but at that point the attacker already has access to the gateway's plaintext processing.
   - (c) A per-tenant *handle signing key* derived via `HKDF(master_key, salt="kiseki-handle-token-v1", info=tenant_id)`. Compromise of one tenant's signing key doesn't affect others. Adds keymanager round-trip on first handle issuance per tenant; cacheable thereafter.

2. **Same question for DEK-fetch tickets.** Same options apply.

3. **Key rotation**: when the system master key rotates (epoch advance), what happens to in-flight handles / tickets? The handle token's `issued_at` doesn't carry the master-key epoch.

**Suggested resolution**:
- Pick option (b) or (c). Document in ADR-042 §9 with explicit derivation: `HMAC-SHA256(handle_signing_key, postcard(HandleToken))` where `handle_signing_key = HKDF(master_key, salt="kiseki-handle-token-v1")`.
- Add the master-key-epoch to the token. Server validates against the current epoch ± N (where N is the keymanager's grace window for in-flight ops post-rotation). Mismatch returns `Unauthenticated{handle_token_epoch_stale}`.
- Add A-NG / I-NG for the discipline.

This is CRITICAL because shipping with "system DEK" semantics undefined risks an implementer picking a different interpretation than the architect intended, which becomes a security boundary moved silently.

### F-C2: Multi-chunk PUT atomicity across shards — undefined

**Severity**: Critical
**Category**: Correctness > Specification compliance, Failure cascades
**Location**: ADR-042 §1 (PutObjectStream), §5 (multipart), §12 (perf budget), I-NG2
**Spec reference**: I-L5 (composition-not-visible-until-chunks-durable), I-L8 (cross-shard EXDEV), ADR-026 (per-shard Raft), ADR-033 (initial shard topology)

**Description**: A PutObjectStream of a 50 MiB object (within the per-stream cap) splits into multiple chunks (`MAX_PLAINTEXT_PER_CHUNK` = 4 MiB by default → ~13 chunks). Different chunks hash to different `chunk_id`s and the cluster places them on different shards (rendezvous-hash placement, ADR-033). The gateway's existing write loop calls `chunks.write_chunk(env, "default")` per chunk, which is a single-chunk-store call but underneath it dispatches to whichever shard owns that chunk's placement.

ADR-042 §1 says CommitStream must not return Ok until "every chunk referenced by the composition is durable on min_acks peers per the pool's durability strategy (preserves I-L5)." But:

- **What if 12 of 13 chunks succeed and the 13th fails (e.g., the shard for the 13th chunk loses quorum)?** Spec doesn't specify. Today's S3 path silently leaves the 12 successful chunks orphaned (refcount 0, GC'd later). For native, ADR-042 promises "no partial visible" (I-NG2) but doesn't say *what happens to the half-written state*.

- **Are the 13 chunk writes parallelized or serialized?** Spec doesn't say. Parallel = lower latency but more concurrent shard contention; serial = higher latency, simpler error handling.

- **Multipart equivalent**: `CompleteMultipart` says "server validates etags + emits the composition delta atomically." But the parts themselves may have landed on different shards (the per-part chunks fan out by chunk_id). What's the atomicity unit?

This is CRITICAL because the I-NG2 commit-on-close contract is what HPC clients build correctness on. "Maybe partial chunks orphan" is an acceptable failure mode if SPECIFIED; "spec is silent" is the bug.

**Suggested resolution**:
- Add ADR-042 §1.X: "Multi-chunk PUT atomicity": CommitStream is a barrier that returns Ok ONLY if all `min_acks` succeed across all referenced chunks. On any chunk's failure, the stream is aborted; staged chunks are GC'd via the existing orphan-fragment scrub; client sees `Aborted{partial_chunk_failure, succeeded=N, total=M}`. Client retries with a fresh `idempotency_key` (or the same one — A-NG10 dedup will short-circuit subsequent retries to the original outcome).
- Specify whether chunks are written in parallel (yes — bounded concurrency, configurable knob `KISEKI_PUT_CHUNK_PARALLELISM`, default 4).
- Add F-NG (new entry) capturing the multi-chunk partial-failure mode.

---

## HIGH findings (block architect; resolve before implementer commits)

### F-H1: Cert revocation doesn't invalidate Open handle tokens

**Severity**: High
**Category**: Security > Trust boundaries, Authentication
**Location**: ADR-042 §3 (cert reval), §9 (handle token max lifetime 1 hour)
**Spec reference**: A-NG17 (cert revocation mid-session), F-NG8

**Description**: ADR-042 §9 says handle tokens have a `token_max_lifetime` of 1 hour. ADR-042 §3 says cert revocation tears down the gRPC stream with `Unauthenticated{cert_revoked}`. But:

- A revoked cert's holder still possesses any handle tokens issued before revocation.
- They could establish a new mTLS connection with a *different* (valid) cert and present the old token. The token's HMAC validates (not tied to the cert), the inode + open_mode + crypto_boundary all check out.
- Result: post-revocation access via a co-conspirator's cert presenting the revoked principal's handle token.

**Evidence**: §9's HandleToken struct has `namespace_id, inode, open_mode, crypto_boundary_at_open, issued_at, issuance_nonce` — no cert SAN binding. The token is portable across mTLS connections.

**Suggested resolution**:
- Bind the cert SAN (canonical form) into the HandleToken. Server validates token's SAN matches the current connection's SAN.
- Cert revocation that lands during an active stream torn the stream down (already specified). The handle token is implicitly invalidated because the binding requires a matching live cert.
- For the cross-connection-replay case: the token's SAN must match the current connection's SAN, regardless of revocation status. A revoked cert can't establish a connection at all (mTLS handshake fails per ADR-023), so the replay-via-different-cert is blocked at the connection layer.

### F-H2: TrustedCompute Read with N chunks → N FetchDek round-trips

**Severity**: High
**Category**: Robustness > Resource exhaustion (latency), Correctness > Failure cascades
**Location**: ADR-042 §1 (GetObjectChunk.SealedChunk), §8 (FetchDek)
**Spec reference**: I-NG6a, A-NG7

**Description**: A 64 MiB Read in TrustedCompute mode returns ~16 sealed chunks (4 MiB each by default). Each chunk has its own `dek_fetch_ticket`. The client makes 16 separate `FetchDek` round-trips to the keymanager (which is a different gRPC service, possibly on a different node). For a sequential 1 GiB Read, that's ~256 keymanager calls.

Per-call latency for the keymanager is dominated by Raft-replicated key state (ADR-007) + mTLS — typically 1-3 ms. 256 × 2 ms = 512 ms of pure keymanager latency on a 1 GiB Read. The whole point of TrustedCompute was to be FASTER than ServerOnly.

**Suggested resolution**:
- Add `BatchFetchDek(repeated dek_fetch_ticket) returns BatchFetchDekResponse` to the keymanager service. Single round-trip for all chunks of a Read.
- OR (cleaner): the gateway pre-bundles a "session DEK envelope" — a one-shot symmetric key that's used to encrypt all the DEKs for a single Read response. Client decrypts the envelope once, decrypts the chunks N times locally. One keymanager round-trip per Read instead of per chunk.
- Document the choice in §8 + add F-NG (new entry): "TrustedCompute multi-chunk Read latency cliff — bounded by [chosen mechanism]."

The DEK cache (out of scope for ADR-042) would also help here. Architect should note the tension: ADR-042 is the right ADR to specify the batch shape; the cache is the orthogonal optimization.

### F-H3: GetTopology unauthenticated cluster-structure leak

**Severity**: High
**Category**: Security > Tenant isolation, Trust boundaries
**Location**: ADR-042 §4 (GetTopology), §1 (proto schema)
**Spec reference**: I-T1..I-T7 (tenant isolation invariants)

**Description**: `GetTopology` returns `repeated NodeInfo nodes` and `repeated ShardLeadership shards`. The proto schema doesn't have a tenant scope on the request — any authenticated client can call it and learn the cluster's full topology.

This leaks:
- Number of nodes and their IP/port addresses
- Shard leadership distribution (which node is leader for which shard)
- Shard ranges (`range_start`, `range_end` — the hashed_key partitioning)

**Why it matters**: in a multi-tenant SaaS-style deployment, knowing another tenant's shard topology enables targeted DoS (attack the right node) and timing-side-channel attacks (correlate response latencies with shard leadership). HPC single-tenant clusters don't care; multi-tenant deployments do.

**Suggested resolution**:
- Scope `GetTopology` to the calling tenant's namespaces. The response `shards` list filters to only shards that own at least one of the caller's namespaces.
- Node addresses still leak (the caller has to dial them anyway), but the shard-leadership map for OTHER tenants is hidden.
- Document the trade-off in §4 + add A-NG entry.

### F-H4: Lease + idempotency_key dedup interaction undefined

**Severity**: High
**Category**: Correctness > Concurrency, Specification compliance
**Location**: ADR-042 §6 (idempotency dedup) + §7 (lease lifecycle)
**Spec reference**: I-NG5, I-NG12, A-NG10

**Description**: Lease-bound writes attach `fencing_token` (I-NG12). The server rejects with `LeaseFenced{current_token}` if the token is stale. But the dedup state (I-NG5) is keyed on `(tenant_id, namespace_id, idempotency_key)` — fencing_token is not part of the key.

**Concrete race**:
1. Client A holds lease, fencing_token=42. Issues Write with idempotency_key="abc". Server commits, dedup row recorded with response_summary="ok".
2. Client A's lease expires; Client B acquires, fencing_token=43.
3. Network partition heals. Client A retries Write with same idempotency_key="abc" + stale fencing_token=42.
4. Server's dedup table short-circuits: idempotency_key matches an earlier success, return "ok" without checking the fencing_token.
5. Client A believes the write succeeded; Client B believes it has exclusive access. **Both wrong.**

The fencing-token check (I-NG12) is supposed to prevent this, but it's bypassed by the dedup short-circuit.

**Suggested resolution**:
- Either:
  - (a) Include fencing_token in the dedup key: `(tenant_id, namespace_id, idempotency_key, fencing_token)`. Retries with stale fencing_tokens won't dedup-match; they fall through to the fencing check and fail correctly.
  - (b) Validate fencing_token BEFORE consulting the dedup table. If stale, return `LeaseFenced` regardless of dedup state.
- Spec must specify which. Recommend (b) — keeps dedup keying simple and the security check explicit.

### F-H5: Workflow_ref required-vs-default ambiguity opens silent attribution holes

**Severity**: High
**Category**: Correctness > Specification compliance, Audit gaps
**Location**: ADR-042 §1 (ControlFields), §10 (audit), A-NG6 (workflow_ref policy)
**Spec reference**: ADR-020 (workflow advisory), I-WA14 (workflow_ref policy)

**Description**: A-NG6 says workflow_ref defaults to `unattributed` "when absent and tenant policy permits." ADR-042 §10 says "workflow_ref defaults to the literal `unattributed` when absent." These are CLOSE but not identical:

- A-NG6 implies tenant policy can REJECT writes without workflow_ref (return error).
- §10 implies the server always accepts and substitutes `unattributed`.

The implementer's choice changes the security posture: tenant compliance auditors who require attribution rely on the rejection behavior. If the server silently substitutes `unattributed`, audit traces are useless.

**Suggested resolution**:
- Specify in ADR-042 §10: "When tenant policy `workflow_ref_required_for_writes = true` (per ADR-020) and the request omits workflow_ref, the server rejects with `InvalidArgument{workflow_ref_required}`. Otherwise the server substitutes `unattributed` and the audit event records that literal."
- Add a Gherkin scenario to `native-gateway.feature` covering both branches.

### F-H6: Per-tenant stream cap counter is a global hot path

**Severity**: High
**Category**: Robustness > Resource exhaustion (scalability), Correctness > Concurrency
**Location**: ADR-042 §12 ("Per-tenant concurrent-stream counter: parking_lot::Mutex<HashMap<TenantId, AtomicUsize>>")
**Spec reference**: I-NG11, A-NG14

**Description**: Architect specifies a `parking_lot::Mutex<HashMap<TenantId, ConcurrentStreams>>` for the per-tenant stream cap. Every `OpenStream` call (and every stream close) acquires this single mutex, serializing across ALL tenants.

In a multi-tenant cluster with thousands of tenants doing concurrent streaming writes, this becomes a per-stream-open serialization point that bottlenecks the entire data plane. The 2026-05-05 spike spent significant work moving away from exactly this kind of coarse mutex.

**Suggested resolution**:
- Use `dashmap::DashMap<TenantId, AtomicUsize>` — sharded by tenant. The increment is `entry().or_insert(AtomicUsize::new(0)).fetch_add(1, Relaxed)`; the cap-then-allocate sequence is atomic per tenant via the DashMap entry guard.
- Or simpler: `Arc<RwLock<HashMap<TenantId, Arc<AtomicUsize>>>>`. Tenants acquire the read lock to find their counter; writers (new tenant arrivals) take the write lock briefly. Tenant counter `fetch_add` is lock-free.
- Document the choice in §12.

---

## MEDIUM findings (architect should resolve)

### F-M1: Streaming idempotency_key only validated on first message

**Severity**: Medium
**Category**: Correctness > Concurrency, Security > Replay
**Location**: ADR-042 §1 (PutObjectChunk schema)
**Spec reference**: I-NG5, A-NG10

**Description**: `PutObjectChunk.first` carries the `ControlFields` (with `idempotency_key`). Subsequent `data` and `commit` messages don't repeat it. A malicious client could send `first` with key="A", then on subsequent frames inject a `first` with key="B" (proto allows oneof). Server's behavior on repeated first messages is unspecified.

**Suggested resolution**:
- Spec explicitly: "On repeated `first` messages, server returns `InvalidArgument{stream_already_initialized}`."
- Implementer enforces this at the proto-handler boundary.

### F-M2: AcquireLease/RenewLease audit storm

**Severity**: Medium
**Category**: Robustness > Observability gaps
**Location**: ADR-042 §10 (every gateway-dispatched op produces an audit event)
**Spec reference**: I-NG7, ADR-009

**Description**: With lease TTL 30 s and recommended renewal cadence 1/3 TTL (10 s), each active lease produces 360 RenewLease audit events per hour. A cluster with 10 000 active POSIX leases produces 3.6 M audit events per hour just from renewals. Audit pipeline backpressure / storage cost.

**Suggested resolution**:
- Audit `AcquireLease` (issuance) and `ReleaseLease` (termination) events; drop `RenewLease` events to a counter (`kiseki_native_lease_renewals_total{tenant}`) instead of per-event audit. The metric is enough for SLOs; the per-event audit adds nothing.
- Alternative: aggregate renewals into a "lease activity summary" event emitted periodically (every minute per lease).

### F-M3: Inode allocation/recycling discipline unspecified

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §1 (Open returns inode), §9 (HandleToken.inode)
**Spec reference**: ADR-013 (POSIX semantics), I-NG3

**Description**: `Open` returns an inode (or resolves an existing one). HandleToken commits to it. But:
- Who allocates inodes? (composition store? gateway? log state machine?)
- When an inode is unlinked + GC'd, can the same inode number be reused? Standard POSIX says yes (inodes are recyclable). But a stale handle token still validates against the new inode owner — security gap.

**Suggested resolution**:
- Specify: inodes are allocated by the composition store via `next_inode()` (monotonic 64-bit counter, persisted in redb meta). Once allocated, NEVER reused. (4 billion inodes/sec for ~146 years before u64 wraps — fine.)
- Stale handle tokens for unlinked inodes return `Unauthenticated{inode_orphaned}` on next op. Add audit + metric.

### F-M4: Multiple SAN URIs in cert — match policy

**Severity**: Medium
**Category**: Security > Authentication
**Location**: ADR-042 §3 (SanInterceptor)
**Spec reference**: I-NG1

**Description**: x.509 certs can have multiple SAN URI entries. ADR-042's interceptor description doesn't say which one is the "tenant identity." Implementer might pick the first; a malicious cert issuer could put `spiffe://kiseki/tenant/victim` first and `spiffe://kiseki/tenant/legitimate-but-different` second.

**Suggested resolution**:
- Specify: cert MUST have exactly one SAN URI matching the canonical kiseki tenant URI pattern (`spiffe://kiseki/tenant/<org_id>`). Multiple matching = reject. Zero matching = reject.

### F-M5: bytes::Bytes "zero-copy" overstates tonic's behavior

**Severity**: Medium
**Category**: Correctness > Specification compliance, Performance budget realism
**Location**: ADR-042 §12 (perf budget — "zero-copy boundary on Read")

**Description**: tonic decodes incoming gRPC frames into `Vec<u8>` (prost's default), then the user's response message is serialized via prost which copies again. True zero-copy from chunk-store → wire requires custom codec or `prost::bytes::Bytes`-aware messages.

**Suggested resolution**:
- Replace the §12 claim with the more honest "minimize allocations on Read" — use `bytes::Bytes` in the application-layer types (e.g., `GetObjectResponse.data` could be `Bytes` instead of `Vec<u8>` if prost-bytes feature is enabled), but accept that tonic+prost's framing layer copies once on encode. Aspirational zero-copy is for a future tonic codec audit.

### F-M6: Crypto_boundary mutation auth path

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §8 (Setattr changes mode), I-NG6c

**Description**: I-NG6c says setting `crypto_boundary = TrustedCompute` requires also setting `crypto_shred_policy = best_effort`. But who can set these? Tenant admin? Cluster admin?

Setting `TrustedCompute` weakens the crypto-shred contract (per F-C1 of the gate-0 review). If a tenant admin can flip it without cluster-admin sign-off, a compromised tenant admin escalates the residual-exposure window for their own data — visible to forensics.

**Suggested resolution**:
- Specify: `crypto_boundary` mutation requires cluster-admin authentication, NOT tenant admin. The tenant admin can request the flip via a separate `RequestCryptoBoundaryChange` admin RPC (out of scope for ADR-042; defer to operations ADR), but the actual flip is operator-gated.

### F-M7: Schema-evolution discipline for the new proto

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 (entire proto schema)
**Spec reference**: ADR-004 (schema versioning)

**Description**: ADR-004 specifies schema versioning for persisted records. ADR-042 introduces a wire protocol with no equivalent. proto3's default forward/backward compatibility helps, but doesn't cover semantic changes (e.g., changing the meaning of `crypto_boundary_at_open` in the HandleToken).

**Suggested resolution**:
- Add §X "Schema evolution": proto fields are append-only; field numbers never reused; deprecated fields tagged `[deprecated = true]` not removed. Token formats (HandleToken, DEK-fetch ticket) carry an explicit version byte.

### F-M8: GetTopology 304-equivalent has no freshness guarantee

**Severity**: Medium
**Category**: Correctness > Concurrency, Failure cascades
**Location**: ADR-042 §4 ("GetTopology returns empty TopologyInfo with topology_version set to current when known_topology_version == current")
**Spec reference**: I-NG13, A-NG13

**Description**: The 304-equivalent assumes the server's view of "current" is fresher than the client's. But if the server has a stale view (e.g., a leader change happened on a different node and the topology_version increment hasn't propagated), the server may return "you're current!" when the client is actually behind.

**Suggested resolution**:
- Document the consistency model: "GetTopology returns the *responding node's* view of the topology version, which may be stale by up to one Raft heartbeat (default 100 ms). Clients should treat the version as eventually consistent within this bound."
- Add `kiseki_native_topology_staleness_seconds` metric.

---

## LOW findings

### F-L1: NodeState enum wire stability

**Severity**: Low
**Category**: Correctness > Schema evolution

**Description**: The `NodeState` enum in `gateway_data.proto` has the same shape as the existing one in `kiseki-control` (Active / Degraded / Failed / Draining / Evicted). proto3 enum stability requires the numeric values to NEVER change. Architect should pick the values to match the existing enum so a future merge is value-stable.

### F-L2: PartEtag duplicates Etag

**Severity**: Low
**Category**: Correctness > Specification compliance

**Description**: `PartEtag { part_number, etag }` is the existing S3-multipart shape; ADR-042 reuses it in the new proto. No bug, but worth ensuring the field types match exactly so the implementer can share code paths.

### F-L3: Empty / ReadEof message types are noise

**Severity**: Low
**Category**: Specification

**Description**: `Empty` and `ReadEof` are placeholder messages used in `oneof` arms. Could be replaced with `bool` markers. Stylistic.

### F-L4: PutObjectStream and Multipart are two streaming mechanisms

**Severity**: Low
**Category**: Correctness > Specification compliance

**Description**: For 64 MiB ≤ payload ≤ 64 MiB, clients use PutObjectStream. For payload > 64 MiB, they use Multipart. The split is documented in §5, but the wire shapes differ enough that client code must handle both cases. Consider: could PutObjectStream extend to multi-session-resume to cover both? (Architect's call — both shapes are workable.)

---

## Cross-cutting observations

1. **The "system DEK" wording (F-C1) is the single most important fix.** The architect borrowed shorthand from informal conversation; the wire-protocol spec needs explicit key derivations.

2. **F-C2 (multi-chunk PUT atomicity)** is a genuine spec gap that the analyst phase didn't surface — the existing in-process `GatewayOps::write` orphans on failure today, but ADR-042's "no partial visible" promise is stricter than what the in-process impl actually provides. The architect must either weaken the promise or specify the rollback mechanism.

3. **F-H1 (handle token outliving cert)** is the kind of finding that gets pre-empted in the architect's §13 list ("trust boundaries") but the specific mechanism wasn't pinned down. Cert-SAN binding into the token is a small change with a big security delta.

4. **F-H2 (TrustedCompute N-chunk perf cliff)** undermines the entire reason for TrustedCompute. Without a batch DEK fetch, ServerOnly is faster than TrustedCompute for any object > 1 chunk. The architect should pick a fix BEFORE the protocol shape commits because it changes the wire-level keymanager interaction.

5. **F-H3 (GetTopology leak)** affects only multi-tenant SaaS deployments; single-tenant HPC clusters don't care. But the proto change is small and forward-compatible.

6. **F-H4 (lease + dedup race)** is the most subtle correctness issue. The fencing token must dominate the dedup short-circuit; the architect's spec implicitly inverted the priority.

7. **The architect's pre-emptive list (§13) covered 5 of these findings** at the symptom level: cert revocation (F-H1), TrustedCompute mode flip (F-C1 indirectly), lease+drain (F-M2 indirectly), topology version regress (F-M8), per-tenant counter atomicity (F-H6). Adversary's role is to drill from symptom to mechanism.

---

## Recommended next steps

1. **Architect**: amend ADR-042 §8/§9 to specify the handle-token + DEK-fetch-ticket signing key derivation (F-C1). Add the multi-chunk atomicity section (F-C2). These are protocol-shape decisions.
2. **Architect**: specify cert-SAN binding into HandleToken (F-H1), batch DEK fetch (F-H2), tenant-scoped GetTopology (F-H3), fencing-token-before-dedup ordering (F-H4), workflow_ref required-vs-default (F-H5), per-tenant counter implementation (F-H6).
3. **Analyst** (light loop): A-NG entries for the resolved choices on F-C1, F-C2, F-H1, F-H4. Update I-NG7 to specify both gateway and proto-boundary audit paths for the new RPCs.
4. **Architect → Adversary**: re-review the amendments. CRITICAL findings must clear before implementer touches code; HIGH findings should clear before the wire format commits.
5. **Implementer**: do not begin until F-C1 + F-C2 amendments land.

The 4 LOW findings can resolve during implementation — they're cosmetic / forward-compat.

---

## Resolution table (2026-05-05, all findings addressed in place)

User directive: "fix all findings" + "no backwards compat needed, we're pre-production." Resolutions reflect the pre-1.0 scope.

| Finding | Resolution location |
|---|---|
| **F-C1** (handle-token / DEK-ticket signing key) | New ADR-042 §9.1 *Cryptographic key derivations* specifies four named keys derived from the system master key via HKDF-SHA256 with distinct salts. Master-key-epoch is in every token for rotation. New invariant **I-NG15** captures the discipline. |
| **F-C2** (multi-chunk PUT atomicity) | New ADR-042 §5.0 *Multi-chunk PUT atomicity*: chunks parallel up to `KISEKI_PUT_CHUNK_PARALLELISM` (default 4); CommitStream is the all-or-nothing barrier; partial failure aborts the stream and orphan-fragment scrub reclaims staged chunks; no partial state visible. New invariant **I-NG16** + new failure mode **F-NG12** + severity-summary update (now P3 = 25, total catalogue = 47). |
| **F-H1** (handle token outliving cert) | HandleToken now carries `cert_san_canonical`; `OpenResponse` proto comment updated; new invariant **I-NG17** (token-cert-SAN binding). Cert revocation invalidates implicitly because the revoked cert can't establish a new mTLS connection. |
| **F-H2** (TrustedCompute N-chunk Read perf cliff) | New `BatchFetchDek` RPC added to `gateway_data.proto`; ADR-042 §1 service surface + §8 encryption-boundary text both reference the batch path. New A-NG21 captures the performance contract; new failure-mode F-NG13 captures the cliff for future-audit if a regression reverts. |
| **F-H3** (`GetTopology` cluster-structure leak) | `GetTopologyRequest.tenant_id` field added; server filters `shards` to caller's tenant. New invariant **I-NG20**. ADR-042 §4 documents the consistency model + the ≤100 ms staleness bound. |
| **F-H4** (lease fencing bypassed by dedup short-circuit) | ADR-042 §6 explicitly orders fencing-token check BEFORE dedup table consult. New invariant **I-NG18**. Dedup row records the request's fencing_token alongside response_summary for audit / post-mortem. |
| **F-H5** (workflow_ref required-vs-default) | ADR-042 §10 specifies the `workflow_ref_required_for_writes` policy: `true` → reject + security-failure audit; `false` → substitute `unattributed` literal + counter `kiseki_native_writes_unattributed_total`. New invariant **I-NG19**. |
| **F-H6** (per-tenant stream cap mutex hot path) | ADR-042 §12 swaps `parking_lot::Mutex<HashMap>` → `dashmap::DashMap<TenantId, Arc<AtomicUsize>>`. Sharded by tenant; lock-free atomic increment with overflow rollback on cap. |
| **F-M1** (repeated stream-`first` messages) | ADR-042 §5 specifies the single-`first` invariant: server returns `InvalidArgument{stream_already_initialized}` on second `first`; `InvalidArgument{stream_not_initialized}` on frames before any `first`. New invariant **I-NG24**. |
| **F-M2** (lease renewal audit storm) | ADR-042 §10 documents the `RenewLease`-as-counter rule: `kiseki_native_lease_renewals_total{tenant, namespace}` instead of per-event audit. New invariant **I-NG23**. |
| **F-M3** (inode allocation/recycling discipline) | ADR-042 §3 *Inode allocation discipline* specifies per-namespace monotonic 64-bit counter, never reused. Stale tokens return `Unauthenticated{inode_orphaned}`. New invariant **I-NG22**. |
| **F-M4** (multiple SAN URIs in cert) | ADR-042 §3 *Multiple SAN URIs* specifies exactly-one-match rule: zero matches → `Unauthenticated{no_kiseki_tenant_san}`, two+ matches → `ambiguous_kiseki_tenant_san`. New invariant **I-NG21**. |
| **F-M5** (`bytes::Bytes` zero-copy overstated) | ADR-042 §12 reframed as "minimize per-op gateway-side allocations"; honest about the tonic+prost framing copy. New A-NG24 captures the contract. |
| **F-M6** (crypto_boundary mutation auth path) | ADR-042 §8 specifies cluster-admin-only mutation; tenant admins request via separate admin RPC (deferred). New A-NG22. |
| **F-M7** (schema-evolution discipline) | ADR-042 §11.1 *Schema discipline (pre-1.0 scope)* — pre-production framing per the user's clarification. Iterate freely, schema_version byte on tokens is corruption/forgery defense not compat. Future "wire-stability" ADR will tighten post-1.0. New A-NG23 (pre-1.0 scoped). |
| **F-M8** (`GetTopology` 304-equivalent freshness) | ADR-042 §4 documents the consistency model: per-node view, ≤ one Raft heartbeat staleness (default 100 ms); `kiseki_native_topology_staleness_seconds` metric. Folded into I-NG20. |

The 4 LOW findings (F-L1..F-L4) remain implementation-time concerns:
- **F-L1** NodeState enum value stability: defer to implementation-time audit against `kiseki-control` / ADR-035's existing enum.
- **F-L2** PartEtag/Etag duplication: implementer ensures field types match the existing S3 multipart impl.
- **F-L3** Empty/ReadEof message types as `oneof` markers: stylistic; current shape is fine.
- **F-L4** PutObjectStream + Multipart as two streaming mechanisms: documented in §5 of the ADR; both shapes are workable.

All 2 CRITICAL + 6 HIGH + 8 MEDIUM = 16 substantive findings resolved at the architect-spec layer.

Final spec-layer counts (post-gate-1 amendments):
- ADR-042 sections: 17 main + §5.0 (multi-chunk atomicity) + §9.1 (key derivations) + §11.1 (schema discipline) — 20 sections
- Ubiquitous-language native-gateway terms: 17 (unchanged from analyst phase; gate-1 didn't add new terms)
- A-NG entries: 24 (added A-NG21..A-NG24)
- I-NG invariants: 26 unique IDs (I-NG1..I-NG24 with I-NG6 split into 6a/6b/6c — added I-NG15..I-NG24 in this gate-1 amendment)
- F-NG failure modes: 13 (added F-NG12, F-NG13); total catalogue 47

Architect can re-submit ADR-042 + amended proto for second-pass adversary review, or hand off to implementer if the user is satisfied with the gate-1 fixes.
