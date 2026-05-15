# Implementation plan: write-routing posture (A + B + C)

**Driver**: 2026-05-15 GCP compact perf run — `gateway_requests=1863 / 0 / 0` across the three nodes. The cluster *can* spread writes across shard leaders (ADR-033 §1 ships ≥ 3 shards per namespace; ADR-026 §"Raft group configuration" gives each shard an independent leader). The perf result reflects three layered gaps in client + server posture, **not** a Raft topology problem.

This plan sequences server-side proxy forwarding (A), bench-side fan-out (B), and client-side leader hints (C). All three land. Sequencing is driven by what removes errors from the wire vs. what removes overhead from it.

**Cross-references**: [docs/performance/targets.md](https://github.com/witlox/kiseki/blob/main/docs/performance/targets.md) for the per-profile aggregate-write targets this plan is designed to hit. [docs/performance/2026-05-15-gcp-compact.md](https://github.com/witlox/kiseki/blob/main/specs/performance/2026-05-15-gcp-compact.md) for the snapshot that surfaced the gap.

---

## 1. ADR mapping

| Piece | Existing ADR coverage | What's missing | Decision |
|---|---|---|---|
| **(A) Server-side leader forwarding** | ADR-042 §4 specifies a hybrid model: client direct-dial as default, **server-side proxy fallback configurable per cluster, default off**. The "proxy" path *is* in scope for native gRPC. ADR-026 says nothing about cross-node forwarding. ADR-038 §D3 explicitly forbids DS→MDS forwarding (pNFS DS is stateless). | (a) Native gRPC server has no `proxy_fallback` implementation. (b) S3/NFS gateways have nothing analogous and currently return 5xx on `LeaderUnavailable`. | **Implement ADR-042 §4 proxy fallback on native** (closes existing spec). **Issue ADR-044 (new): "Per-protocol leader-forwarding posture"** — declares S3 = 307 redirect (cheap, RFC-compliant), NFS/pNFS = no forwarding (`NFS4ERR_MOVED` + referral is the pNFS-shaped answer; deferred), native gRPC = full in-process proxy. ADR-044 is the *cross-protocol* policy doc; ADR-042 §4 is the native-specific mechanism. |
| **(B) Multi-shard bench fan-out** | ADR-033 §1 already produces N shards per namespace and §2 spreads leaders. The perf-suite simply doesn't exercise it. | Bench harness lacks any concept of multi-namespace or round-robin across `STORAGE_IPS`. | **No new ADR.** Bench-only change. Add a `specs/findings/2026-05-15-write-fanout-validation.md` finding doc capturing the baseline-vs-fanout delta. |
| **(C) Client-side leader hints** | ADR-008 §"Discovery query" already lists "List of active shards (shard_id, leader node, key range)". ADR-033 §4 specifies per-shard `leader_node` in `NamespaceShardMap`. ADR-042 §4 specifies `topology_version` trailing metadata for native. `kiseki-server::web::api::shard_leader` (line 413) already exposes `/cluster/shard/{id}` — leader info is on the wire today, just unused by S3/NFS clients. | (a) S3/NFS/FUSE clients don't consume `/cluster/info` per-shard leader fields. (b) `kiseki-client mount --endpoint` is single-host. (c) `DiscoveryClient::discover` (`crates/kiseki-client/src/discovery.rs:103`) returns a stub `"bootstrap"` shard pointing at the seed. | **Extend ADR-008** (rev 2 — "Per-shard leader exposure on discovery"). **Wire ADR-033 §4 `NamespaceShardMap` into discovery's `ShardEndpoint`** (the deferred work in ADR-033 §7 row 5). No ADR-045 needed yet — wait until non-native protocols (S3 307 / NFS referral) need their own decision. |

**Result**: one new ADR (**ADR-044** cross-protocol leader-forwarding posture) + one amendment to **ADR-008** rev 2. ADR-042 §4 and ADR-033 §4 / §7 row 5 stay where they are.

---

## 2. Sequencing

```
                  (B) bench fan-out
                 [bench-only, 0 Rust]
                          │
                          │ runnable independently — proves shards spread
                          │
                          ▼
       ┌─────────────────────────────────────┐
       │  Baseline measurement                │
       │  (B alone vs. B + A vs. B + A + C)   │
       └─────────────────────────────────────┘
                          │
            ┌─────────────┴─────────────┐
            ▼                           ▼
   (C) client leader hints      (A) server proxy forwarding
   [needs ADR-008 rev 2          [needs ADR-042 §4 implementation
    + native TopologyCache        + ADR-044 for S3 307 + NFS posture]
    refresh wiring]
            │                           │
            └───────────┬───────────────┘
                        ▼
                  Stabilization + verification matrix
```

Key sequencing facts:

- **(B) ships today.** Pure bash, zero Rust, no ADR. Cheapest measurement-quality lift — answers "is ADR-033 actually working?" without touching code. Land first to unblock the baseline for A and C.
- **(C) does NOT depend on (A).** Client-side direct dialing to the per-shard leader works **provided** the client falls back on `LeaderUnavailable`. The fallback already exists in `kiseki-log::error::LogError::LeaderUnavailable` (retriable per `error.rs:64`). The native client's `TopologyCache` already plans for `NotLeader{leader=X}` per ADR-042 §4. C can ship before A; A *removes the retry round-trip* C would otherwise pay on stale-leader cache.
- **(A) is the steady-state optimization.** It eliminates the client's stale-leader-retry penalty under cache churn (leader elections, splits). It's also the only viable path for S3/NFS clients that *can't* easily learn per-shard leaders (S3 has no shard concept; NFS clients route by mount point, not by hashed_key).
- **Slot vs. ADR-042 Phase 9 (gRPC perf tax):** Phase 9 in `specs/architecture/build-phases.md:771-779` is the **RDMA ibverbs binding** for ADR-042, NOT the gRPC perf optimization. The gRPC perf-tax work referenced in the prompt is the slice described in [specs/performance/2026-05-05-adr042-native-local.md](2026-05-05-adr042-native-local.md):30-45 — flamegraph + per-call codec setup, UUID parse, etc. **A + B + C are independent of that perf-tax work** because they target the *write-distribution* axis, not the per-op CPU cost axis. They can land in parallel slots; (A)'s proxy-path will inherit the same per-op tax until that work lands separately.

**Parallelism within the plan**:
- (B) and the ADR-044 draft can run in the same half-day.
- (A) gRPC proxy implementation and (C) S3/NFS client hint work are different crates (`kiseki-gateway::native` vs. `kiseki-client::native::topology_cache` + `kiseki-gateway::s3`); they can be split across two implementers.

---

## 3. Per-step deliverables

### Step B — bench fan-out (0.5 day, bench-only)

**Files**:
- `infra/gcp/benchmarks/perf-suite.sh` — phase 9 (lines 290-316).
- `infra/gcp/benchmarks/perf-common.sh` — add `pick_storage_for_client(idx)` helper.
- `specs/findings/2026-05-15-write-fanout-validation.md` — new finding doc capturing the before/after metrics-snapshot.

**Change shape**: replace `EP='$LEADER_S3'` in phase 9 with `EP="http://${STORAGE_IPS_ARRAY[$idx % ${#STORAGE_IPS_ARRAY[@]}]}:9000"`. Add a phase 9b that creates 3 namespaces (`perf-agg-ns0..2`) and routes each client to a different namespace AND a different node. The two effects (different namespace = different shard set; different node = different ingest gateway) compound.

**Acceptance**: post-run `metrics-snapshot.txt` shows `gateway_requests > 0` on all three nodes, ratio no worse than 4:1 between max and min.

**Invariant impact**: none (bench-only).

---

### Step A — native server-side proxy fallback (1.5 days)

**Files**:
- `crates/kiseki-log/src/raft/openraft_store.rs:304-315, 353-364, 397-408` (and the other `ForwardToLeader` call sites at 537, 572, 599, 634, 722) — extract `ClientWriteError::ForwardToLeader(hint)` into a new `LogError::ForwardToLeader { shard_id, leader_node_id }` variant. **Do NOT change retry semantics for existing callers**; introduce `append_delta_with_forwarding(req)` as a new method that returns the hint instead of `LeaderUnavailable`.
- `crates/kiseki-log/src/error.rs` — new variant.
- `crates/kiseki-gateway/src/native/grpc.rs` / `server.rs` — implement the proxy path. On `ForwardToLeader{leader_node_id}`, look up the leader's `data_addr` from the local topology cache (already maintained by `kiseki-control::cluster_control::state_machine`), build a `GatewayDataServiceClient` against that node, re-issue the RPC with the *original* `ControlFields` (idempotency_key dedups per ADR-042 §6), return the response + trailing `kiseki-topology-version` from the leader.
- Server config: `KISEKI_NATIVE_PROXY_FALLBACK=on|off` (default `off`, matching ADR-042 §4 "explicit-routing-only").
- `specs/findings/2026-05-15-leader-forwarding-posture.md` — new ADR.
- `specs/features/native-gateway.feature:161-178` — scenarios "transparently uses server-side proxy fallback" + "proxying node fails mid-proxy" are already drafted; wire BDD step impls.

**Acceptance**:
- Native PUT to any of 3 nodes returns 200 when `KISEKI_NATIVE_PROXY_FALLBACK=on`.
- Per-shard Raft metric `kiseki_log_forward_to_leader_total{shard,source_node,leader_node}` increments correctly.
- Existing `LeaderUnavailable` retry path on S3/NFS gateways unchanged (those still error out — A is native-only by design; S3 catches up via ADR-044's 307 path).

**Invariant impact**: **I-L2 unchanged**: the proxy node *forwards*, the leader still commits to its majority before the ack returns. The proxy node never returns success without the leader's `client_write().await` completing. Verify by inspection of the proxy code path — no early-ack short-circuit allowed.

**Days**: 1.5 (1 day proxy + 0.5 day BDD + integration).

---

### Step C — client leader hints (1.5 days)

**Files**:
- `crates/kiseki-server/src/web/api.rs:353-399` — extend `/cluster/info` JSON to include `shards: [{shard_id, leader_id, leader_data_addr, range_start, range_end}]` derived from the existing `NamespaceShardMap` (already on the control plane Raft per ADR-033 §4). For non-native (S3) callers, this is HTTP/JSON, costless.
- `crates/kiseki-client/src/discovery.rs:117-129` — replace the stub `DiscoveryResponse` with a real HTTP GET against `/cluster/info` (the seed nodes already serve this on the metrics port 9090). Populate `shards: Vec<ShardEndpoint>` with per-shard leaders.
- `crates/kiseki-client/src/native/topology_cache.rs:163-200` — already accepts `Snapshot{nodes, shards}`; wire `from_cluster_info_json(...)` constructor and a one-shot bootstrap refresh on `NativeClient::connect`.
- `crates/kiseki-client/src/native/client.rs:72-93` — accept `Vec<String>` of seeds (not just one) and dial the topology-cache-resolved leader for the request's hashed_key after bootstrap.
- `crates/kiseki-gateway/src/s3_server.rs` — implement S3 **307 Temporary Redirect** on `LogError::LeaderUnavailable` AND on a new `LogError::ForwardToLeader{leader_node_id}` (added in step A). Add `Location: http://{leader_host}:9000/...` from the cached topology. AWS S3 SDKs follow 307 by default. **No client-side change required for S3**.
- `crates/kiseki-gateway/src/nfs4_server.rs` — **scope: document only**, no code. The NFS path's pNFS layout already directs data ops to the right DS (ADR-038 §D3). Metadata ops on the wrong MDS would need `NFS4ERR_MOVED` + `fs_locations4`; defer to a follow-up since the perf-suite NFS phases (4-5) are not on the metrics-snapshot 0/0/1863 critical path.
- `docs/decisions/adr/008-native-client-discovery.md` — append rev 2 section "Per-shard leader exposure" referencing ADR-033 §4.
- `specs/features/native-gateway.feature:143-158` — "Native client dials shard leader directly" scenario steps.

**Acceptance**:
- `curl http://node-{1,2,3}:9090/cluster/info | jq .shards` returns the per-shard leader map on every node (eventually consistent within one Raft heartbeat per ADR-042 §4 consistency model).
- `kiseki-client mount --seeds host1,host2,host3 ...` succeeds and routes 64KiB PUTs to the correct leader on first try in ≥ 95% of cases (TopologyCache hit rate).
- S3 `curl -L` followed redirects spread requests across nodes; `metrics-snapshot.txt` confirms.

**Invariant impact**: ADR-033 §4 already mandates `leader_node` is best-effort/may-be-stale — clients MUST validate via `AppendDelta` returning `KeyOutOfRange`. C inherits this. Add metric `kiseki_native_topology_stale_leader_redirects_total` per ADR-042 §4 §"Consistency model".

**Days**: 1.5 (0.5 control-plane endpoint + 0.5 native client wiring + 0.5 S3 307).

---

## 4. Risks + open questions

1. **Proxy fanout doubles metadata-write hops.** At 3 nodes with bench-B fan-out, ~⅔ of PUTs already hit the correct leader directly (per the shard-spread math from ADR-033 §1: 3 shards, 3 leaders, hash uniform → 33% local-hit). With (C) client hints, the cache-hit rate should rise to > 95% in steady state. (A)'s proxy path is the *failure-mode safety net* (election churn, cache stale, multi-tenant cold start) — at steady state it carries < 5% of traffic, so the double-hop tax is bounded. The 2× cost only bites if (C) is broken or churn is high. **Mitigation**: ship metric `kiseki_native_proxy_forwards_total{source_node, leader_node}` and alarm if > 20% sustained.

2. **pNFS-style layout redirects as a generalization?** pNFS already does this for data (LAYOUTGET → DS endpoints). For metadata (composition deltas), the analogue would be S3 returning `Location:` to the leader OR NFSv4 `fs_locations4` referral. Trade-off: **cheaper hop** (client moves, server doesn't proxy), but **more client-side complexity** (every protocol needs its own redirect mechanic, retry budget, idempotency story). The hybrid answer in ADR-044 — 307 for S3 (cheap), proxy for native (controlled), pNFS-native referral for NFS (deferred) — is the right shape. **Open question for ADR-044 review**: should native gRPC also return `NotLeader{leader_addr}` as the **default** path and proxy as a fallback (matching pNFS's referral-first stance)? ADR-042 §4 already says default = client-direct, proxy = explicit opt-in. So we're already there.

3. **GH #38 EC-4+2 cap blocks ≥ 6-node validation.** `docs/performance/README.md:35` says "≥ 6-node profiles unusable until this lands." This bites us at: (a) measuring (B)'s fanout on a 6-node cluster — we can only validate on 3-node compact in the meantime; (b) showing (A) proxy-overhead at scale; (c) any meaningful pNFS layout test. **Mitigation**: validate A+B+C on 3-node compact; re-measure on 6-node default once #38 lands. The ratio-floor math (ADR-033 §3) says compact 3-node = 4-5 shards = enough leader spread to exercise the path.

4. **Native client TopologyCache bootstrap chicken-and-egg.** `NativeClient::connect(seed, tenant_id)` currently dials a single seed and gets no topology. With (C), the seed must already serve `GetTopology`. **Mitigation**: bootstrap via the existing HTTP `/cluster/info` (metrics port 9090, no mTLS needed for read-only topology — this is already public per ADR-019 §"Client resolution"); the gRPC `GetTopology` is for steady-state refresh.

5. **S3 307 redirect + retry storm under leader churn.** If 3 clients × 100 PUTs × 3 redirects/PUT during a leader-election window, that's 900 spurious requests. AWS SDK retries follow 307 by default but have a max-retry cap; manual `curl` doesn't unless `-L` is set. **Mitigation**: add server-side jitter on 307 (`Retry-After: 0-50ms`), and require the bench harness to use `curl -L --max-redirs 3`.

6. **Idempotency dedup interaction with proxy.** ADR-042 §6 specifies dedup state lives in the per-shard Raft state machine. The proxy retry path MUST re-use the original `idempotency_key` (else the proxy node and the leader would race to create two composition rows). The proposed proxy code path in step A keeps the original `ControlFields` byte-for-byte — verify in the BDD scenario "Server-side proxy fallback — proxying node fails mid-proxy" (line 172-177).

---

## 5. Verification matrix

| Step | Unit tests | BDD scenarios | Bench-delta target |
|---|---|---|---|
| **(B)** | none (bash) | none | 3-node compact: `metrics-snapshot` shows non-zero `gateway_requests` on all 3 nodes; aggregate throughput per phase-9 lifts from **528 MB/s → ≥ 1.3 GB/s** (3× ≈ linear-N speedup over single-node 528). |
| **(A)** | `kiseki-log/src/raft/openraft_store.rs` — test the new `LogError::ForwardToLeader{leader_node_id}` extraction; `kiseki-gateway/src/native/grpc.rs` — test proxy round-trip via 2-node `test_cluster` harness; idempotency-key replay through proxy. | `specs/features/native-gateway.feature:161-178` (2 scenarios already drafted). | With `KISEKI_NATIVE_PROXY_FALLBACK=on` and clients pointed at *non*-leader nodes: native PUT throughput within 10% of direct-dial-to-leader (validates A is correct and not bottlenecked). |
| **(C)** | `kiseki-client/src/discovery.rs` — `DiscoveryClient::from_cluster_info_json` parse test; `topology_cache.rs` — refresh-on-version-bump (already covered). | `specs/features/native-gateway.feature:143-158` (drafted). New scenario in `specs/features/cluster-formation.feature`: "`/cluster/info` exposes per-shard leader map". | After (C): proxy-forward-rate (metric `kiseki_native_proxy_forwards_total`) is **< 5%** of total writes under steady state (vs. ~67% without C on a 3-node cluster). |

**Quantified expected lift on 3-node compact (post A+B+C)**:

Starting numbers (2026-05-15): single-leader S3 parallel write = **528 MB/s** aggregate. With three shards (ADR-033 §1: `initial_shards = max(min(3×3, 64), 3) = 9` per namespace — 3 leaders distributed) and full client fan-out, the absolute ceiling is min(per-node-write-budget × 3, network-budget). Per-node write budget at S3 was measured at **673-1094 MB/s** (single-client), network is **31-46 Gbps ≈ 3.6-5.7 GB/s** per direction. Three-way parallel ingest should land at **~1.5-2.0 GB/s** aggregate write — a **3-4× lift over today's 528 MB/s**. After flattening for the proxy-tax bound (< 5% of traffic at < 2× cost = < 5% effective overhead), the realistic target is **1.4-1.9 GB/s aggregate S3 parallel write** on 3-node compact, up from 528 MB/s.

Native gRPC PUT (per ADR-042 §12: target 56 k op/s per node) scales linearly with leader spread: **3-node target ≈ 150-170 k op/s aggregate** after A+B+C, vs. single-leader bottleneck ceiling of ~56 k.

---

## Critical files for implementation

- `crates/kiseki-log/src/raft/openraft_store.rs`
- `crates/kiseki-gateway/src/native/grpc.rs`
- `crates/kiseki-server/src/web/api.rs`
- `crates/kiseki-client/src/discovery.rs`
- `infra/gcp/benchmarks/perf-suite.sh`
