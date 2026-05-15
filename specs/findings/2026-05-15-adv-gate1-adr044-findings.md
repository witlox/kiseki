# Adversary gate-1 — ADR-044 + Step A architect output

**Date**: 2026-05-15
**Reviewer**: Adversary (same agent advancing through the role chain)
**Targets**: `docs/decisions/adr/044-leader-forwarding-posture.md` (analyst output), commits `65ef681` (ADR-044) + `9ab4920` (architect — LogError variant + `append_delta_with_forwarding`).
**Scope**: Step A of `specs/implementation/write-routing-posture.md` — native server-side leader forwarding.

## Summary

0 Critical + 4 High + 4 Medium + 2 Low. Five spec-level findings (ADR §"Adversary gate 1 — findings", reproduced inline at the bottom of the ADR for traceability) plus this document's five code-level adversary checks against the architect's commits.

## Code-level findings

### C-H1 — `From<LogError> for KisekiError` loses `leader_node_id` (HIGH)

**File**: `crates/kiseki-log/src/error.rs` lines 86-121.

**Risk**: the `Into<KisekiError>` path maps `ForwardToLeader { shard_id, .. }` to `Retriable::ShardUnavailable(shard_id)` — the leader_node_id is dropped on the floor. If a caller upgrades to the new variant and then converts to `KisekiError` (e.g., for the `?` operator in an outer function), the leader hint vanishes.

**Resolution**: the native server proxy path MUST match `LogError::ForwardToLeader { leader_node_id, .. }` directly **before** any `?` / `Into<KisekiError>` conversion. The implementer step enforces this by structuring the gateway-level code path to check the LogError concrete variant inline. Adversary will re-verify in gate 2.

**Spec reference**: ADR-044 §"Decision" — native row; ADR-042 §4.

---

### C-H2 — `map_raft_error_with_forwarding` self-forward loop seed (HIGH)

**File**: `crates/kiseki-log/src/raft/openraft_store.rs` `map_raft_error_with_forwarding` helper.

**Risk**: openraft's `ForwardToLeader::leader_id: Option<C::NodeId>` could theoretically be `Some(self_node_id)` if the local node is mid-state-transition (lost leadership but still believes it's the leader, or vice versa). The helper currently emits `LogError::ForwardToLeader { leader_node_id = self_node_id }` in that case. The proxy path would then try to dial itself, infinite loop (bounded only by the hop counter in ADR-044 §"Hop cap").

**Resolution**: defense-in-depth at the proxy layer. The native server proxy code MUST reject `leader_node_id == self.node_id` with `Status::internal("self-forward loop — stale Raft state")`. Implementer step adds this check explicitly. The hop counter cap of 2 is the secondary safety net. ADR-044's "Open questions §1" (proxy retry semantics) covers the retry budget separately.

**Spec reference**: ADR-044 §"Hop cap" + I-L2.

---

### C-H3 — `kiseki-log::grpc::to_status` collapses `ForwardToLeader` onto `Status::unavailable` (HIGH→informational)

**File**: `crates/kiseki-log/src/grpc.rs` lines 35-39.

**Risk**: a *remote* caller invoking the `LogService::append_delta` gRPC (the Raft cross-node log service) would see `Status::unavailable` for `ForwardToLeader`, indistinguishable from the unknown-leader case. Loses the routing hint.

**Resolution**: **acceptable for Step A**. `kiseki-log::grpc` is the Raft cross-node log service used by replication, NOT by gateway clients. ADR-042 §4 native proxy operates at a different transport (gateway data port, not Raft port). The native gateway calls `OpenRaftLogStore` *directly* through `kiseki-gateway::mem_gateway` — never over `kiseki-log::grpc`. So this mapping is dead-code-equivalent for the ADR-044 path. No action required for Step A. **Re-open** if a future client design wants to use kiseki-log::grpc as a write proxy (which would conflict with ADR-026 anyway).

**Spec reference**: ADR-026 (per-shard Raft), ADR-041 (multiplexed transport).

---

### C-H4 — Hop counter is metadata-only; doesn't survive a serialization round-trip (HIGH)

**Risk**: ADR-044 §"Hop cap" specifies a hop counter carried in request metadata (`kiseki-proxy-hop-count` u8). If the metadata is dropped by an interceptor or middleware between the proxy node and the leader, the cap is silently disabled. Cycle defense degrades to "rely on Raft membership stabilizing within seconds."

**Resolution**: implementer step adds the hop counter as **both** (a) a tonic request metadata entry on the outbound proxy call, AND (b) a sanity check on the inbound side — if hop_count is present and >= 2, reject without consulting Raft. The metadata key is non-reserved (`kiseki-proxy-hop-count`) so it won't clash with tonic's internal `:method`/`:scheme`/etc.

**Spec reference**: ADR-044 §"Hop cap" + ADR-042 §4 "trailing metadata".

---

### C-M1 — `ForwardToLeader` variant ordering in the enum changes binary layout (MEDIUM→informational)

**File**: `crates/kiseki-log/src/error.rs` — `ForwardToLeader` inserted between `LeaderUnavailable` and `QuorumLost`.

**Risk**: if `LogError` were serialized in stable form (e.g., as a postcard tagged-union), inserting a variant in the middle would shift the discriminator of every variant after it. Wire-compatibility hazard.

**Resolution**: `LogError` is **not** serialized over the wire. It's mapped through `to_status` (`kiseki-log::grpc`) to tonic `Status` codes. The conversion is by-shape, not by-discriminant. No action required, but documented here so a future implementer adding `Serialize` to `LogError` knows to append-only.

**Spec reference**: ADR-004 (schema versioning).

---

### C-M2 — `append_delta_with_forwarding` doesn't carry `idempotency_key` in the request (MEDIUM)

**File**: `crates/kiseki-log/src/raft/openraft_store.rs` `append_delta_with_forwarding` + `crates/kiseki-log/src/traits.rs` `AppendDeltaRequest`.

**Risk**: ADR-042 §6 idempotency dedup is per-shard Raft state machine. The gateway-level code (in `kiseki-gateway::mem_gateway`) does its own dedup before calling into the log layer. If a proxy retry hits the leader's log layer directly (bypassing the gateway), the dedup wouldn't kick in.

**Resolution**: **acceptable for Step A**. The proxy hop is gateway-to-gateway (native server A's gateway → native server B's gateway), NOT gateway-to-log. So the dedup table at the gateway layer fires correctly because the proxied request hits B's gateway with the same `ControlFields.idempotency_key`. The log layer doesn't need its own dedup. Re-open if a future proxy variant bypasses the gateway.

**Spec reference**: ADR-042 §6 + ADR-044 §"Adversary gate 1 — H2".

---

### C-M3 — No metric for `ForwardToLeader` extraction rate (MEDIUM)

**File**: `crates/kiseki-log/src/instrumented.rs` `outcome_for` — currently uses `_ => outcome::ERROR` catch-all.

**Risk**: operators can't distinguish "leader stable but follower received write" (`ForwardToLeader`) from "election in progress" (`LeaderUnavailable`) from generic Raft failure (`Unavailable`) in the metrics. The cluster-health alarm in ADR-044 §"Consequences" (`kiseki_native_proxy_forwards_total{source_node, leader_node}` > 20% sustained) needs this distinction.

**Resolution**: implementer step adds a distinct outcome label for `ForwardToLeader` in `outcome_for`. Cost: 1 line. Captured here so it isn't forgotten.

**Spec reference**: ADR-015 (observability).

---

### C-M4 — `KisekiNode` payload (per-shard leader's `data_addr`) needs to reach the proxy code path (MEDIUM)

**File**: `crates/kiseki-log/src/raft/openraft_store.rs` `map_raft_error_with_forwarding` — discards `hint.leader_node` (the openraft `Node` payload).

**Risk**: openraft's `ForwardToLeader` carries both `leader_id: Option<NodeId>` AND `leader_node: Option<C::Node>` (which for kiseki is `KisekiNode` — the per-node connection info). The current architect cut extracts only `leader_id`. The proxy code path needs the leader's `data_addr` (gRPC endpoint) to actually dial it. The implementer step has two options:

1. **Look up `data_addr` from the local topology cache** (`kiseki-control::cluster_control::state_machine`). This is what ADR-044 §"Implementation map" specifies. Works because the control plane keeps per-node addresses globally synced. Adversary preference: this option, because it keeps `LogError` payload minimal and aligns with ADR-042 §4 "topology cache" architecture.
2. **Carry the `data_addr` inside `LogError::ForwardToLeader`**. Requires extending the variant and `KisekiNode → SocketAddr` conversion. Tighter coupling.

**Resolution**: implementer step uses option (1) — look up `data_addr` from the topology cache by `leader_node_id`. The current `LogError` shape (just `leader_node_id`) is sufficient. **No change required to the architect commit.** Document the decision in the implementer commit message.

**Spec reference**: ADR-044 §"Implementation map" + ADR-026 (Raft topology) + ADR-008 (discovery).

---

### C-L1 — `make verify` passes but doesn't yet exercise the new variant (LOW)

**Risk**: the architect commit (`9ab4920`) added the variant and method but has no tests calling them. `make test-fast` passes only because no caller has switched over yet.

**Resolution**: implementer step adds **RED** unit tests for:
- Extracting `ForwardToLeader { leader_node_id }` from openraft `ClientWriteError::ForwardToLeader(_)` with `Some(leader_id)`.
- Falling back to `LeaderUnavailable` when `leader_id == None`.
- The native gRPC proxy round-trip via 2-node test cluster.
- Idempotency replay through the proxy.

Then GREEN.

**Spec reference**: implementer protocol in `.claude/CLAUDE.md`.

---

### C-L2 — `OpenRaftLogStore` doesn't expose its own `node_id` for the self-forward check (LOW)

**Risk**: C-H2's self-forward defense needs `self.node_id`. The struct holds it implicitly (Raft's `node_id`) but doesn't have an accessor.

**Resolution**: implementer adds a `pub fn node_id(&self) -> u64` accessor on `OpenRaftLogStore` (the raft is private but the value is set at `new(node_id, ...)`). Cost: 5 lines. Captured here.

**Spec reference**: ADR-044 §"Hop cap" + this doc's C-H2.

## Disposition

- 4 HIGH findings — **all resolved or routed**:
  - C-H1: pattern-match in the gateway code BEFORE the `?` conversion. Implementer enforces.
  - C-H2: self-forward defense at the proxy layer. Implementer adds.
  - C-H3: not in scope for Step A; documented for future.
  - C-H4: hop counter as both outbound metadata + inbound check. Implementer adds.

- 4 MEDIUM findings — **all resolved or accepted**:
  - C-M1: documented for future serialization design.
  - C-M2: out-of-scope (gateway-layer dedup is sufficient).
  - C-M3: implementer adds outcome label.
  - C-M4: implementer uses topology-cache lookup (architectural decision recorded in ADR-044 §"Implementation map").

- 2 LOW findings:
  - C-L1: implementer writes RED tests first per TDD.
  - C-L2: implementer adds a 5-line accessor.

## Re-verification

Adversary gate 2 (post-implementer) re-checks:
1. C-H1 pattern-match enforcement at the gateway boundary.
2. C-H2 self-forward defense in proxy code path.
3. C-H4 hop counter end-to-end.
4. C-L1 RED-then-GREEN test progression preserved (no GREEN-only commits).
5. ADR-044 §"Adversary gate 1 — H1" (no early-ack short-circuit on proxy) — code inspection.
6. ADR-044 §"Adversary gate 1 — H2" (proxy-node-death + idempotency replay) — BDD scenario at `specs/features/native-gateway.feature:172-177` is GREEN.
