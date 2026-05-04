# ADR-041: Raft Transport — Shard Multiplexing on a Single Node Port

**Status**: Proposed (architect, awaiting adversary gate 1)
**Date**: 2026-05-04
**Deciders**: Architect + domain expert
**Adversarial review**: pending
**Context**: ADR-026 (Raft topology — Strategy A), ADR-033 (initial shard
topology + ratio-floor splits), ADR-034 (shard merge), ADR-036 (LogOps
shard management), ADR-004 (schema versioning), I-L2, I-L11.

## Problem

`kiseki-raft::tcp_transport::run_raft_rpc_server` binds **one TCP
listener per Raft group**:

```rust
pub async fn run_raft_rpc_server<C>(
    addr: &str,
    raft: Arc<Raft<C, ...>>,
    tls_config: Option<...>,
) -> io::Result<()>
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // ... accept loop dispatches to the single `raft` handle ...
}
```

`OpenRaftLogStore::spawn_rpc_server(addr)` calls this once per shard.
Today the runtime creates exactly one shard per node, so the existing
shape works. But ADR-033 §1 prescribes
`initial_shards = max(min(multiplier × node_count, shard_cap),
shard_floor)` — for a 3-node cluster with default `multiplier=3`, the
namespace gets **9 shards** at boot. Calling `spawn_rpc_server(addr)`
nine times on the same address fails with `EADDRINUSE` on the second
call.

ADR-026's "Strategy C" (TiKV-like batched transport) was deferred to
"Phase 3 (100+ nodes)". The 9-shard case shows we hit the
single-port-per-node ceiling at **3 nodes**, not 100.

## Decision

**Single TCP listener per node**, multiplexed by shard via a wire-format
prefix. The listener routes each accepted message to the right `Raft`
instance using a per-node shard registry.

This is ADR-026 Strategy C brought forward — the rationale ("Multi-Raft
with batched transport, TiKV-like") covers exactly this case, and the
ADR explicitly lists it as a future option. The ADR-026 transport
phase table is amended (see §"Phase mapping update" below).

### Wire format

Length-prefixed framed messages. Schema version is the **first byte**
of the framed payload, per ADR-004 ("Format version is the first field
read on every deserialization path").

```
+-------------------+----------+--------------------------------+
| length (4 bytes)  | version  | versioned-payload              |
| big-endian u32    | 1 byte   | (length - 1) bytes             |
+-------------------+----------+--------------------------------+
```

`version = 1`: kiseki-raft 0.x, multi-shard format

```
versioned-payload (v1) =
  json::to_vec( (shard_id_str: String, tag: String, rpc_payload: serde_json::Value) )

where:
  shard_id_str = ShardId.0.to_string()  (RFC-4122 UUID, 36 bytes)
  tag          = "append_entries" | "vote" | "full_snapshot"
  rpc_payload  = openraft request struct, serde_json
```

The pre-existing format was
`json::to_vec( (tag, rpc_payload) )` (no shard_id, no version byte).
Servers running ADR-041 reject pre-ADR-041 messages by version-byte
mismatch — see "Migration" below.

### Server-side API

```rust
// kiseki-raft::tcp_transport (new)

/// Per-node Raft RPC listener. One instance per node, bound to the
/// node's `raft_addr`. Shards register their `Raft` handles into
/// the listener's registry on creation; the accept loop dispatches
/// inbound RPCs to the right shard by routing on the wire-format
/// `shard_id` prefix.
pub struct RaftRpcListener {
    addr: String,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    registry: Arc<DashMap<ShardId, ShardDispatch>>,
}

impl RaftRpcListener {
    pub fn new(addr: String, tls_config: Option<Arc<rustls::ServerConfig>>) -> Self;

    /// Register a shard's `Raft` handle. Idempotent — re-registration
    /// replaces the previous dispatcher (used during membership
    /// changes that swap state machines).
    pub fn register_shard<C, SM>(&self, shard_id: ShardId, raft: Arc<Raft<C, SM>>)
    where
        C: RaftTypeConfig<SnapshotData = Cursor<Vec<u8>>>,
        SM: RaftStateMachine<C> + 'static,
        C::D: Serialize + DeserializeOwned + Send + Sync + 'static,
        C::R: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Remove a shard from the registry. Subsequent RPCs for that
    /// shard get `ShardNotFound`. Used on shard retirement after
    /// ADR-034 merge cutover.
    pub fn unregister_shard(&self, shard_id: ShardId);

    /// Spawn the accept loop. Returns a `JoinHandle`; caller decides
    /// shutdown. One call per node — subsequent calls fail with
    /// `EADDRINUSE` (existing kernel behavior, surfaced unchanged).
    pub async fn run(self) -> io::Result<()>;
}

/// Type-erased per-shard dispatcher. Each `register_shard<C, SM>`
/// builds a closure that captures the typed `Raft<C, SM>` handle and
/// performs the JSON deserialization for that shard's `C`. Different
/// shards on the same listener may have different `C` types in
/// principle; in practice all kiseki shards share the same
/// `kiseki_log::raft::C`.
type ShardDispatch = Arc<
    dyn Fn(&str, Vec<u8>) -> BoxFuture<'static, Vec<u8>> + Send + Sync,
>;
```

The registry is `DashMap` (concurrent map, no global lock) so
inbound RPCs and concurrent register/unregister calls don't serialize.
Lookup is O(1) amortized.

#### Why a closure-based registry, not `Arc<dyn ErasedRaft>`

`Arc<Raft<C, SM>>` cannot be put behind `dyn ErasedRaft` because
`SM: RaftStateMachine<C>` is per-instance and not object-safe.
Closures sidestep this: each closure captures the typed `Raft<C, SM>`
handle, and the `Fn(&str, Vec<u8>) -> BoxFuture<Vec<u8>>` surface is
object-safe. The cost is one extra `Arc::clone` per RPC, which is
free at this scale.

#### Server-side dispatch

```rust
async fn run_accept_loop(self) -> io::Result<()> {
    let listener = TcpListener::bind(&self.addr).await?;
    let tls = self.tls_config.map(TlsAcceptor::from);
    loop {
        let (stream, _peer) = listener.accept().await?;
        let registry = Arc::clone(&self.registry);
        let tls = tls.clone();
        tokio::spawn(async move {
            handle_one_connection(stream, tls, registry).await;
        });
    }
}

async fn handle_one_connection<S>(
    stream: S, tls: Option<TlsAcceptor>, registry: Arc<DashMap<ShardId, ShardDispatch>>,
) {
    // 1. (optional) mTLS handshake — same posture as today (ADV-S2)
    // 2. Read length-prefixed frame
    // 3. Validate length <= MAX_RAFT_RPC_SIZE (existing ADV-S1/S6 guard)
    // 4. Read version byte; reject unknown version with empty response
    // 5. Parse (shard_id, tag, payload) from versioned-payload
    // 6. registry.get(shard_id) -> dispatcher | None
    //    None: write empty 4-byte length response (caller times out)
    // 7. dispatcher(tag, payload).await -> response bytes
    // 8. Write length-prefixed response back
}
```

The empty response on `ShardNotFound` mirrors the existing behavior
on parse errors — caller sees an empty body, treats it as a
transient transport error, retries with backoff. This is correct for
the case where a shard is being created/destroyed concurrently with
in-flight RPCs.

### Client-side API

```rust
// kiseki-raft::tcp_transport (changed)

pub struct RpcClient<C> {
    /// Peer address (single per node, regardless of shard count).
    addr: String,
    /// The shard this client speaks to. Carried in every RPC frame
    /// so the peer's `RaftRpcListener` can route.
    shard_id: ShardId,
    tls_config: Option<Arc<rustls::ClientConfig>>,
    _phantom: PhantomData<C>,
}

impl<C> RpcClient<C> {
    pub fn new(
        addr: String,
        shard_id: ShardId,
        tls_config: Option<Arc<rustls::ClientConfig>>,
    ) -> Self;
}
```

The existing `RaftNetwork` trait impl on `RpcClient<C>` (methods
`append_entries`, `vote`, `transfer_leader`) keeps its surface; only
the wire-frame builder changes to include `version` + `shard_id`.

Construction sites:
- `OpenRaftLogStore::new` builds peer `RpcClient`s — adds `shard_id`
  param.
- `RaftShardStore::create_shard` (inherent, line 109) — passes the
  shard's id.

### Lifecycle

```
Node startup:
  1. Build RaftRpcListener::new(raft_addr, tls)
  2. Spawn listener.run() task
  3. Each shard create:
     a. OpenRaftLogStore::new(...) builds the typed Raft handle
     b. listener.register_shard(shard_id, raft)
  4. Each shard retire (ADR-034 §"grace period"):
     a. listener.unregister_shard(shard_id)
     b. drop the Raft handle (its task exits)

Node shutdown:
  - Listener task exits on drop (or via shutdown signal)
  - Inbound RPCs in flight complete; new connections refused
```

The listener handle is owned by the runtime (one per node) and
threaded through the `RaftShardStore` so per-shard `create_shard`
calls register. Today `OpenRaftLogStore::spawn_rpc_server(addr)` is
called inside `RaftShardStore::create_shard`. After this ADR, that
method is removed; `create_shard` registers with the shared listener
instead.

### Migration

Pre-1.0 status (kiseki has not shipped to external users). One-shot
flag-day cutover is acceptable: nodes running pre-ADR-041 binaries
cannot interoperate with post-ADR-041 binaries. Operators replace
all binaries in a single rolling restart.

The version-byte design preserves the option for a future
ADR-041-amendment to add a v2 wire format that interoperates with v1
(e.g., for RDMA framing). v0 (pre-ADR-041) is permanently
incompatible — it had no version byte.

### Phase mapping update (amends ADR-026)

ADR-026's "Transport" table:

```
| Phase 1 (now)    | TCP + TLS                          | Direct, one per peer |
| Phase 2 (10+ nodes) | TCP + TLS + connection pooling | Reuse across groups |
| Phase 3 (100+ nodes) | Batched transport (Strategy C) | Coalesce heartbeats |
```

Becomes:

```
| Phase 1 (now)    | TCP + TLS, multiplexed per node (ADR-041) | One listener, shard-tagged frames |
| Phase 2 (10+ nodes) | + connection pooling                    | Reuse keep-alive across shards    |
| Phase 3 (100+ nodes) | + batched transport (Strategy C)        | Coalesce heartbeats per node pair |
```

Strategy C's heartbeat batching is now a follow-on optimization on
top of the multiplexed transport, not a parallel design.

### What does NOT change

- **Raft protocol semantics.** openraft sees one `Raft<C, SM>` per
  shard. Election, log replication, snapshots are unchanged. The
  multiplexing is purely transport.
- **mTLS posture.** ADV-S2 (cert-binding to peer identity) is
  enforced at TLS handshake on the single port — same code path,
  just one bind site instead of N.
- **Message size cap.** `MAX_RAFT_RPC_SIZE` still applies. The
  shard_id prefix is a few dozen bytes, well within the cap.
- **Existing failure modes.** ADV-S1 (oversized request) and ADV-S6
  (truncated read) keep their guards; those are pre-frame-parse
  checks that don't care about multiplexing.

### Observability

New metrics, named in the `kiseki_raft_transport_*` family (consistent
with the existing `kiseki_fabric_*` and `kiseki_gateway_*` patterns):

| Metric | Type | Labels | Purpose |
|--------|------|--------|---------|
| `kiseki_raft_transport_rpc_total` | counter | `shard, op, outcome` | per-shard RPC count by `append_entries` / `vote` / `full_snapshot` and `ok` / `unknown_shard` / `parse_error` / `timeout` |
| `kiseki_raft_transport_rpc_duration_seconds` | histogram | `shard, op` | per-shard RPC latency on the server side |
| `kiseki_raft_transport_registry_size` | gauge | — | active shard count on this listener |
| `kiseki_raft_transport_unknown_shard_total` | counter | — | inbound RPCs targeted at a `shard_id` not in the registry (high values during in-flight membership changes; persistently high values indicate stale peer caches) |

Implementation routes through the same `KisekiMetrics` struct as the
gateway/fabric counters; runtime wires them on construction.

## Consequences

### Positive

- Unblocks ADR-033 §1 (multi-shard bootstrap via `compute_initial_shards`).
- Unblocks ADR-033 §3 / ADR-034 (split / merge create new Raft groups
  on the same node).
- Resolves the implicit ADR-026 transport ceiling that surfaced at
  3 nodes — Phase 3 brought forward to Phase 1.
- Single port simplifies network policy / firewall rules — no
  per-shard port allocation.
- Single mTLS handshake per peer-pair connection (caller pools across
  shards in Phase 2).

### Negative

- Breaking wire format. v0 nodes cannot peer with v1 nodes.
- Single-listener throughput ceiling: all shard RPCs serialize
  through the OS accept queue + a single `tokio::spawn` per
  connection. Mitigated by per-connection async tasks (existing
  pattern); revisit at >100 shards/node when DashMap contention or
  TCP accept rate become measurable.
- `unknown_shard` responses are silently empty. A monitoring metric
  is required so persistent stale-cache or split-brain conditions
  are visible.

### Neutral

- Storage layer (kiseki-log) needs no schema change — only the
  transport layer interface shifts.
- TLS configuration unchanged.
- The closure-based dispatcher pattern matches the existing
  `kiseki-chunk-cluster::FabricMetrics::record_op` pattern of
  type-erased recording — established team convention.

## Alternatives considered

### Port-per-shard (rejected)

Each shard binds its own port (e.g., 9300 for shard 1, 9301 for
shard 2, ...). Peers map gains a shard dimension:
`{ "1.shard1": "host:9301", "1.shard2": "host:9302", ... }`.

Rejected because:
- Operationally unfriendly: firewall rules must enumerate ports;
  port exhaustion at scale (64-shard cap × N peers per shard).
- mTLS handshake cost per port pair multiplies by shard count.
- Doesn't compose with future RDMA transport (one queue pair per
  port pair).
- Contradicts ADR-024 (single management network spine) where Raft
  traffic should not consume scarce port-mapping table entries.

### Separate management network for Raft (rejected for now)

ADR-026 §"Network requirements" already permits this as
"belt-and-suspenders isolation" but doesn't require it. A separate
network changes the deployment topology, not the multiplexing
question — even on a dedicated management network the per-port
ceiling problem stays. Multiplexing is orthogonal and required.

### Streaming connection per shard pair (rejected)

Persistent streams between every shard-pair across nodes (as
QUIC or h2 streams) trade RPC handshake cost for stream-management
state. With 64 shards × 100 nodes that's 64×100 = 6400 streams per
node, each with timer state for keepalive. The closure-dispatcher
multiplexing keeps the count at one connection per peer pair (TCP
keepalive, scaled to N pairs not N × shards).

## Implementation guidance for the implementer

The architect role produces structure only (per `roles/architect.md`).
The implementation contract this ADR establishes:

1. New module `kiseki-raft::tcp_transport::listener` exposes
   `RaftRpcListener` per the API in §"Server-side API". The closure
   pattern + DashMap registry is non-negotiable; the public method
   signatures are.
2. `RpcClient<C>` gains a `shard_id: ShardId` field; constructor
   signature changes accordingly. All call sites (`OpenRaftLogStore`,
   `RaftShardStore`) are updated to pass it.
3. `OpenRaftLogStore::spawn_rpc_server` is **removed** — the listener
   is owned by the runtime, not by individual shards.
   `RaftShardStore::new` gains a `RaftRpcListener` handle; its
   `create_shard` calls `listener.register_shard(...)`.
4. Wire-format constants (`RAFT_TRANSPORT_VERSION_V1: u8 = 1`) live
   in `kiseki-raft::tcp_transport` next to the existing
   `MAX_RAFT_RPC_SIZE`.
5. The 5 new metrics from §"Observability" are added to
   `kiseki-server::metrics::KisekiMetrics` and threaded into the
   listener via a `with_metrics(metrics_handle)` builder, following
   the same pattern as `FabricMetrics`.
6. Tests:
   - Unit: parse / serialize each wire-format variant; reject unknown
     version byte; reject oversized frame; accept empty response on
     unknown shard.
   - Integration (`tests/multi_shard_transport.rs`): 3-node ×
     2-shard cluster on one port per node, verify each shard's Raft
     group reaches quorum independently and `append_delta` to one
     shard does not appear in the other.
7. Adversary review focus:
   - Stale shard_id in client cache after merge (ADR-034 grace
     period) — does the empty-response path cause caller hangs?
   - Registry contention under concurrent membership change —
     DashMap fairness?
   - mTLS replay on a deregistered-then-reregistered shard — is
     there a generation counter needed?

## References

- ADR-026: Raft Topology — Strategy A (this ADR amends the Transport
  phase mapping)
- ADR-033: Initial Shard Topology + Ratio-Floor Splits
- ADR-034: Shard Merge
- ADR-036: LogOps Trait — Shard Management
- ADR-004: Schema Versioning and Rolling Upgrades (wire-format
  versioning convention)
- TiKV Multi-Raft transport batching: <https://tikv.org/deep-dive/scalability/multi-raft/>
- openraft `RaftNetwork` trait: <https://docs.rs/openraft/latest/openraft/network/>
