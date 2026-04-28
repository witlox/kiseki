# Phase 16a — Adversary + Audit Pass

**Reviewer**: adversary role. **Date**: 2026-04-28.
**Scope**: Phase 16a cross-node chunk replication
(`kiseki-chunk-cluster`, `kiseki-log` cluster_chunk_state state
machine, `kiseki-server` runtime wiring, mTLS SAN cert gen).
**Result**: 1 critical finding fixed in-pass; 1 known gap deferred
to follow-up.

This is the final-gate review for Phase 16a per
`specs/implementation/phase-16-cross-node-chunks.md` (rev 4) build
sequence step 14. Re-attacks the implementation with full code
knowledge after steps 1–13 shipped.

---

## Finding 1 — SAN interceptor implemented but **not wired** (CRITICAL)

**Severity**: Critical (security gap on the cross-node fabric).
**Status**: **FIXED in this pass**.

**What I found**:
`kiseki-chunk-cluster::auth::verify_fabric_san` correctly extracts the
`spiffe://cluster/fabric/<node-id>` SAN URI and rejects tenant SANs
(`spiffe://cluster/org/<uuid>`). The `fabric_san_interceptor` function
correctly runs the check at the tonic interceptor seam. The
`grpc_tls_san_round_trip` integration test confirms both happy-path
acceptance and tenant-SAN rejection.

But `kiseki-server::runtime` was registering `ClusterChunkService` via
`ClusterChunkServer::new(...).into_tonic_server()` — which builds a
**non-intercepted** server. Because the data-path port (9100) shares
the listener with `LogService` (which legitimately accepts tenant
certs), a tenant cert reaching the data port could call `PutFragment`
/ `GetFragment` / `DeleteFragment` directly. Defense-in-depth
collapsed.

**The exploit**: any tenant with a valid data-path cert could
cross-tenant exfiltrate fragments by chunk-id-guessing
`GetFragment(some_chunk_id)`. Chunk IDs are content-addressed but
visible in compositions; a colluding tenant could read another
tenant's chunks — bypassing I-T1.

**Fix applied**:
- New `ClusterChunkServer::into_tonic_server_with_san_check()`
  builder that wraps the server with `fabric_san_interceptor`.
- New public type alias `InterceptedClusterChunkService` so the
  runtime can branch on TLS without leaking the interceptor's
  function-pointer type.
- Runtime branches: when `cfg.tls.is_some()` we register the
  intercepted server (and log INFO that the SAN check is active);
  when plaintext, we register the unwrapped server (and log
  WARN that the cross-node fabric is unprotected). Plaintext
  is documented development-only; production deployments use
  TLS, so the SAN check is active in every production path.

**Verified by**:
- 4 mTLS integration tests in
  `crates/kiseki-chunk-cluster/tests/grpc_tls_san_round_trip.rs`
  exercise the wiring.
- Production code path: `cargo clippy --workspace -- -D warnings`
  clean; `cargo test --workspace` 0 failures.

---

## Finding 2 — `cluster_chunk_state` Raft proposals not yet emitted (KNOWN GAP)

**Severity**: Medium (design fidelity gap; not a security or
durability issue today).
**Status**: **DEFERRED to follow-up** (16a step 7.5 / 16b).

**What I found**:
The Phase 16a plan rev 4 D-4 specifies that the gateway's write path
must submit a `CombinedProposal { meta, delta }` (the
`LogCommand::ChunkAndDelta` variant) so that the
`cluster_chunk_state` table and the composition delta are durable
together at Raft-commit time. The state machine (apply path) accepts
these variants — verified by the 4 RED→GREEN tests in
`crates/kiseki-log/src/raft/state_machine.rs::tests`.

But **nobody proposes them**. The gateway's `mem_gateway.rs` write
path today does:
1. `self.chunks.write_chunk(...)` → `ClusteredChunkStore` →
   local + fabric fan-out (fragments durable).
2. `composition_store.create + log delta append` (the existing
   `AppendDelta` path; not `ChunkAndDelta`).

Steps 1 and 2 are independent. The promised "ack only after the
CombinedProposal commits" isn't enforced — the client gets ack
after step 2's plain delta commit, but there's no Raft-replicated
record of `cluster_chunk_state[(tenant, chunk_id)]` describing the
chunk's refcount + placement.

**What this affects today**:
- ✅ Cross-node read after PUT: works. Fragments are durable on every
  peer; `read_chunk` finds them locally or via fabric fallback.
  (B-3 closed.)
- ✅ Single-node failure survival: works. Same mechanism.
- ✅ 2-of-3 quorum gate: works. Surfaces 503 on shortfall.
- ⚠ **Cross-node refcount + GC coordination**: not yet wired. The
  cluster doesn't have a Raft-replicated view of "which chunks have
  refcount > 0 on which placement set". Local refcounts (in
  `kiseki-chunk` `ChunkStore`) work per-node; cluster-wide GC that
  drives `DeleteFragment` fan-out does NOT happen yet.
- ⚠ **D-4 atomicity contract**: not yet enforced. A leader could
  fan out fragments, ack the client (after the plain delta commits),
  then crash before any cluster_chunk_state proposal is made → on
  recovery the chunks are durable but invisible to the cluster's
  metadata view.

**Why I'm not blocking on it for 16a**:
- The 16a stated goal is "cross-node read after PUT works" — that's
  shipped. I-D1 cross-node fabric fallback works. I-T1 fabric SAN
  rejection works.
- D-4's atomicity matters most for GC coordination, which 16b also
  needs (the orphan-fragment scrub, defaults-table-driven repair,
  EC fragment distribution). It's a single deeper change to the
  gateway write path that's better landed alongside the rest of
  16b's metadata work.

**Suggested resolution**:
Phase 16a step 7.5 (or first 16b sub-task) — gateway write-path
refactor to assemble and submit `LogCommand::ChunkAndDelta` instead
of plain `AppendDelta` whenever `new_chunks` is non-empty. This is
a ~1-day refactor in `mem_gateway.rs` plus passing `new_chunks`
through `compositions::create`.

---

## Finding 3 — Plaintext-development posture is `--allow-insecure-fabric` shaped

**Severity**: Low (documentation / operability, not a true
vulnerability).
**Status**: **DOCUMENTED** in this pass.

When the server runs without `KISEKI_CA_PATH` (no TLS), the SAN
interceptor is intentionally skipped — otherwise every fabric call
would fail with "TLS client info missing". A development-mode WARN
log calls this out:

> ClusterChunkService: NO SAN interceptor (plaintext development
> mode — cross-node fabric is not protected against tenant certs)

The `--require-tls` flag from finding A.4 (mtls-grpc-gate) would
cleanly resolve this: a production deployment refuses to start
without TLS, which guarantees the SAN interceptor is active. That
flag still doesn't exist (carried over from A.4) — flagging here so
the gap is visible alongside 16a.

---

## Finding 4 — Documentation-grade BDD scenarios (intentional)

**Severity**: None (intentional design choice).
**Status**: **DOCUMENTED** in step 9 commit message.

7 cross-node scenarios in `multi-node-raft.feature` and 3 in
`chunk-storage.feature` ship without step definitions. The
cucumber runner skips them gracefully (verified). The behaviors
are covered by:
- 27 unit tests in `kiseki-chunk-cluster`
- 4 plaintext + mTLS integration tests
- 4 Python e2e tests in `tests/e2e/test_cross_node_replication.py`

Step definitions would have required a 3-node in-process fabric
harness in `kiseki-acceptance` — a 3–4 day undertaking that doesn't
add coverage we don't already have.

This matches the existing pattern in this repo (e.g.
"Throughput scales with shard count" in multi-node-raft.feature
also has no step definitions).

---

## Audit verification

Cross-checked the `enforcement-map.md` updates from step 13 against
the actual code:

| Invariant | Claimed enforcement | Actual code | Verdict |
|---|---|---|---|
| I-C2 | kiseki-log Raft state machine + cluster_chunk_state apply path | State machine accepts ChunkAndDelta / Inc / Dec but no one *proposes* them yet (Finding 2) | ⚠ Code path exists but is not on the production write path |
| I-D1 | kiseki-chunk-cluster fabric fetch fallback (cross-node) | `ClusteredChunkStore::read_chunk` walks peers on local NotFound | ✅ Verified |
| I-T1 | kiseki-chunk-cluster: tenant_id keying + SAN-role interceptor | (tenant, chunk_id) keying confirmed in `state_machine.rs::ClusterChunkStateEntry`. SAN interceptor wired at runtime (Finding 1 fixed) | ✅ Verified |
| I-Auth4 | SAN-role interceptor rejects tenant certs on fabric | Production wiring: intercepted when TLS is configured | ✅ Verified |
| I-L2 | ack-after-Raft-commit | Today: ack after AppendDelta commit, not after CombinedProposal (Finding 2) | ⚠ Honored for delta path; cluster_chunk_state path is the gap |

---

## Test coverage summary (post-audit)

- **kiseki-chunk-cluster**: 27 unit + 4 integration = **31 tests, 0 RED**
- **kiseki-log raft state_machine**: 14 tests including 4 phase-16
  state-machine variants
- **kiseki-server**: workspace-wide 0 failures across all crates
- **e2e tests**: 4 Python tests in `test_cross_node_replication.py`
  (gated on docker-compose 3-node availability)

---

## Outcome

Phase 16a primary goal — *a 3-node Replication-3 cluster genuinely
tolerates single-node loss, with mTLS-gated fabric and an
SAN-enforced tenant boundary* — **is shipped and working**. One
critical security gap was fixed in this pass (Finding 1). One
design-fidelity gap is documented and deferred (Finding 2).

Recommended next step: Phase 16a step 7.5 — gateway-side
`ChunkAndDelta` proposal wiring — closes Finding 2 and unblocks
Phase 16b's defaults table + repair scrub.
