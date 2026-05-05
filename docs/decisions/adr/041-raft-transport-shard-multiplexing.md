# ADR-041: Raft Transport — Shard Multiplexing on a Single Node Port

**Status**: Accepted (gate-1 amendment)
**Date**: 2026-05-04 (initial), 2026-05-04 amendment after gate 1
**Deciders**: Architect + domain expert
**Adversarial review**: 2026-05-04 (3H 5M 7L; HIGH findings addressed in this
amendment, see §"Gate-1 amendments" at the bottom)
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

#### Request frame

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

#### Response frame (post-gate-1)

```
+-------------------+--------------+--------------------------------+
| length (4 bytes)  | status (1B)  | response-body                  |
| big-endian u32    |              | (length - 1) bytes             |
+-------------------+--------------+--------------------------------+
```

| `status` | meaning | `response-body` |
|---|---|---|
| `0x00` | OK — dispatcher returned a response | typed openraft response, serde_json |
| `0x01` | `unknown_shard` — registry has no entry for `shard_id` | empty |
| `0x02` | `parse_error` — request was malformed at version/shard/tag | empty |
| `0x03` | `dispatcher_panic` — dispatcher panicked; listener stayed up | empty |

The response status byte (added after gate-1 finding F-H2) lets the
client distinguish "shard retired" from "transport error" so the
caller can invalidate its `NamespaceShardMap` cache. Without this,
empty responses indistinguishably mapped to `RPCError::Network` and
openraft retried indefinitely against a retired shard during ADR-034's
5-minute grace period — see §"Gate-1 amendments" / F-H2 below.

#### Reserved version-byte values

The pre-existing format was
`json::to_vec( (tag, rpc_payload) )` — no version byte, no shard_id.
The first byte of a pre-ADR-041 frame is the start of a JSON value,
typically `0x5b` (`[`) or, less commonly, `0x7b` (`{`) or `0x22` (`"`).

**Reserved (never assignable to future versions)**: `0x5b`, `0x7b`,
`0x22`. A future ADR-041 amendment introducing v2/v3/etc. must skip
these values so a v0 (pre-ADR-041) frame can never be misread as a
known version. This applies to the request frame's version byte
**and** the response frame's status byte (a stale pre-ADR-041 server
sends a response without status; reserving these bytes lets a v1
client detect-and-fail rather than misinterpret).

Servers running ADR-041 reject pre-ADR-041 messages by version-byte
mismatch (any of the reserved bytes → `parse_error`) — see "Migration"
below.

### Server-side API

The server side is split into two cooperating types — a
`RegistryHandle` (clonable, used by shard owners for
register/unregister) and a `RaftRpcListener` (consumed by the accept
loop). The split is required because `run(self)` consumes the
listener while shards still need to register/unregister after the
listener is spawned (gate-1 finding F-H1).

```rust
// kiseki-raft::tcp_transport (new)

/// Clonable handle to the per-node shard registry. Each shard's
/// owner (typically `RaftShardStore::create_shard`) holds one and
/// calls `register_shard` / `unregister_shard` over the lifetime
/// of the shard.
#[derive(Clone)]
pub struct RegistryHandle {
    inner: Arc<DashMap<ShardId, ShardDispatch>>,
    metrics: Option<Arc<RaftTransportMetrics>>,
}

impl RegistryHandle {
    /// Register a shard's `Raft` handle. Idempotent — re-registration
    /// replaces the previous dispatcher (used during membership
    /// changes that swap state machines).
    ///
    /// # Bounds (gate-1 F-L4)
    /// `Send + Sync + 'static` is required on `Raft<C, SM>` so the
    /// closure can be stored in `Arc<dyn Fn …>`. openraft's
    /// `Raft<C, SM>` satisfies this when `SM` is the typical
    /// `Arc<Mutex<…>>`-backed state machine.
    pub fn register_shard<C, SM>(&self, shard_id: ShardId, raft: Arc<Raft<C, SM>>)
    where
        C: RaftTypeConfig<SnapshotData = Cursor<Vec<u8>>> + Send + Sync + 'static,
        SM: RaftStateMachine<C> + Send + Sync + 'static,
        C::D: Serialize + DeserializeOwned + Send + Sync + 'static,
        C::R: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Remove a shard from the registry. Subsequent RPCs for that
    /// shard get response `status = 0x01 unknown_shard`. Used on
    /// shard retirement after ADR-034 merge cutover.
    ///
    /// **Best-effort prompt**: the registry entry is removed
    /// immediately, but any dispatch already in flight (with its
    /// own `Arc::clone` of the closure) keeps the closure alive
    /// until the dispatch completes. ADR-034's 5-minute grace
    /// period vastly exceeds any single RPC duration so this is
    /// observationally synchronous in practice (gate-1 F-L2).
    pub fn unregister_shard(&self, shard_id: ShardId);
}

/// Per-node Raft RPC listener. One instance per node, bound to the
/// node's `raft_addr`. The listener owns the accept loop only; the
/// shard registry is held via `RegistryHandle` and can be cloned
/// before `run()` consumes the listener.
pub struct RaftRpcListener {
    addr: String,
    tls_config: ArcSwap<Option<TlsAcceptor>>,
    registry: RegistryHandle,
    metrics: Option<Arc<RaftTransportMetrics>>,
}

impl RaftRpcListener {
    pub fn new(addr: String, tls_config: Option<Arc<rustls::ServerConfig>>) -> Self;

    /// Get a clonable handle to the shard registry. Callers MUST
    /// obtain this BEFORE invoking `run()` — afterwards the listener
    /// is moved into the spawned task and only the handle remains.
    pub fn registry(&self) -> RegistryHandle;

    /// Hot-rotate the TLS context (gate-1 F-L3). New connections
    /// after this call use the new acceptor; in-flight handshakes
    /// finish on the old one. No rebind required.
    pub fn set_tls_acceptor(&self, new_acceptor: Option<Arc<rustls::ServerConfig>>);

    /// Attach the metrics struct so per-shard RPC counts +
    /// durations + registry size + restart count get exported.
    /// Called once at startup; the metrics handle lives in
    /// `KisekiMetrics`.
    pub fn with_metrics(self, metrics: Arc<RaftTransportMetrics>) -> Self;

    /// Spawn the accept loop. Returns a `JoinHandle`; caller decides
    /// shutdown. One call per node — subsequent calls fail with
    /// `EADDRINUSE`.
    pub async fn run(self) -> io::Result<()>;

    /// Run with supervisor: restart the accept loop on panic with
    /// jittered backoff (gate-1 F-H3). Bounded retry budget — after
    /// 10 panics in 60s, returns `Err`. Increment
    /// `kiseki_raft_transport_listener_restarts_total` on each
    /// restart.
    ///
    /// Prefer this over `run` in production. Tests that want
    /// deterministic crash behavior call `run` directly.
    pub async fn run_supervised(self) -> io::Result<()>;
}

/// Type-erased per-shard dispatcher. Each `register_shard<C, SM>`
/// builds a closure that captures the typed `Raft<C, SM>` handle and
/// performs the JSON deserialization for that shard's `C`. Different
/// shards on the same listener may have different `C` types in
/// principle; in practice all kiseki shards share the same
/// `kiseki_log::raft::C`.
type ShardDispatch = Arc<
    dyn Fn(&str, Vec<u8>) -> BoxFuture<'static, DispatchOutcome> + Send + Sync,
>;

/// Result of dispatching a single inbound RPC. The status byte on
/// the wire response is built from this — `Ok(bytes)` → `0x00`,
/// `ParseError` → `0x02`, `Panicked` → `0x03`. `unknown_shard` is
/// produced at the registry layer (no dispatcher to call), not here.
enum DispatchOutcome {
    Ok(Vec<u8>),
    ParseError,
    Panicked,
}
```

The registry is `DashMap` (concurrent map, no global lock) so
inbound RPCs and concurrent register/unregister calls don't serialize.
Lookup is O(1) amortized.

#### Runtime placement (gate-1 F-M1)

The listener and the `Raft<C, SM>` instances it dispatches to **MUST
run on the same tokio runtime**. openraft methods (`append_entries`,
`vote`, `install_full_snapshot`) `tokio::spawn` internal tasks onto
the ambient runtime; calling them from a different runtime can panic.

In kiseki, the runtime that owns the shards is
`RaftShardStore::rt` (the dedicated Raft runtime).
`RaftRpcListener::run` is therefore spawned **on `RaftShardStore::rt`**
via `rt.handle().spawn(listener.run())`. The server's main runtime
never touches the listener.

This pin is enforced by `RaftShardStore` building the listener
internally (the runtime is the constructor's responsibility) — the
public API does not let callers spawn `run` on a foreign runtime.

#### Why a closure-based registry, not `Arc<dyn ErasedRaft>`

`Arc<Raft<C, SM>>` cannot be put behind `dyn ErasedRaft` because
`SM: RaftStateMachine<C>` is per-instance and not object-safe.
Closures sidestep this: each closure captures the typed `Raft<C, SM>`
handle, and the `Fn(&str, Vec<u8>) -> BoxFuture<DispatchOutcome>`
surface is object-safe. The cost is one extra `Arc::clone` per RPC,
which is free at this scale.

#### Per-task panic isolation (gate-1 F-H3)

Every per-connection task is wrapped in
`tokio::task::JoinHandle::is_panic` detection: a panic in dispatch
returns `DispatchOutcome::Panicked` (status `0x03`) and the listener
keeps accepting. Without this, a single malformed-payload-induced
panic in one shard's dispatcher would propagate up through
`tokio::spawn`'s unwind and (depending on the runtime's
`panic_handler`) potentially abort the whole listener task.

```rust
async fn handle_one_connection<S>(
    stream: S,
    tls: Option<TlsAcceptor>,
    registry: RegistryHandle,
    metrics: Option<Arc<RaftTransportMetrics>>,
) {
    // 1. (optional) mTLS handshake — same posture as today (ADV-S2)
    // 2. Read length-prefixed frame
    // 3. Validate length <= MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED
    //    (gate-1 F-M3 — leave headroom for snapshots near the cap)
    // 4. Read version byte; if reserved (0x5b/0x7b/0x22) or unknown,
    //    write status=0x02 parse_error and return
    // 5. Parse (shard_id, tag, payload) from versioned-payload
    // 6. registry.inner.get(shard_id) -> dispatcher | None
    //    None: write status=0x01 unknown_shard, increment
    //    `kiseki_raft_transport_unknown_shard_total`
    // 7. Wrap dispatcher call in catch_unwind:
    //      Ok(bytes) -> status=0x00, body=bytes
    //      ParseError -> status=0x02, empty body
    //      Panicked -> status=0x03, empty body, log + metric
    // 8. Write length-prefixed response back
}
```

#### Per-peer connection cap (gate-1 F-M5)

The accept loop tracks active connection count per peer cert
fingerprint in a small `DashMap<CertFingerprint, AtomicU32>`. New
connections from a peer over `RAFT_TRANSPORT_PER_PEER_MAX = 16` are
closed immediately after TLS handshake (so the cert is verified
before the cap kicks in — mTLS-trusted peers see the cap, untrusted
peers fail at handshake regardless). The cap is conservative:
openraft requires only one connection per peer for in-flight RPCs;
16 leaves headroom for transient overlap during failover.

Cap exceedance increments
`kiseki_raft_transport_connection_cap_exceeded_total{peer}` so
operators see attack patterns or runaway client bugs.

#### Slow-dispatch backpressure (gate-1 F-M2)

Dispatcher closures **MUST** route operations expected to take >1 ms
through `tokio::task::spawn_blocking`. Specifically:

- `install_full_snapshot` — large state machine apply, definitely
  blocking;
- `append_entries` with batches >256 entries or any inline payload
  that decodes >1 MiB.

`vote` and `append_entries` for small heartbeat batches stay on the
async path. The implementer documents the per-tag policy in
`tcp_transport.rs` next to the dispatcher closure builder.

Without this, a slow snapshot on one shard occupies a worker thread
for the full transfer duration. Concurrent snapshots on multiple
shards would otherwise exhaust the runtime's worker pool and starve
fast `append_entries` to other shards.

#### Snapshot framing headroom (gate-1 F-M3)

`MAX_RAFT_RPC_SIZE = 128 MiB` is the absolute frame cap.
`WIRE_FRAME_OVERHEAD_RESERVED = 1 KiB` reserves headroom so a
snapshot built right at the cap doesn't tip over once the version
byte + shard_id + tag prefix is added. The snapshot-builder in
`kiseki-log::raft::state_machine::ShardStateMachine::build_snapshot`
caps its output at `MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED`
and a unit test pins the bound (a snapshot at the cap fits the
frame including prefix).

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

#### Response status handling (gate-1 F-H2)

The `RaftNetwork` trait expects `Result<Response, RPCError>` — a
single failure variant set inherited from openraft. To plumb the
new `unknown_shard` signal, `to_rpc_error` is extended to map
status byte → typed `RPCError`:

| Wire status | `to_rpc_error` mapping | Caller treatment |
|---|---|---|
| `0x00 ok` | `Ok(parse_response_body)` | normal path |
| `0x01 unknown_shard` | `Err(RPCError::Network(NetworkError::ShardRetired))` | `kiseki_log` interceptor catches `ShardRetired`, fires `NamespaceShardMap` cache refresh, then propagates to openraft as a normal `Network` error so its retry/backoff still applies |
| `0x02 parse_error` | `Err(RPCError::Network(NetworkError::ProtocolMismatch))` | wedge on cluster-version mismatch — operator alert |
| `0x03 dispatcher_panic` | `Err(RPCError::Network(NetworkError::ServerPanic))` | log + metric; openraft retries (a single panic likely transient) |

The `NetworkError` is a kiseki-defined wrapper carrying these
sub-variants; openraft sees a single `RPCError::Network`. The
`kiseki_log::raft` layer wraps `RpcClient<C>` in an interceptor
that inspects the wrapped `NetworkError` BEFORE handing back to
openraft:

```rust
async fn append_entries_intercepted(...)
    -> Result<AppendEntriesResponse<C>, RPCError<C>>
{
    let res = inner_rpc_client.append_entries(req).await;
    if let Err(RPCError::Network(NetworkError::ShardRetired)) = &res {
        // Trigger NamespaceShardMap cache refresh for THIS shard's
        // namespace — the route is stale.
        shard_map_cache.invalidate(self.shard_id);
    }
    res.map_err(|e| e.into())  // unwrap NetworkError to plain Network
}
```

This closes the F-H2 gap: heartbeat-only paths now trigger cache
refresh (previously only the write path's `KeyOutOfRange` did, leaving
ADR-034's 5-minute grace period as 5 minutes of wasted heartbeat
traffic against retired shards).

Construction sites:
- `OpenRaftLogStore::new` builds peer `RpcClient`s — adds `shard_id`
  param.
- `RaftShardStore::create_shard` (inherent, line 109) — passes the
  shard's id.
- The cache-invalidation hook is plumbed via a
  `Arc<dyn ShardMapCache>` slot on `OpenRaftLogStore` (None for
  single-node tests; Some in production wiring).

### Lifecycle

```
Node startup:
  1. RaftShardStore::new(addr, peers, ...) constructs and builds
     the dedicated Raft tokio runtime
  2. Build RaftRpcListener::new(raft_addr, tls).with_metrics(m)
  3. registry_handle = listener.registry()  // BEFORE consuming the listener
  4. RaftShardStore stores `registry_handle` for use in create_shard
  5. RaftShardStore::rt.handle().spawn(listener.run_supervised())
     — runs on the SAME runtime that owns the Raft instances
       (gate-1 F-M1)
     — supervisor restarts on panic with bounded retry budget
       (gate-1 F-H3)
  6. Each shard create:
     a. OpenRaftLogStore::new(...) builds the typed Raft handle
     b. registry_handle.register_shard(shard_id, raft)
  7. Each shard retire (ADR-034 §"grace period"):
     a. registry_handle.unregister_shard(shard_id)
     b. drop the Raft handle (its task exits)
     c. peer caches refresh on next RPC via the F-H2 status path

Node shutdown:
  - Drop RaftShardStore: runtime drops, listener task aborts
  - Inbound RPCs in flight finish-or-cancel via runtime drop
```

`OpenRaftLogStore::spawn_rpc_server` is **removed**. The listener
is owned by `RaftShardStore`, not by individual shards. Shards
register their `Raft` handles via the `RegistryHandle` clone they
inherit from the store.

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
| `kiseki_raft_transport_rpc_total` | counter | `shard, op, outcome` | per-shard RPC count by `append_entries` / `vote` / `full_snapshot` and `ok` / `unknown_shard` / `parse_error` / `dispatcher_panic` |
| `kiseki_raft_transport_rpc_duration_seconds` | histogram | `shard, op` | per-shard RPC latency on the server side |
| `kiseki_raft_transport_registry_size` | gauge | — | active shard count on this listener |
| `kiseki_raft_transport_unknown_shard_total` | counter | — | inbound RPCs targeted at a `shard_id` not in the registry (high values during in-flight membership changes; persistently high values indicate stale peer caches) |
| `kiseki_raft_transport_listener_restarts_total` | counter | — | supervisor restarts on listener panic (gate-1 F-H3); should stay 0 in steady state |
| `kiseki_raft_transport_dispatcher_panic_total` | counter | `shard, op` | per-task panics caught by the catch_unwind wrapper; per-shard so a misbehaving Raft instance is identifiable |
| `kiseki_raft_transport_connection_cap_exceeded_total` | counter | `peer` | per-peer connection cap exceedances (gate-1 F-M5) |
| `kiseki_raft_transport_active_connections` | gauge | — | current accepted-connection count on this listener (DoS visibility — gate-1 F-M5) |

**Per-shard label cardinality** (gate-1 F-L6): the `shard` label is
bounded by ADR-033 §1's `shard_cap` (default 64). At the cap, total
series is 64 × 3 ops × 4 outcomes = 768 per node — well within
Prometheus budget. Future amendments raising `shard_cap` past ~250
should drop the `shard` label (aggregate-only) to stay under the
typical 10k-series-per-node guideline.

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
   `RaftRpcListener` + `RegistryHandle` per the API in §"Server-side
   API". The split (handle clonable for register/unregister, listener
   consumed by `run`) is non-negotiable — gate-1 F-H1.
2. `RpcClient<C>` gains a `shard_id: ShardId` field; constructor
   signature changes accordingly. All call sites (`OpenRaftLogStore`,
   `RaftShardStore`) are updated to pass it.
3. `OpenRaftLogStore::spawn_rpc_server` is **removed**.
   `RaftShardStore::new` constructs the `RaftRpcListener`,
   `spawn`s its `run_supervised()` on the dedicated Raft runtime
   (gate-1 F-M1), and stores the cloned `RegistryHandle` for use in
   `create_shard`.
4. Wire-format constants live in `kiseki-raft::tcp_transport` next
   to the existing `MAX_RAFT_RPC_SIZE`:
   - `RAFT_TRANSPORT_VERSION_V1: u8 = 1`
   - `WIRE_FRAME_OVERHEAD_RESERVED: usize = 1024`
   - `RAFT_TRANSPORT_PER_PEER_MAX: u32 = 16`
   - `RESERVED_VERSION_BYTES: [u8; 3] = [0x5b, 0x7b, 0x22]` —
     permanently unassignable (gate-1 F-L1)
5. Response status byte mapping in code matches the table in
   §"Response frame": `0x00 ok`, `0x01 unknown_shard`,
   `0x02 parse_error`, `0x03 dispatcher_panic`.
6. The 8 new metrics from §"Observability" are added to
   `kiseki-server::metrics::KisekiMetrics` and threaded into the
   listener via `RaftRpcListener::with_metrics`, following the same
   pattern as `FabricMetrics`.
7. Snapshot builder
   (`kiseki-log::raft::state_machine::ShardStateMachine::build_snapshot`)
   caps output at `MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED`
   (gate-1 F-M3). A unit test exercises a snapshot-at-cap and verifies
   the framed wire fits.
8. Affected tests requiring migration in the same change (gate-1
   F-M4):
   - `crates/kiseki-log/tests/multi_node_raft.rs` — uses
     `OpenRaftLogStore::spawn_rpc_server` directly (line 62 and
     similar). Migrate to constructing `RaftRpcListener` + reusing
     the existing 3-node fixture.
   - `crates/kiseki-log/tests/raft_shard_store_topology.rs` — uses
     `RaftShardStore::create_shard` with `Some(addr)` for the seed
     shard. After the refactor, addr is owned by the listener inside
     `RaftShardStore`; the create_shard inherent signature changes
     accordingly.
   - `crates/kiseki-log/tests/openraft_integration.rs` — likely
     similar patterns; verify with grep before commit.
   - `crates/kiseki-server/src/runtime.rs` — production wiring; keep
     consistent with the test changes.
9. New integration test
   (`crates/kiseki-log/tests/multi_shard_transport.rs`): 3-node ×
   2-shard cluster on one port per node, verify each shard's Raft
   group reaches quorum independently and `append_delta` to one
   shard does not appear in the other. Also exercises:
   - The F-H2 status-byte path: deregister shard mid-test, verify
     a peer's `append_entries` returns `ShardRetired` and the
     cache-invalidation hook fires.
   - The F-H3 supervisor: inject a panic via a test-only debug
     hook; verify the listener restarts and metrics tick.
10. ADR-034's merge cutover step now calls
    `registry_handle.unregister_shard(retired_shard_id)` — link the
    ADR's "post-cutover state" to this transport-level operation in
    the implementation comment.

### Test coverage matrix for gate-1 findings

| Finding | Test location | What to assert |
|---------|---------------|----------------|
| F-H1 | `multi_shard_transport.rs::registry_clone_register_after_run` | clone registry, spawn run, register a shard from the clone — compiles + dispatches correctly |
| F-H2 | `multi_shard_transport.rs::unregistered_shard_returns_typed_status` | peer sees `RPCError::Network(NetworkError::ShardRetired)`; cache invalidation hook fires |
| F-H3 | `multi_shard_transport.rs::listener_restarts_on_panic` | inject panic → metric ticks → next RPC succeeds |
| F-M1 | `multi_shard_transport.rs::cross_runtime_dispatch_works` | construct listener on Raft runtime; assert no `block_on inside runtime` panic |
| F-M2 | `multi_shard_transport.rs::slow_snapshot_does_not_block_other_shards` | one shard installs full snapshot; concurrent `append_entries` to a 2nd shard completes within p99 < 50ms |
| F-M3 | `tcp_transport::tests::snapshot_at_cap_fits_with_prefix` | build a snapshot at `MAX - WIRE_FRAME_OVERHEAD_RESERVED`; verify the framed wire ≤ MAX |
| F-M4 | (compile + existing tests) | tests that previously called `spawn_rpc_server` migrated; CI green |
| F-M5 | `multi_shard_transport.rs::per_peer_connection_cap_enforced` | 17 connections from same peer cert; 17th is closed; metric ticks |
| F-L1 | `tcp_transport::tests::reserved_version_bytes_rejected_as_parse_error` | byte 0x5b/0x7b/0x22 → status 0x02 |
| F-L4 | (compile-time) | `Raft<C, SM>` bounds satisfied by openraft's typical types |

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

## Gate-1 amendments (2026-05-04)

Adversary gate-1 review surfaced 3 HIGH, 5 MEDIUM, 7 LOW findings.
This amendment addresses every HIGH and MEDIUM, plus the two LOW
findings that materially affect the spec
(`specs/findings/2026-05-04-adv-gate1-adr041-findings.md`).

| Finding | Severity | Resolution |
|---------|----------|------------|
| F-H1 | High | Split listener (`RaftRpcListener`, consumed by `run`) from registry (`RegistryHandle`, clonable). `register_shard`/`unregister_shard` move to the handle. See §"Server-side API". |
| F-H2 | High | Response frame gains a status byte; `0x01 unknown_shard` plumbs to `RPCError::Network(NetworkError::ShardRetired)` and triggers a `NamespaceShardMap` cache refresh on the caller side. See §"Response frame" + §"Response status handling". |
| F-H3 | High | New `run_supervised()` wraps the accept loop in a panic-tolerant supervisor with bounded retry budget. Per-task `catch_unwind` so a panicking dispatcher returns `0x03 dispatcher_panic` without taking down the listener. New metric `kiseki_raft_transport_listener_restarts_total`. See §"Per-task panic isolation". |
| F-M1 | Medium | New §"Runtime placement" pins that the listener and Raft instances run on the same runtime. `RaftShardStore` constructs the listener internally so callers can't violate this. |
| F-M2 | Medium | New §"Slow-dispatch backpressure" mandates `spawn_blocking` for >1 ms operations (snapshots, large append batches). |
| F-M3 | Medium | New constant `WIRE_FRAME_OVERHEAD_RESERVED = 1024` reserves headroom; snapshot builder caps at `MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED`. |
| F-M4 | Medium | §"Implementation guidance" item 8 enumerates the test files requiring migration in the same change. |
| F-M5 | Medium | New §"Per-peer connection cap" mandates per-cert-fingerprint cap of 16 active connections. New metric `kiseki_raft_transport_connection_cap_exceeded_total`. |
| F-L1 | Low | §"Reserved version-byte values" subsection pins `0x5b`, `0x7b`, `0x22` as permanently unassignable. |
| F-L2 | Low | `unregister_shard` doc clarifies "best-effort prompt" — closure stays alive until in-flight dispatches complete. |
| F-L3 | Low | New `RaftRpcListener::set_tls_acceptor` for hot rotation without rebind. |
| F-L4 | Low | Generic bounds in `register_shard` made explicit: `C: Send + Sync + 'static`, `SM: Send + Sync + 'static`. |
| F-L5 | Low | Documented in this section: shard_id reuse is permitted; openraft's term mechanism provides epoch protection. Optional metric `kiseki_raft_transport_stale_term_total` deferred as a non-blocking observability follow-up. |
| F-L6 | Low | §"Observability" pins per-shard label cardinality bound to `shard_cap` (default 64); future raises past ~250 shards drop the per-shard label. |
| F-L7 | Low | Subsumed by F-H2's resolution — the typed `unknown_shard` response makes startup-race RPCs harmless (caller refreshes cache). |

The structural direction was confirmed sound by the gate-1 review;
the amendments above address concrete failure modes without changing
the multiplexing decision itself.
