# ADR-044: Per-protocol leader-forwarding posture

**Status**: Accepted
**Date**: 2026-05-15
**Deciders**: Analyst (this ADR), Architect (Step A wire shape), Adversary (gate 1 — same agent, captured inline below)
**Adversarial review**: Gate-1 captured in §"Adversary gate 1 — findings" at the bottom of this ADR. 0C + 2H + 2M + 1L resolved in place; one HIGH (proxy-node-death replay) routed to BDD scenario `specs/features/native-gateway.feature` lines 171-177.
**Context**: ADR-042 §4 (hybrid leader routing — direct-dial default + proxy fallback), ADR-042 §6 (idempotency dedup), ADR-026 (per-shard Raft groups), ADR-033 §4 (per-shard `leader_node` in `NamespaceShardMap`), ADR-038 §D3 (pNFS DS stateless / no forwarding), ADR-008 (native client discovery — rev 2 in flight in parallel Step C), I-L2 (ack only after Raft commit), I-NG5 (idempotency dedup), I-NG13 (topology refresh on version mismatch), I-NG8 (cache safety-net TTL).

## Problem

The 2026-05-15 GCP compact perf run measured `gateway_requests = 1863 / 0 / 0` across three storage nodes. The cluster *can* spread writes across shard leaders (ADR-033 §1 provisions ≥ 3 shards per namespace; ADR-026 gives each shard an independent leader), but in practice **every protocol** today funnels writes to one node and that node is the only one that owns the namespace's shard leaders. Three layered gaps surface the symptom:

1. **(A) server-side forwarding** — when a non-leader node receives a write for a shard whose leader is elsewhere, openraft returns `ClientWriteError::ForwardToLeader(hint)`. `kiseki-log::raft::openraft_store` maps the hint to `LogError::LeaderUnavailable` and surfaces it as a 5xx-shaped failure. There is no in-process proxy path that re-issues the request against the leader and returns the leader's response transparently. ADR-042 §4 specifies the mechanism ("server-side proxy fallback configurable per cluster, default off — explicit-routing-only") but the implementation is missing.
2. **(B) bench-side fan-out** — the perf-suite drives every request at the same `LEADER_S3` endpoint. Even if A and C land, the bench can't observe the spread without fanning ingestion across `STORAGE_IPS`. Captured separately in `specs/findings/2026-05-15-write-fanout-validation.md`; **not in scope for this ADR**.
3. **(C) client-side leader hints** — S3, NFS, and FUSE clients don't consume `/cluster/info`'s per-shard leader map; they hit any node and pay whatever forwarding posture that node implements. Captured in ADR-008 rev 2 (separate work item Step C); **not in scope for this ADR** beyond declaring the cross-protocol policy each client must align with.

ADR-042 §4 already declared the *native gRPC* policy. What's missing is the **cross-protocol policy**: what should S3 do when a node receives a write for a shard whose leader is elsewhere? What should NFS/pNFS do? This ADR settles the cross-protocol question so each gateway implementation can converge.

## Decision

Adopt a **per-protocol leader-forwarding posture** rather than a single mechanism. Each protocol picks the cheapest correct mechanism for its wire shape:

| Protocol | Posture | Mechanism | Default | Reason |
|---|---|---|---|---|
| **Native gRPC** | In-process proxy (server-side forwarding) | Node receives request → openraft returns `ForwardToLeader{leader=X}` → handler dials X's native gRPC channel from the local `kiseki-control` topology cache → re-issues the **same** RPC with the **original `ControlFields`** byte-for-byte (idempotency dedup per ADR-042 §6) → returns response + trailing `kiseki-topology-version` from the leader. | `KISEKI_NATIVE_PROXY_FALLBACK=off` (matching ADR-042 §4 "explicit-routing-only") | Native clients carry the *topology cache* (I-NG13). Steady state, they direct-dial the leader and never hit this path. Proxy is the safety net for cold start / churn / election. Default-off forces explicit operator opt-in; turning it on trades a per-write double-hop on cache miss for "no `LeaderUnavailable` from the client's view." |
| **S3** | HTTP 307 Temporary Redirect | Server returns `Location: http://<leader_host>:9000<original-path>` from the locally-cached topology. AWS-style SDKs follow 307 with method-preserved semantics by default. Manual `curl` requires `-L --max-redirs N`. | Always on (no opt-out — RFC-compliant default behavior) | Cheap (client moves, server doesn't proxy). RFC 7231 §6.4.7 explicitly carries the request method through 307. No idempotency story needed: the redirected request hits the leader directly. Implementation in Step C, not Step A. |
| **NFSv4 / pNFS** | `NFS4ERR_MOVED` + `fs_locations4` referral | NFSv4.1 §11.4 RFC 5661: a server can declare a filesystem migrated. Clients follow `fs_locations4` to the new server. For pNFS, this applies to MDS metadata ops; **data ops are already redirected** by `LAYOUTGET` returning DS endpoints per ADR-038 §D3. | **Deferred to a follow-up ADR** | Perf-suite NFS phases (4-5) are not on the metrics-snapshot 0/0/1863 critical path. RFC 5661 §11.4 is well-defined and Linux NFSv4 clients implement it (`nfs_lookup_revalidate` path). Implementation expense doesn't justify the perf gain at this stage. Tracked as follow-up: `specs/findings/2026-05-15-nfs-leader-forwarding-followup.md` (to be written when prioritized). |
| **FUSE / native HTTP** | Inherits the underlying transport's posture | FUSE → native gRPC client → ADR-042 §4 hybrid routing applies. S3-over-HTTP from FUSE → 307 follow. No separate forwarding mechanic. | Same as the underlying transport | FUSE is a *client* of one of the other protocols, not a server in its own right. |

### Cross-references

- **ADR-042 §4** (existing) defines the *mechanism* for native gRPC proxy fallback (`KISEKI_NATIVE_PROXY_FALLBACK`, trailing `topology_version`). This ADR confirms its default-off posture and ties it into the cross-protocol policy.
- **ADR-042 §6** (existing) defines idempotency dedup in the per-shard Raft state machine. The proxy hop reuses the original `idempotency_key`; the leader's dedup table catches double-submits on retry.
- **ADR-026** (existing) defines per-shard Raft groups. The "leader" the proxy hops to is per-shard, not global.
- **ADR-033 §4** (existing) defines `NamespaceShardMap` with per-shard `leader_node`. The topology cache that the proxy reads is this map's runtime mirror.
- **ADR-038 §D3** (existing) forbids DS→MDS forwarding inside pNFS. This ADR does NOT contradict that: pNFS DS forwarding stays forbidden; NFS *MDS* `NFS4ERR_MOVED` referral is the analogous server-side mechanism on a different op axis (metadata, not data).
- **ADR-008 rev 2** (in flight, parallel Step C) extends discovery to expose the per-shard leader map. This ADR declares the *policy* for clients that don't yet consume that map: 307 for S3, follow-up for NFS, proxy for native.

### Hop cap (cycle defense)

Stale topology caches can produce A→B forwarding loops:

```
client → A: A's cache says shard's leader = B
A → B (proxy hop): B's cache says shard's leader = A
B → A (proxy hop): looping…
```

**Cap**: every native proxy request carries a request-extension counter `kiseki-proxy-hop-count` (u8) starting at 0. The server increments on each forward. When the counter reaches **2**, the server rejects with `Status::resource_exhausted("proxy hop limit exceeded — topology cache divergence")`. The client retries (its own topology refresh covers the divergence). The hop cap is a defense-in-depth measure; in steady state both nodes should agree on the leader within one Raft heartbeat (ADR-042 §4 "consistency model"). S3's 307 path has the same risk and is bounded by `--max-redirs N` on the client (RFC 7231 §6.4 mandates a cap; AWS SDK defaults to 3).

## Consequences

### Accepted

- Native gRPC handlers gain a new error-flow surface: `LogError::ForwardToLeader{shard_id, leader_node_id}` (separate from `LeaderUnavailable`). Existing callers that don't opt into the new method keep the old `LeaderUnavailable` mapping — no regression.
- The native server holds a `tonic` channel to every node it has ever forwarded to (HTTP/2 multiplexing — same channel hosts all subsequent forwards). Channels are evicted on topology snapshot rotation. Bounded by the cluster size (≤ N nodes).
- The proxy hop pays one extra round-trip and one extra TLS handshake-amortized HTTP/2 stream open. Under steady state with client-side leader hints (ADR-008 rev 2), this fires on < 5 % of writes (cache miss, churn, election); the rest hit the leader directly. Metric `kiseki_native_proxy_forwards_total{source_node, leader_node}` exposes the rate; alarm if sustained > 20 %.
- I-L2 holds: the proxy waits for the leader's `client_write().await` to return successfully before returning to its own caller. No early-ack short-circuit. The leader still acks only after Raft commit on its majority.
- Idempotency dedup (ADR-042 §6) covers the failure case where the proxy node dies after the leader committed but before the response reached the client: the client retries via topology refresh + direct dial; the leader's dedup table short-circuits to the original response.

### Rejected

- **"Always proxy" as default**: rejected because the double-hop tax compounds with the gRPC per-call codec / UUID parse tax (see `specs/performance/2026-05-05-adr042-native-local.md`) and steady-state native clients already have a topology cache. Proxy is the safety net, not the steady-state path. ADR-042 §4 already settled this. Explicit-routing-only is the default; operators flip it on when a workload demands "no LeaderUnavailable from the client's view" (long-tail single-PUT scripts, S3 SDK fallback path, etc.).
- **"Always direct dial; no proxy at all"**: rejected because S3 has no shard concept — its clients can't compute the shard owner before sending. 307 is the equivalent of proxy for S3 (server tells client where to go). For native gRPC, proxy is the operator's choice for "noise floor of LeaderUnavailable retries" reduction; banning it would force every client to implement perfect topology caching.
- **"Proxy fallback for S3 too"**: rejected because S3 already has 307. Proxying S3 in the server would require the server to be an HTTP client of itself, doubling the per-request socket count and forcing the proxy node to buffer the entire PUT body. The 307 path streams from the client through the load balancer directly to the leader at zero proxy cost.
- **"Implement NFS4ERR_MOVED now"**: rejected for *this* ADR. The wire shape is RFC-defined and the implementation effort is non-trivial (`fs_locations4` requires NFSv4 server-side support that the kiseki-gateway NFS server doesn't have today). Defer until perf-suite NFS phases (4-5) become a critical path. This ADR records the *intent* (NFS path will use referral, not proxy or LOOKUP-side proxy) so future work doesn't re-litigate the design.

## Implementation map

| Concern | Files | ADR section |
|---|---|---|
| New `LogError::ForwardToLeader` variant | `crates/kiseki-log/src/error.rs` | §"Decision" — native row |
| Surface forward hint from openraft | `crates/kiseki-log/src/raft/openraft_store.rs` lines 304-315, 353-364, 397-408 + 537, 572, 599, 634, 722 (new method `append_delta_with_forwarding`; existing `append_delta` semantics unchanged) | §"Decision" — native row |
| Native server proxy path | `crates/kiseki-gateway/src/native/server.rs` + `crates/kiseki-gateway/src/native/grpc/adapter.rs` | §"Decision" — native row + §"Hop cap" |
| Config — `KISEKI_NATIVE_PROXY_FALLBACK` | `crates/kiseki-server/src/runtime.rs` | §"Decision" — default-off |
| Metric — `kiseki_native_proxy_forwards_total{source_node, leader_node}` | metrics registry (existing) | §"Consequences" |
| BDD scenarios | `specs/features/native-gateway.feature:161-178` | §"Decision" — native row |
| S3 307 path | `crates/kiseki-gateway/src/s3_server.rs` (**Step C scope, not Step A**) | §"Decision" — S3 row |
| NFS `NFS4ERR_MOVED` referral | (Deferred — follow-up ADR) | §"Decision" — NFS row |

## Adversary gate 1 — findings

Captured 2026-05-15 by the same agent advancing through the role chain. Five findings; resolutions in place.

### H1 — I-L2 break via proxy short-circuit (HIGH)

**Risk**: if the proxy node returns success before the leader's `client_write().await` completes, the client thinks the write committed but quorum may not have ack'd yet. Violates I-L2 ("ack only after Raft commit").

**Resolution**: the proxy code path is a *blocking* `.await` on the upstream gRPC call. The native server hands the request to the leader's `GatewayDataServiceClient::put_object(...)` (or sibling verb) and awaits the response. The leader does the openraft `client_write().await` before returning its own response. End-to-end: proxy node returns success ⟸ leader returns success ⟸ Raft majority committed.

**Inspection rule**: the proxy code path MUST be a single `.await` on the upstream RPC. No `tokio::spawn` of the forward, no early return, no `tokio::time::timeout(...)` shorter than the underlying `KISEKI_RAFT_PROPOSAL_TIMEOUT_MS`. Adversary will re-verify in gate 2.

### H2 — Proxy node dies mid-proxy (HIGH)

**Risk**: client → proxy node A → leader B. B commits successfully. A crashes between (a) receiving B's response and (b) sending it to the client. The client sees a transport-level failure (`Status::aborted` or TCP RST) but the write *did* commit on B. The client retries; without idempotency dedup, the retry would commit a *second* time.

**Resolution**: idempotency dedup (ADR-042 §6) catches this. The proxy forwards the *original* `ControlFields` (including `idempotency_key`) byte-for-byte. The client retry uses the same key. The leader's dedup table (in the per-shard Raft state machine, TTL 5 min) short-circuits to the original response — exactly-once semantics preserved.

**BDD coverage**: `specs/features/native-gateway.feature:172-177` scenario "Server-side proxy fallback — proxying node fails mid-proxy" exercises this. The implementer (next step in the chain) wires step impls and verifies RED-then-GREEN.

### M1 — Forwarding cycle on stale caches (MEDIUM)

**Risk**: A's topology cache says the leader is B; B's cache says the leader is A. Proxy loop: A → B → A → B → … unbounded.

**Resolution**: hop counter capped at 2 (see §"Hop cap" above). On hop 3, server returns `Status::resource_exhausted("proxy hop limit exceeded")`. Client refreshes its own cache and retries. Two-cluster steady-state divergence is bounded by one Raft heartbeat (ADR-042 §4 consistency model).

**Open**: is 2 the right cap? In a 3-node cluster, the worst legitimate case is 1 hop (client hits non-leader → forwarded once). Cap of 2 absorbs a single in-flight transition (leader was X, now Y, X still believes itself the leader). Cap of 3+ extends the divergence window into a perf-tail latency hazard. **Decision: cap at 2.**

### M2 — Cross-tenant SAN-vs-payload check at proxy boundary (MEDIUM)

**Risk**: client presents SAN for tenant T1; proxy node A validates (passes); A re-issues to B; B re-validates against… A's SAN? The connection from A to B is a *node-to-node* mTLS connection, not the client's.

**Resolution**: the proxy node A forwards the original `ControlFields.tenant_id` to B. B's SAN check is against A's node cert (since A is the gRPC client to B). A is a cluster-trusted node so the SAN-vs-payload tenant check (I-NG1) does **not** apply on the node-to-node hop — instead, B treats A as a *delegating* client. To preserve auditability, the forwarded request carries a new `forwarded_from_node` field in `ControlFields` (or as request metadata `kiseki-forwarded-from-node`) so B's audit log records both the originating node and the original tenant. Implementer adds this in the proxy path; A populates the field from its own `NodeId` before forwarding.

**Audit invariant**: every audit record on B for a forwarded request MUST carry `forwarded_from_node = A.node_id`. Spec audit-record schema lives in ADR-009 / kiseki-audit; the implementer adds the field if it's missing.

### L1 — Config name overlap with future "client-side proxy" (LOW)

**Risk**: `KISEKI_NATIVE_PROXY_FALLBACK` could later be confused with a client-side concept (e.g., a future "auto-proxy through a sidecar"). Naming clarity.

**Resolution**: the var name is specified by ADR-042 §4 already; keep as-is. Future client-side concepts get their own prefix (`KISEKI_CLIENT_PROXY_…`). Documented here.

## Alternatives considered

1. **Proxy at the load-balancer level (HAProxy / Envoy plug)**: rejected because it adds an external dependency (kiseki targets bare-metal HPC clusters where the LB layer is often a kernel routing table, not an Envoy mesh). The in-process proxy is "no new deployment dependency."
2. **Implement `NFS4ERR_MOVED` immediately**: see "Rejected" above. Deferred.
3. **Drop `LeaderUnavailable` entirely and always proxy**: rejected; see "Rejected" above. ADR-042 §4 explicitly opts for "explicit-routing-only" as the default.
4. **Cap proxy hops at 1**: rejected because legitimate in-flight leader transitions can require a 1-hop redirect (client → non-leader → leader). Capping at 1 prematurely errors on every transition. Cap of 2 absorbs one transition.

## Open questions

1. **Should the proxy retry on transient transport errors to the leader?** Current decision: **no retry inside the proxy**. The client-side retry budget covers transport-level failures; the proxy doing its own retry would multiply the latency on a slow-failing leader. Re-evaluate if the BDD scenarios surface a perf concern.
2. **Should the proxy share an HTTP/2 channel with the native gRPC client's outbound dialer?** Open: a single shared channel pool on each node would simplify the resource footprint. Decision: implementer reuses the existing `kiseki-control` topology cache's channel pool if it exists; otherwise opens a per-leader channel and lets HTTP/2 multiplex.
