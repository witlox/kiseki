# Adversary Gate 1 Findings — ADR-041 (Raft transport multiplexing)

**Type**: Adversary → Architect
**Date**: 2026-05-04
**Reviewer**: adversary
**Verdict**: **CHANGES REQUESTED** — three HIGH findings block implementation
**Status (2026-05-04)**: ADR-041 amended; 3H + 5M + 2L findings resolved inline. See ADR-041 §"Gate-1 amendments" for the resolution table.
**Re: escalation**: `specs/escalations/2026-05-04-adv-gate1-adr041-raft-multiplexing.md`

## Summary

| Severity | Count |
|----------|-------|
| Critical | 0 |
| High     | 3 |
| Medium   | 5 |
| Low      | 7 |

The transport-multiplexing design is **structurally sound** — the
choice (single port + shard-tagged frames) matches ADR-026 Strategy C
and the wire format respects ADR-004 versioning conventions. Three
issues block implementation: a public-API ownership bug
(F-H1), an under-specified `unknown_shard` response shape that risks
caller hot-loops (F-H2), and missing supervision for the listener
task (F-H3). The MEDIUM findings are correct-but-tunable; the LOW
findings are mostly observability + future-proofing.

---

## HIGH findings (block implementation)

### F-H1: `run(self)` consuming the listener prevents `register_shard` calls after spawn

**Severity**: High
**Category**: Correctness > Implicit coupling
**Location**: `specs/architecture/adr/041-raft-transport-shard-multiplexing.md`, §"Server-side API"
**Spec reference**: ADR-041 itself

**Description**: ADR-041 specifies:

```rust
pub async fn run(self) -> io::Result<()>;
```

and concurrently specifies:

```rust
pub fn register_shard<C, SM>(&self, shard_id: ShardId, raft: Arc<Raft<C, SM>>);
pub fn unregister_shard(&self, shard_id: ShardId);
```

These two are mutually exclusive in Rust ownership. `run(self)`
consumes the listener; after the call, `&self` no longer exists, so
`register_shard` cannot be invoked. The §"Lifecycle" pseudocode
shows this incompatibility plainly:

```
2. Spawn listener.run() task            // consumes listener
3. Each shard create:
   b. listener.register_shard(...)      // listener has been moved
```

**Evidence**: this fails at `cargo check` time. The implementer
cannot satisfy the ADR as written.

**Suggested resolution**: split the listener from its registry.
Two clean options:

- **(a)** Expose a `RegistryHandle` that's `Clone + Send + Sync`,
  obtained from the listener before `run()` is spawned:
  ```rust
  impl RaftRpcListener {
      pub fn registry_handle(&self) -> RegistryHandle;
      pub async fn run(self) -> io::Result<()>;
  }
  impl RegistryHandle {
      pub fn register_shard<C, SM>(&self, ...);
      pub fn unregister_shard(&self, ...);
  }
  ```
- **(b)** Make `run` take `&self` and spawn the accept loop internally
  (returning a `JoinHandle` for shutdown ownership). Less symmetric
  but lets `register_shard` keep its current shape.

(a) is cleaner — the registry has its own lifecycle and the listener
is just the server wrapping. (b) preserves the existing `spawn_rpc_server`
ergonomics. Architect chooses.

---

### F-H2: Empty response on `unknown_shard` is indistinguishable from network error — caller may hot-loop

**Severity**: High
**Category**: Correctness > Failure cascades
**Location**: ADR-041, §"Server-side dispatch", step 6 + step 7 of `handle_one_connection`
**Spec reference**: ADR-026 §"Election storm mitigation"; ADR-034 §"grace period"

**Description**: ADR-041 specifies that an inbound RPC for an
unregistered shard receives an empty length-prefixed response (4
bytes of zeros). The §"Server-side dispatch" comment says:

> The empty response on `ShardNotFound` mirrors the existing
> behavior on parse errors — caller sees an empty body, treats it
> as a transient transport error, retries with backoff.

This is incorrect about openraft semantics. `RaftNetwork::append_entries`
returns `Result<AppendEntriesResponse<C>, RPCError<C>>`. Empty
response bytes deserialize-fail to `serde_json::Error`, which the
existing `to_rpc_error` helper maps to `RPCError::Network`. openraft
treats `RPCError::Network` as a transient failure and **retries
with exponential backoff up to the election timeout** — but the
caller is the leader of a *different* shard whose state and timing
are independent.

Consequence in the ADR-034 grace-period scenario:

1. Shard B is retired after merge; its `Raft<C, SM>` is unregistered
   from the listener.
2. A peer with a stale `NamespaceShardMap` continues to send
   `append_entries` to shard B. The peer is the leader of *some
   other* shard A; the calls are issued from A's heartbeat loop.
3. Each call gets an empty response → `RPCError::Network` →
   openraft retries A's heartbeat loop hits the dead shard B every
   tick, slowing A's progress.
4. Until the peer's shard map cache refreshes (which happens on
   write-side `KeyOutOfRange`, not on heartbeat error), the loop
   continues for the full ADR-033 §4 cache-refresh interval.

**Evidence**: ADR-033 §4 states "fetch the latest map from the
control plane via `GetNamespaceShardMap` RPC" only on
`KeyOutOfRange`. Heartbeat-only paths never trigger cache refresh,
so a stale heartbeat target persists indefinitely.

**Suggested resolution**: define a typed "unknown shard" response
on the wire and on the client. Two options:

- **(a)** Add a status byte to the response frame: `0x00 = ok`,
  `0x01 = unknown_shard`, `0x02 = parse_error`. Client maps
  `unknown_shard` to a NEW `RPCError::ShardRetired` variant which
  triggers cache refresh.
- **(b)** Return an explicit JSON `{"error": "unknown_shard",
  "shard_id": "..."}` in the response body. Same effect; verbose.

Either way the caller-side path needs to plumb the signal into the
shard-map cache invalidation.

---

### F-H3: Listener task panic loses all Raft RPC for the node — no supervision specified

**Severity**: High
**Category**: Robustness > Failure cascades
**Location**: ADR-041, §"Lifecycle" + §"Server-side API" `run()`
**Spec reference**: ADR-026 §"Election storm mitigation"

**Description**: ADR-041 specifies one listener per node and
explicitly notes:

> One call per node — subsequent calls fail with `EADDRINUSE`
> (existing kernel behavior, surfaced unchanged).

If the listener task panics (unwind from a malformed frame, OOM
during JSON parse, dispatcher closure panic propagating up through
`tokio::spawn`'s task boundary), the bind is dropped but no other
component knows. Every shard on this node loses the ability to
receive Raft RPCs. Symptoms:

- Heartbeats from peers fail; this node's followers transition to
  candidate state across all shards simultaneously.
- An election storm is triggered for *every Raft group on this
  node*, exactly the failure mode ADR-026 §"Election storm mitigation"
  was designed to bound.

Pre-ADR-041 had per-shard listeners; one shard's panic was isolated
to one Raft group. Multiplexing concentrates this single point of
failure.

**Suggested resolution**: specify supervision in §"Lifecycle":

```
Node startup:
  1. Build RaftRpcListener::new(raft_addr, tls)
  2. Spawn supervisor task that calls listener.run() in a loop;
     on panic, log + tokio::time::sleep(jitter(100ms..1s)) + retry
  3. Bound restart attempts (e.g., circuit-break after 10 panics in
     1 minute) to prevent infinite spin
```

Plus an `unwind = "abort"` panic policy on the dispatcher closures
(panic in dispatch should NOT take down the listener — it should
return an empty response and continue accepting). `tokio::spawn`'s
`tokio::task::JoinError::is_panic()` is the boundary — every
spawned per-connection task must catch its panic and continue the
accept loop.

Add a metric `kiseki_raft_transport_listener_restarts_total` so
operators see the restart rate.

---

## MEDIUM findings

### F-M1: Cross-runtime closure capture risks "block_on inside runtime" panics

**Severity**: Medium
**Category**: Concurrency > Implicit coupling
**Location**: ADR-041, §"Server-side API" `register_shard<C, SM>` closure body

**Description**: `RaftShardStore` owns its own dedicated tokio
runtime (`crates/kiseki-log/src/raft_shard_store.rs:42`) precisely
to avoid nesting. When `register_shard<C, SM>` builds a closure that
captures `Arc<Raft<C, SM>>`, the closure runs on the **listener's**
runtime (whichever runtime spawned `run()`). The Raft handle inside
was built on the `RaftShardStore` runtime. openraft's
`raft.append_entries(req).await` requires the runtime that owns the
Raft instance — calling it from a different runtime can panic.

The team has been bitten by this exact class of bug at least twice
(the recent `raft_shard_store_topology` test had to switch from
`#[tokio::test]` to plain `#[test]` for the same reason).

**Evidence**: `crates/kiseki-log/src/raft/openraft_store.rs:312`:
`tokio::spawn` on `self.raft.clone()` — explicitly relying on the
ambient runtime. Cross-runtime call would `tokio::spawn` onto the
wrong runtime.

**Suggested resolution**: specify in ADR-041 that the listener
**MUST** run on the same runtime that owns the Raft instances. In
practice: the listener is built and spawned by `RaftShardStore`,
not by the server's main runtime. Add a §"Runtime placement"
subsection to the lifecycle section pinning this.

---

### F-M2: No backpressure between shards — slow shard starves fast shards

**Severity**: Medium
**Category**: Robustness > Resource exhaustion
**Location**: ADR-041, §"Server-side dispatch" `tokio::spawn(handle_one_connection)`

**Description**: Pre-ADR-041, each shard's listener task was its
own scheduling unit; tokio's work-stealing balanced load. Post-ADR-041,
all per-connection tasks share the listener's runtime. A shard
performing a slow snapshot serialize occupies a worker thread for
the full transfer duration. Concurrent snapshots on multiple shards
exhaust the runtime's worker pool; even fast `append_entries` calls
to other shards queue behind them.

**Suggested resolution**: ADR-041 should require dispatcher
closures to use `tokio::task::spawn_blocking` for any operation
expected to take >1 ms (snapshots, large state machine apply
batches). Document the bound. Worker-pool sizing
(`KISEKI_RAFT_THREADS`) becomes more critical with multiplexing —
note in the ADR.

---

### F-M3: Snapshot transfers near the size cap

**Severity**: Medium
**Category**: Correctness > Edge cases
**Location**: ADR-041, §"Wire format"; `crates/kiseki-raft/src/tcp_transport.rs:29`

**Description**: `MAX_RAFT_RPC_SIZE = 128 MiB`. Adding
~60 bytes of (version + shard_id + tag) prefix to a snapshot
transfer that previously fit just under 128 MiB tips it over. The
existing snapshot-build code at
`kiseki-log::raft::state_machine::ShardStateMachine::build_snapshot`
emits postcard-encoded deltas; whether the resulting
`SnapshotEnvelope<C>` is sized exactly to 128 MiB or with headroom
is implementation-dependent.

**Evidence**: at 128 MiB cap, even a 0.0001% overhead on the wire
is ~50 KB. An exact-fit snapshot becomes a silent failure.

**Suggested resolution**: ADR-041 should mandate that the
snapshot-build code pad against
`MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED` (e.g., 1 KiB
reserve) and that tests verify a snapshot at the cap fits the
frame including prefix.

---

### F-M4: Trait surface migration breaks existing tests

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-041, §"Implementation guidance" item 3 ("`spawn_rpc_server` is removed")

**Description**: `crates/kiseki-log/tests/multi_node_raft.rs` calls
`spawn_rpc_server` directly. So does the recently-landed
`crates/kiseki-log/tests/raft_shard_store_topology.rs` (indirectly,
via `RaftShardStore::create_shard`). Removing the API forces a
migration of these tests. ADR-041 lists this in §"Implementation
guidance" but doesn't enumerate the affected sites.

**Evidence**: `grep -rn "spawn_rpc_server" crates/kiseki-log/`
shows `multi_node_raft.rs:62` (`_rpc1 = node1.spawn_rpc_server(...)`)
plus the inherent method itself.

**Suggested resolution**: ADR-041 §"Implementation guidance" should
add an explicit list of test files requiring update + a note that
the implementer commit them in the same change as the API removal
(no transient red CI).

---

### F-M5: Connection-flood attack amplified by single port

**Severity**: Medium
**Category**: Robustness > Resource exhaustion
**Location**: ADR-041, §"Server-side dispatch" accept loop

**Description**: Pre-ADR-041, an attacker connecting to one shard's
port consumed only that listener's task slots. Post-ADR-041, all
shards' RPC capacity sits behind a single accept queue. A peer
opening 100k connections (compromised cluster cert, malicious peer
in the cluster) blocks new RPCs to *every* shard.

The existing transport doesn't enforce any per-peer connection cap.
ADR-041 inherits this without amplification awareness.

**Evidence**: `crates/kiseki-raft/src/tcp_transport.rs:280`:
unbounded `tokio::spawn` per accept.

**Suggested resolution**: ADR-041 should specify a per-peer
connection cap (e.g., 16 concurrent inbound connections per peer
cert subject). Implementation: track active connection count per
peer-cert-fingerprint in a small `DashMap`; refuse accept (close
immediately) over the cap. Adversary review of the implementation
(gate 2) verifies the cap.

---

## LOW findings

### F-L1: Reserved version-byte values for compat with pre-ADR-041 sniffing

**Severity**: Low
**Category**: Correctness > Edge cases
**Location**: ADR-041, §"Wire format" version-byte assignment

**Description**: Pre-ADR-041 messages have no version byte; the
first byte of the framed payload is the start of a JSON value —
typically `0x5b` (`[`), `0x7b` (`{`), or `0x22` (`"`). If a future
ADR-041 amendment assigns one of these as a version code, an
ADR-041 server reading a pre-ADR-041 message would misinterpret it
as a known version and proceed to a parse error. Subtle.

**Suggested resolution**: ADR-041 should reserve `0x5b`, `0x7b`,
and `0x22` as PERMANENTLY unassignable for future version codes.
One-line addition to §"Wire format".

---

### F-L2: Closure-captured `Arc<Raft>` may delay shard teardown

**Severity**: Low
**Category**: Robustness > Resource exhaustion
**Location**: ADR-041, §"Server-side API" closure capture pattern

**Description**: `register_shard<C, SM>` builds a closure that
holds `Arc<Raft<C, SM>>`. The closure is stored in the registry. On
`unregister_shard`, the closure is dropped from the registry — but
any in-flight dispatch that already cloned the Arc keeps the Raft
handle alive until the dispatch completes. For a slow snapshot
dispatch, this could be 10s+ of seconds.

The Raft handle owns background tasks (heartbeat timers, log
applier). These continue to run during the grace period.

**Suggested resolution**: ADR-034's 5-minute grace period for shard
retirement is much longer than any in-flight RPC, so this is mostly
hypothetical. Document the property in ADR-041 (`unregister_shard`
is "best-effort prompt" with a tail bound by the longest in-flight
RPC) so future readers don't expect synchronous teardown.

---

### F-L3: Cert rotation timing across multiplexed shards

**Severity**: Low
**Category**: Security > Trust boundaries
**Location**: ADR-041, §"What does NOT change", mTLS posture

**Description**: Today, per-shard listeners can each re-bind during
rotation independently. Post-ADR-041, the single listener must be
restarted for cert rotation. During the brief restart, **all
shards** lose RPC connectivity simultaneously — same blast radius
as F-H3.

Mitigation: hot rotation via TLS context swap on the existing
listener (no rebind). The current `tls_acceptor: Option<TlsAcceptor>`
is captured at construction; swapping it requires either an
`Arc<RwLock<TlsAcceptor>>` or a tear-down + re-spawn of the
listener task.

**Suggested resolution**: ADR-041 should pin a hot-rotation
mechanism: `RaftRpcListener::set_tls_acceptor(new_acceptor)` swaps
the inner `Arc<RwLock<...>>` so subsequent accepts use the new
cert without rebinding. Implementer verifies under load.

---

### F-L4: `Raft<C, SM>` trait bounds may not satisfy `Send + Sync + 'static`

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-041, §"Server-side API" generic bounds

**Description**: The closure stored in the registry has `'static`
bounds; this requires `Raft<C, SM>: 'static`. The openraft type's
`SM` type param is bounded by `RaftStateMachine<C>`, but
`RaftStateMachine` itself isn't 'static-bounded — implementations
typically use `Arc<Mutex<...>>` internally so 'static is fine in
practice, but the ADR doesn't spell this out.

**Suggested resolution**: ADR-041 should explicitly require
`SM: RaftStateMachine<C> + Send + Sync + 'static` in the
`register_shard` signature; same for `C::D` and `C::R`. Confirms
the implementer doesn't paper over with `unsafe impl Send` or
similar.

---

### F-L5: Re-registered shard generation collision

**Severity**: Low
**Category**: Correctness > Concurrency
**Location**: ADR-041, escalation Q3

**Description**: A shard is unregistered (post-merge), its id is
later reused for a NEW Raft group (single-tenant decommission +
reuse pattern, hypothetical). Stale RPCs from the old Raft's term
arrive at the new Raft's dispatcher. openraft's vote/term
mechanism rejects them — the new Raft is in term 1; the stale RPC
carries term 5; openraft's "vote with newer term not seen" guard
applies. **Safe** in the sense of not corrupting state, but burns
cycles.

**Suggested resolution**: no ADR change required. Document the
property: shard_id reuse is permitted; openraft's term mechanism
provides epoch protection; reused shard_ids will see brief
"stale-term" rejection traffic until peer caches refresh.

Optional: add a `kiseki_raft_transport_stale_term_total{shard}`
metric so operators see when this happens. Non-blocking.

---

### F-L6: Per-shard metric cardinality at the cap

**Severity**: Low
**Category**: Robustness > Observability gaps
**Location**: ADR-041, §"Observability"

**Description**: At `shard_cap = 64` (ADR-033 §1) × 3 ops × 4
outcomes = 768 series per node for `kiseki_raft_transport_rpc_total`.
Acceptable. If `shard_cap` is raised (per ADR-026 Phase 3
scaling), this grows linearly. At 1000 shards × 3 × 4 = 12,000
series — Prometheus-cardinality-budget territory.

**Suggested resolution**: ADR-041 should note that the per-shard
label is bounded by `shard_cap`, and a future amendment may need
to drop the `shard` label (aggregate-only) at high shard counts.
Non-blocking.

---

### F-L7: Listener startup race not addressed

**Severity**: Low
**Category**: Correctness > Edge cases
**Location**: ADR-041, escalation Q7

**Description**: Listener bound but no shards registered yet — peer
RPCs return `unknown_shard`. With F-H2's typed response (caller
refreshes shard map), this is harmless: the caller refetches and
either finds the shard or learns it doesn't exist. With the empty
response (current ADR), see F-H2.

**Suggested resolution**: addressed by F-H2's resolution. No
separate change needed once F-H2 lands.

---

## What gate 1 confirmed

These elements of ADR-041 are sound:

- Single-port multiplexing direction is correct (matches ADR-026
  Strategy C's intent; the deferred-to-Phase-3 note in ADR-026 was
  optimistic about scale and ADR-041 corrects the timeline).
- Wire format with version byte respects ADR-004 conventions.
- DashMap registry is the right shape for concurrent
  read-dispatch + write-register.
- Closure-based type-erasure is correct given openraft's
  non-object-safe `SM` parameter.
- Migration as flag-day is acceptable for pre-1.0; preserves
  optionality for future N-1/N support via the version byte.
- mTLS posture preservation is correctly handled by the per-listener
  TLS context (subject to F-L3's hot-rotation requirement).
- Snapshot path is sound at the 128 MiB cap (F-M3 is precaution,
  not failure).

## Verdict

**CHANGES REQUESTED.**

Architect must address the 3 HIGH findings before implementer is
unblocked:

- **F-H1**: split listener / registry ownership so `register_shard`
  is callable after `run` is spawned.
- **F-H2**: typed `unknown_shard` response so callers can
  invalidate stale shard caches.
- **F-H3**: supervision strategy + per-task panic isolation +
  observability metric.

The 5 MEDIUM findings (cross-runtime, backpressure, snapshot size,
test migration, connection cap) are tunable in implementation but
should be acknowledged in the ADR as constraints the implementer
respects — not all need ADR text changes.

The 7 LOW findings are mostly future-proofing and observability.
F-L1 and F-L4 deserve one-line ADR additions; the rest can be
captured in implementer notes / metric definitions.

Re-review on architect's amendment.
