# Analyst → Architect Handoff: ADR-042 Native Gateway Data Service

**Date**: 2026-05-05
**From**: Analyst
**To**: Architect
**Trigger**: 2026-05-05 perf measurement showed all client paths funnel through `RemoteHttpGateway` (HTTP/1 to S3 gateway). No genuine native protocol exists. Domain expert decided this is a load-bearing architectural lift; full diamond cycle authorized.

---

## Problem statement

Today every kiseki client path — S3, NFS, FUSE, the Rust SDK marketed as "native" — funnels through HTTP/1 to the S3 gateway (S3, FUSE, native) or raw NFS RPC framing (NFSv3/v4/pNFS). There is no `GatewayDataService` gRPC. Single-host 64 KiB GET caps at ~14 k op/s; flamegraph attributes the ceiling to HTTP/1 framing + per-op gateway tax.

HPC and AI training workloads are the primary target. Lustre / VAST / WekaFS comparisons are measured against the system's *native* client. Without one, kiseki cannot meaningfully claim parity. Strategic intent: aim for WekaFS-class performance (~150 k op/s per client small-file GET) without sacrificing the security, audit, and durability invariants kiseki already commits to.

---

## What's been decided (analyst phase, validated with domain expert 2026-05-05)

The 9 questions across 3 waves landed these answers — all captured as `A-NG*` entries in `specs/assumptions.md`:

| # | Decision |
|---|---|
| Q1 | Both **object-flavored** and **POSIX-flavored** verb families on the same gRPC service; FUSE eventually migrates from `RemoteHttpGateway` to the POSIX-flavored surface (separate work-stream after ADR-042 lands). |
| Q2 | Tenant identity is bound by **mTLS cert SAN URI ∧ payload `tenant_id` cross-check** (defense-in-depth). |
| Q3 | Discovery + leader routing is **hybrid**: client-side topology cache → direct leader dial → server-side proxy fallback on stale topology. |
| Q4 | Native POSIX is a **strict superset** over the FUSE POSIX subset (ADR-013): adds atomic within-shard cross-rename, atomic-create-with-content, lease-based RMW, async batched writes. |
| Q5 | **Streaming threshold = inline-threshold (8 KiB)**; **per-stream cap = 64 MiB** then multipart-equivalent. **Object writes commit-on-close**; **POSIX writes partial-visible-on-fsync**. |
| Q6 | Per-call control fields: **`workflow_ref`, `cache_hint`, `conditional`, `idempotency_key`** — all in the proto request struct, no session state, no out-of-band RPCs. |
| Q7 | **Hybrid encryption boundary**: server-decrypt by default (`ServerOnly`); per-namespace flag `TrustedCompute` opts into client-side decrypt for GPU-direct + zero-copy paths. |
| Q8 | **Honor I-L8** — cross-shard rename returns EXDEV. The Q4 "atomic cross-rename" superset means *atomic within-shard cross-directory*. |
| Q9 | **Audit principal = cert SAN URI verbatim**. Existing audit pipeline auto-fires (same `GatewayOps` trait dispatched as S3/NFS/FUSE). Future ADR (043+) for SPIFFE workload-identity binding. |

---

## Spec-layer artifacts produced

- **Ubiquitous language**: 11 new terms in `specs/ubiquitous-language.md` (Native fabric op, Gateway data endpoint, Object verb / POSIX verb, Inline op, Streaming op, Idempotency key, Cache hint, Cert-SAN tenant binding, Trusted compute pool, Hybrid leader routing, Lease-based RMW, Audit principal (native)).
- **Assumptions**: 11 entries `A-NG1..A-NG11` in `specs/assumptions.md`, including A-NG11 capturing the performance target (≥80 k op/s per node single-host 64 KiB native GET).
- **Invariants**: 10 entries `I-NG1..I-NG10` in `specs/invariants.md`, covering tenant binding (I-NG1), commit-on-close vs fsync semantics (I-NG2/3), within-shard atomicity (I-NG4), idempotency window (I-NG5), encryption boundary mode (I-NG6), audit-on-every-op (I-NG7), hybrid leader routing (I-NG8), streaming boundary (I-NG9), lease semantics (I-NG10).
- **Behavioral Gherkin**: 19 scenarios in `specs/features/native-gateway.feature` covering auth, object writes, POSIX writes, leader routing, streaming boundary, encryption boundary, audit, and a `@perf @smoke` scenario that asserts the 80 k op/s target.
- **Failure modes**: 7 entries `F-NG1..F-NG7` in `specs/failure-modes.md` covering cert-payload mismatch, mid-stream interruption, leader change mid-stream, lease holder crash, DEK fetch failure on client-decrypt, idempotency key cross-tenant safety, native traffic starving data-port heartbeats. Updated severity summary (now 41 failure modes total).

---

## Open questions for the architect (NOT analyst-level)

These remain architect-level after the gate-0 adversary review. Several questions previously listed here were resolved at the analyst layer (gate-0 findings F-C1, F-H1..F-H6, F-M1..F-M8, F-L1..F-L4 — see `specs/findings/2026-05-05-adv-gate0-adr042-analyst-findings.md` for the gate-0 audit, and the spec edits in `ubiquitous-language.md` / `assumptions.md` / `invariants.md` / `failure-modes.md` / `features/native-gateway.feature` for the resolutions).

1. **Lease heartbeat cadence** (I-NG10/I-NG14 detail): default TTL is 30 s; the *RenewLease cadence* a well-behaved client should use (recommended 1/3 to 1/2 of TTL) and the server-side eviction + admin-forced-revoke protocol need concrete numbers. Spec captures the *contract* (TTL, fencing token, drain interaction); architect picks the cadence and revoke path.
2. **Discovery RPC contract**: native client's topology cache populates from where? A new `GetClusterTopology` RPC on the data-port? Reuse `ControlService::ClusterStatus`? Spec commits to the `topology_version` push (I-NG13) and 30 s TTL fallback (I-NG8); architect picks the RPC shape and which existing service hosts it.
3. **Mid-stream resume token** (vs. fresh stream) for object streaming writes interrupted by network blips: spec commits to "fresh stream + idempotency_key" semantics (A-NG10 + I-NG5) which is correct; architect MAY add a resume-token optimization on top to save bandwidth on long-stream resumes, but it is not required for correctness.
4. **Multipart-equivalent shape** (above 64 MiB cap, I-NG9): is it `InitMultipart / PutPart × N / CompleteMultipart` like S3, or something native-shaped (e.g., a single streaming session that accepts re-attach via session-id)? Spec commits to multipart-equivalent semantics; architect picks the wire shape.
5. **mTLS SAN-role interceptor**: kiseki-server already has SAN-role interceptors for `ClusterChunkService` (cluster-only certs). Should `GatewayDataService` reuse the same interceptor with a "client" role, or have its own? The cert issuance story for tenant clients differs from cluster nodes. Spec commits to the canonicalization rules (I-NG1) and SAN URI shape; architect picks the interceptor implementation.
6. **Federation behavior** (ADR-016 cross-site): native ops cross sites or stay local? Analyst inferred "stay local" (federation is async by design), but architect should confirm and capture as ADR-042 scope.
7. **Implementation primitives** for hitting A-NG11's 80 k op/s target: tonic codec choice, zero-copy boundary, hot-path instrumentation defaults. Spec commits to the *target* and the *graduation gate* (in-process floor must be ≥100 k op/s before the protocol commits). Architect chooses the primitives.

8. **DEK cache (explicit non-goal for ADR-042; future ADR-04X)** — `derive_system_dek` (HKDF) ran on every write in the in-process flamegraph (~10 % of CPU per the 2026-05-05 measurement). The dedup short-circuit avoids it on dedup hits but a true read-repeat workload (HPC training loops re-reading the same chunks) still pays the cost on every read. The 2026-05-05 spike landed `Zeroizing` + TTL + crypto-shred-wipe on the existing plaintext `DecryptCache`; a DEK cache would be the **complementary** optimization with the **same security discipline** (process RAM only, `Zeroizing<[u8; 32]>` so drops zero the bytes, TTL matching F-CC3's 30 s default, wired to the same crypto-shred signal pathway). ADR-042 explicitly does NOT introduce a DEK cache. A future ADR-04X amends ADR-002 / ADR-011 with a unified crypto-cache discipline covering both plaintext and DEK caches; only that ADR may introduce DEK caching, and only after gating it on the security review of the trust boundary. Architect should capture this as an explicit non-goal in ADR-042's "Out of scope" section so the implementer doesn't add a DEK cache as an "obvious optimization" without the ADR.

---

## Adversary gate-1 hot spots (preview)

Most gate-0 hot spots have been addressed at the spec layer. Architect's draft of ADR-042 should still pre-empt these gate-1 candidates:

- **Cert-SAN forgery** — gate-0 F-H3 introduced canonicalization rules in I-NG1 + ubiquitous language entry "Canonical SAN URI form" + Gherkin scenario outline covering 6 near-miss cases. Architect must wire the canonicalization helper at proto-handler entry and treat it as part of the trusted base.
- **Idempotency key replay across tenants** — F-NG6 maps to the `(tenant_id, namespace_id, key)` triple. A-NG10 amended to specify Raft-replicated dedup state. Architect picks the redb table layout and the TTL sweep mechanism.
- **Stream-mid-leader-change atomicity** — I-NG2 + F-NG3 + I-NG8 + I-NG13 (topology_version push) compose so the client retries with the same idempotency_key against the new leader and the dedup state is replicated; verify no window where partial state is visible.
- **TrustedCompute namespace flag flip** — I-NG6a (immediate observability for object verbs, DEK-fetch-ticket carries at-Read-time mode) + I-NG6b (handle token carries open-time mode for POSIX) split the issue. Verify the DEK-fetch ticket cannot be forged.
- **Lease + drain interaction** — I-NG14 specifies the contract (drain waits for lease expiry; new lease requests against draining nodes rejected if TTL would outlast the quiesce window). Verify quiesce-window timing assumptions hold under ADR-035's drain protocol.
- **DEK exposure window for TrustedCompute** — F-C1 resolved by requiring `crypto_shred_policy = best_effort` on TrustedCompute namespaces (I-NG6c); operators acknowledge in writing. Verify the namespace metadata surfaces this on tenant-facing audit and reporting.
- **Cross-shard rename via two operations** — even though I-NG4 says EXDEV, a malicious or buggy client could implement "rename" as `Read+Write+Delete` and call it atomic. The *protocol* refuses cross-shard atomicity; the client may emulate non-atomic versions on top. ADR-042 should be clear about this.
- **Lease fencing token** — I-NG12 + A-NG12 added; F-H4 resolved. Verify token monotonicity is preserved across (tenant, namespace, inode) under leader change (state survives via the same Raft-replicated dedup-state mechanism).

---

## Graduation checklist (analyst → architect)

- [x] Domain model covers all bounded contexts touched by ADR-042 (no new contexts; native is a new ingress on existing Gateway/Composition/Chunk/Audit/Encryption contexts)
- [x] Ubiquitous language has one term per concept (16 new terms after gate-0 amendments: 11 original + crypto-shred policy / fencing token / topology version / canonical SAN URI form / + the implicit "DEK-fetch ticket" used in I-NG6a)
- [x] Every feature has concrete Gherkin scenarios (~30 scenarios after gate-0 additions: original 19 plus split routing + fencing + concurrent-stream cap + drain-interaction + cert-revoke + clock-skew + SAN-near-miss + crypto_shred_policy)
- [x] Invariants are testable (every I-NG* maps to at least one Gherkin step; I-NG count is now 14 invariants — original 10 plus I-NG6a/b/c split + I-NG11 stream cap + I-NG12 fencing + I-NG13 topology version + I-NG14 lease/drain)
- [x] Assumptions are explicit and falsifiable (20 A-NG entries: original 11 plus A-NG12..A-NG20 added in gate-0 amendments)
- [x] Failure modes documented with severity (11 F-NG entries: original 7 plus F-NG8 cert revoke / F-NG9 clock skew / F-NG10 proxy mid-fail / F-NG11 replay attack)
- [x] Cross-context interactions: audit (ADR-009), encryption (ADR-002), discovery (ADR-008), I-L8 honored (ADR-026), durability (docs/operations/durability.md), drain (ADR-035 — I-NG14), CRL (ADR-023 — A-NG17/A-NG19), time invariants (I-T1/I-T2 — A-NG18)
- [x] No TODOs / TBD markers in the spec text
- [x] **In-process gateway floor measurement (graduation gate from A-NG11 amendment)** — measured 2026-05-05 on the workstation host with `kiseki-profile --protocol in-process` (new driver added to `crates/kiseki-profile/src/protocols.rs`).

**Initial measurement (before spike):**

| Shape | In-process floor | vs today's S3 path | Reading |
|---|---:|---:|---|
| Get-heavy | 114 995 op/s · p99 666 µs · 7.2 GiB/s | 14 635 op/s | Protocol layer eats ~87 % of read throughput |
| Put-heavy | 20 089 op/s · p99 4.6 ms · 1.3 GiB/s | 3 477 op/s | Protocol eats ~83 % but the floor itself is gateway-internal-bound |
| Mixed (70 P / 30 G) | 18 917 op/s · p99 5.6 ms · 1.2 GiB/s | 4 578 op/s | Same as put-heavy: PUT path is the binder |

Read/write asymmetry: **5.7×** — anomalously high for any storage system (industry norm is 1.5–3× for distributed storage with replication; WekaFS targets ~1×).

**Spike landed 2026-05-05 to attack the asymmetry**:

1. **Plaintext decrypt cache hardened**: added per-entry TTL (default 30 s, env `KISEKI_DECRYPT_CACHE_TTL_MS`, matches F-CC3 contract), `Zeroizing<Vec<u8>>` so evictions clear bytes, public `wipe_decrypt_cache()` API for crypto-shred signal pathway. Resolves a pre-existing security debt where cached plaintext had no TTL and no zeroize-on-evict.
2. **Dedup short-circuit on writes**: before the per-write HKDF + AEAD seal, the gateway calls `try_increment_if_exists(chunk_id)` (new single-critical-section trait method on `ChunkOps` / `AsyncChunkOps`). On dedup hit (chunk already exists from any earlier write to the same content), the gateway skips the seal and simply increments refcount in one round-trip. Saves the entire seal+write path on dedup hits, plus eliminates the long-standing **double-increment bug** where `write_chunk` already incremented internally on dedup AND the gateway's `else` branch incremented again.

**After-spike measurement:**

| Shape | After spike | Δ vs initial | vs today's S3 |
|---|---:|---:|---:|
| Get-heavy | 128 513 op/s | +11.8 % | 8.8× |
| Put-heavy | 26 609 op/s | +32.5 % | 7.7× |
| Mixed | 26 777 op/s | +41.6 % | 5.8× |

Read/write asymmetry after spike: **4.83×** — improved but still above the 1.5× target user identified.

**Honest residual binder**: The new flamegraph (post-spike) shows the dominant hot frames are `parking_lot::Condvar::wait_timeout`, `SyncBridge::increment_refcount`, and `tokio::Mutex::blocking_lock` — i.e., the **chunk-store SyncBridge Mutex** is now the binder. The InMemoryGateway's `Mutex<CompositionStore>` is a secondary serialization point. Both are single-writer locks across all concurrent workers. To hit the 1.5× target requires structural refactoring of these locks (sharded HashMap, RwLock with shorter critical sections, or async-aware lock-free structures) — beyond the scope of a 1–2 day spike.

**Updated A-NG11 targets (post-spike)**: GET ≥80 k op/s (target preserved; floor is 128 k post-spike). PUT ≥18 k op/s for ADR-042-shaped scope (floor 26.6 k × 0.7 gRPC tax). To reach the 1.5× WekaFS-class write asymmetry the user requested, a **precursor ADR-04X (gateway hot-path lock refactor)** must land before ADR-042's write-target ratchets up to ≥85 k op/s.

**This adds a new architect open question**: should ADR-042 ship at the post-spike PUT target (≥18 k op/s) and the lock refactor follow as ADR-04X, or should ADR-04X land first and ADR-042 ratchet up?

Architect has everything needed to draft ADR-042 once the in-process floor measurement clears the graduation gate. Adversary gate-1 review against the ADR is the next phase.

---

## Performance framing carried into architect phase

The local profile matrix (2026-05-05, single-host workstation, write-behind enabled):

- S3 mixed: **4 578 op/s · p99 11 ms** (54 % of May-3 baseline 8 470 op/s)
- S3 get-heavy: **14 635 op/s · p99 4.4 ms** (57 % of May-3 baseline 25 843 op/s)
- The shape of the regression points at the gateway / per-op tax, not at HTTP framing alone.
- A genuine native gRPC client with the inline-threshold streaming boundary should land between 60–90 k op/s per node on this hardware (gRPC-tax ≈ 30 % vs. in-process gateway floor).
- WekaFS-class (150 k op/s) needs Phase B (io_uring + zero-copy) and possibly Phase C (DPDK/SPDK) on top of ADR-042. Not in ADR-042 scope; ADR-042 is the protocol that *enables* those phases.

Architect should hold the per-node 80 k op/s target as the design constraint for ADR-042's protocol shape choices.
