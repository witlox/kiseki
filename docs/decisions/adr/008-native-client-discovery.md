# ADR-008: Native Client Fabric Discovery

**Status**: Accepted
**Date**: 2026-04-17
**Context**: Escalation point 8, A-ADV-1, I-O4

## Decision

Native clients discover shards, views, and gateways via a lightweight
**discovery service** running on every storage node, accessible on the
data fabric. No control plane access required.

## Mechanism

1. **Bootstrap**: client is configured with a list of seed endpoints
   (storage node addresses on the data fabric). Seed list can be
   provided via environment variable, config file, or DHCP option.

2. **Discovery query**: client sends a discovery request to any seed.
   The storage node responds with:
   - List of active shards (shard_id, leader node, key range)
   - List of materialized views (view_id, protocol, endpoint)
   - List of gateway endpoints (protocol, transport)
   - Tenant authentication requirements

3. **Authentication**: client presents mTLS certificate (Cluster CA signed,
   per-tenant). Optional second-stage auth via tenant IdP.

4. **Cache**: discovery results cached with TTL. Periodic refresh.
   Shard split/merge events invalidate relevant cache entries.

5. **Transport negotiation**: client probes available transports
   (CXI → verbs → TCP) and selects highest-performance option.

## Why not DNS-SD or multicast

- Slingshot fabric may not support multicast reliably
- DNS-SD requires DNS infrastructure on the data fabric
- Seed-based discovery is simple, deterministic, and works with any transport

## Consequences

- Every storage node runs a discovery responder (lightweight, part of kiseki-server)
- Seed list is the only bootstrap configuration for compute nodes
- Discovery responder must not expose tenant-sensitive information
  (shard/view metadata is operational, not tenant content)

---

## Rev 2 — Per-shard leader exposure on discovery

**Status**: Accepted
**Date**: 2026-05-15
**Driver**: Step C of `specs/implementation/write-routing-posture.md` —
the 2026-05-15 GCP compact perf run measured `gateway_requests = 1863 /
0 / 0` across three nodes because the SDK-direct path bootstraps off a
single seed and never learns per-shard leader placement. ADR-033 §4
already maintains a `NamespaceShardMap` with `leader_node` per shard on
the control-plane Raft group; rev 1 just hides it from clients.
**Cross-refs**: ADR-033 §4 (`NamespaceShardMap` is the source of
truth); ADR-033 §7 row 5 (the deferred work this rev closes);
ADR-042 §4 (steady-state refresh via gRPC `GetTopology` trailing
`topology_version`); ADR-014 (cross-protocol leader-forwarding posture
— declares S3 = 307 redirect, native = client direct-dial, NFS =
deferred).

### What rev 2 adds

1. **Per-shard leader map on bootstrap discovery.** The seed-bootstrap
   step from rev 1 was specified for a single shard ("List of active
   shards (shard_id, leader node, key range)"). Rev 2 promotes this
   from a placeholder to a concrete wire shape: the
   `/cluster/info` HTTP/JSON endpoint adds a top-level `shards: [...]`
   array, each entry `{shard_id, leader_id, leader_data_addr,
   range_start, range_end}` derived from the
   `NamespaceShardMap`. This is the bootstrap analogue of the gRPC
   `GetTopology` (ADR-042 §1) `TopologyInfo` — same logical content
   shape, HTTP/JSON form to avoid a chicken-and-egg dependency on the
   data port for the very first connection.

2. **Multi-seed bootstrap.** Rev 1 specified "list of seed endpoints"
   but the SDK lands today with `NativeClient::connect(seed,
   tenant_id)` accepting a single string. Rev 2 mandates the API
   surface accepts a `Vec<String>` (CLI form
   `--seeds host1,host2,host3`), and bootstrap dials seeds in order
   with a per-seed connect timeout of 2 s before falling through to
   the next.

3. **Bootstrap consistency model (eventual-within-one-heartbeat).**
   The cached topology from `/cluster/info` is eventually consistent
   within one Raft heartbeat (default 100 ms — ADR-042 §4
   "Consistency model"). Clients MUST validate each per-request
   routing decision via the existing enforcement: `AppendDelta`
   returns `KeyOutOfRange` (ADR-033 §5a) when the cached shard map is
   stale, and the leader hint is validated by the underlying Raft
   layer returning `LeaderUnavailable` / `ForwardToLeader` when the
   leader has moved. On either signal, the client refreshes via the
   steady-state gRPC `GetTopology` (ADR-042 §4) or a fresh
   `/cluster/info` poll if the gRPC channel is not yet established.

4. **Cross-protocol redirect coordination.** ADR-014 (the new
   leader-forwarding posture ADR introduced by Step A of the same
   plan) declares the per-protocol policy: native = client direct-dial
   to leader (this rev's mechanism), S3 = 307 Temporary Redirect on
   `LeaderUnavailable` / `ForwardToLeader`, NFS = no forwarding
   (`NFS4ERR_MOVED` + referral is the pNFS-shaped answer; deferred).
   The S3 307 path uses the *same* topology cache that the native
   client uses, populated by the same `/cluster/info` endpoint, so
   non-native clients also benefit from leader-aware routing without
   running their own discovery client.

### Wire shape — `/cluster/info` rev-2 JSON

```json
{
  "node_id": 1,
  "s3_addr": "10.0.0.1:9000",
  "nfs_addr": "10.0.0.1:2049",
  "metrics_addr": "10.0.0.1:9090",
  "leader_id": 1,                 // bootstrap shard (rev 1, retained)
  "leader_s3": "10.0.0.1:9000",   // bootstrap shard (rev 1, retained)
  "peers": [ ... ],               // rev 1, retained
  "shards": [                     // rev 2 — per-shard leader map
    {
      "shard_id": "00000000-0000-0000-0000-000000000001",
      "leader_id": 2,
      "leader_data_addr": "10.0.0.2:9100",
      "range_start": "0x0000…0000",   // hex-encoded 32-byte lower bound
      "range_end":   "0x5555…5555"    // hex-encoded 32-byte upper bound
    },
    ...
  ]
}
```

Field semantics:

- `shard_id` — UUID string form, matching ADR-033 §4 / ADR-042 §1.
- `leader_id` — `NodeId` u64. Maps into the rev-1 `peers[*].id` entries.
- `leader_data_addr` — `host:port` of the leader's native gateway
  (`KISEKI_DATA_ADDR`, default 9100). For S3 307 redirects, the gateway
  derives the redirect host from the same `peers` map at S3 port 9000.
- `range_start` / `range_end` — hex-encoded 32-byte hashed-key bounds,
  matching `NamespaceShardMap.ShardRange` (ADR-033 §4). `range_end =
  0xFF...FF` is the inclusive upper bound for the last shard
  (matches §1 "absorb remainder" rule).

A node that has not yet observed the cluster's shard map (cold start,
control-plane disconnect) returns `shards: []`. Clients treat an empty
shards list as "fall back to seed-only routing"; this preserves rev 1
behavior for partially-bootstrapped clusters.

**Authorization for `/cluster/info`**: the endpoint stays unauthenticated
(this is per ADR-019 "Client resolution"; ADR-008 rev 1 §"Authentication"
applies to the gRPC follow-up only). It only exposes
**operational** shard metadata — counts, leader placements, key ranges,
node addresses — and NOT tenant content. The per-tenant filtering rule
in ADR-042 §4 "GetTopology contract" applies to the gRPC
`GetTopology` follow-up only, not the bootstrap HTTP/JSON endpoint.
Operators that need bootstrap-time tenant isolation gate the metrics
port (9090) at the network boundary, same as today.

### Bootstrap protocol

```
client.connect(seeds, tenant_id):
  for seed in seeds:
      response = http_get("http://{seed}/cluster/info",
                          connect_timeout=2s, read_timeout=2s)
      if response.ok:
          break
  else:
      raise ConnectFailed(all seeds unreachable)

  topology = Snapshot.from_cluster_info_json(response)
  client.topology_cache.replace(topology)

  # Steady-state refresh via gRPC GetTopology (ADR-042 §4 trailing-meta
  # version-mismatch path) takes over from here. The HTTP /cluster/info
  # poll is bootstrap-only; subsequent refreshes are gRPC.
```

### Failure modes / mitigation

- **Stale leader in cache, leader has migrated**: client dials cached
  leader, server returns `LeaderUnavailable` / (Step A:
  `ForwardToLeader{leader=B}`). Client refreshes topology, dials new
  leader. Cap retries at **2 hops**; on cap, propagate
  `LeaderUnavailable` to the caller. (Closes the cascade where every
  cache entry is stale.)
- **Bootstrap-time seed sequence — first seed down**: 2 s connect
  timeout per seed; fall through to next. Three seeds × 2 s = 6 s
  worst-case bootstrap latency. Operators size the seed list to keep
  bootstrap latency bounded.
- **307 storms under leader-election churn** (S3 path, ADR-014):
  mitigated by `Retry-After: 0-50ms` jitter on the 307 response and
  bench-side `curl -L --max-redirs 3`. ADR-014 carries the full
  analysis.
- **`/cluster/info` reports leader=A but A is now follower** (the
  responding node and the leader disagree on leadership): the
  responding node's view is eventually consistent within one Raft
  heartbeat. The client's first request to A reveals the
  mismatch via `LeaderUnavailable` / `ForwardToLeader` and the
  client refreshes. Operators monitor
  `kiseki_native_topology_stale_leader_redirects_total` for sustained
  churn.

### Observability

ADR-008 rev 2 adds **one** new metric to the Prometheus registry:

```
kiseki_native_topology_stale_leader_redirects_total{protocol, tenant}
```

Incremented every time the gateway emits a leader hint (307 for S3,
NotLeader/ForwardToLeader for native) because the caller's request
arrived at a non-leader. `protocol` distinguishes `s3` and `native`.
Alarm at sustained > 20 % of total writes for any tenant.

### Compatibility

- **Forward**: clients on rev 1 (single-seed `connect(seed,
  tenant_id)`) continue to work — the bootstrap `/cluster/info` still
  returns the rev-1 fields; rev-1 clients ignore `shards`. New rev-2
  clients adopt the multi-seed API as a non-breaking extension.
- **Backward**: a rev-2 server's `/cluster/info` is a superset of
  rev-1's; no field is removed. Servers serving an empty `shards: []`
  array (cold-start / unbootstrapped) are honoured by rev-2 clients
  as "fall back to seed-only routing".
