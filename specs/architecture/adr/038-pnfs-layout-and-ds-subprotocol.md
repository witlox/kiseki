# ADR-038: pNFS Layout and Data Server Subprotocol

**Status**: Proposed (rev 2 — addresses ADV-038-1/2/3/7)
**Date**: 2026-04-27
**Deciders**: Architect (diamond workflow: architect → adversary gate 1 → implementer)
**Context**: Phase 14 closed; perf-cluster spin-up gated on pNFS completeness (ADR-013, RFC 5661, RFC 8435, RFC 9289)

## Revision history

- rev 1 (2026-04-27): initial proposal. Returned by adversary with 4 blocking findings.
- rev 2 (2026-04-27): NFS-over-TLS as default + audited plaintext fallback for older kernels; explicit fh4 MAC field encoding; introduced TopologyEvent bus (§D10) and Phase 15d; layout cache eviction explicit (§D11) with new I-PN8; reconciled `expiry_ms` throughout.
- **§D1 layout encoding superseded by ADR-039 (2026-04-29)**:
  Phase 15c.5 → 15c.10 found that the "per-stripe `nfsv4_1_file_layout_ds_addr4`-style
  mirror list" interpretation rendered as N segments × 1 mirror
  doesn't work with Linux 6.x flex-files driver. The corrected
  encoding is one segment × N mirrors per RFC 8435 §13.2 — see
  ADR-039 for the full revision.

## Problem

`crates/kiseki-gateway/src/pnfs.rs` and `op_layoutget` in
`crates/kiseki-gateway/src/nfs4_server.rs:632-695` define a
`LayoutManager` that round-robins 1-MiB stripes across storage nodes,
but the wire result is non-conformant and unusable by real pNFS
clients:

1. **Layout body malformed.** RFC 5661 §13.5 mandates a structured
   `nfsv4_1_file_layout4` body (deviceid + util + first-stripe-index +
   pattern-offset + fh_list). Current code writes an opaque string of
   the device address (line 692). Linux `nfs4` driver rejects this
   silently and falls back to MDS-only I/O.
2. **`GETDEVICEINFO` (op 47) is absent.** Without it, even a
   well-formed layout cannot be resolved to a network address. Linux
   pNFS clients require this op before issuing any DS I/O.
3. **No DS subprotocol on storage nodes.** The peer addresses passed
   in `runtime.rs:375-383` point at full MDS endpoints
   (`nfs.kiseki:2049`), not a DS-only listener. A direct mount to a
   peer would route through that peer's MDS state machine — no
   acceleration, plus state-leak risk (cross-MDS layout-stateid
   confusion).

The perf benchmark notes the gap (`infra/gcp/benchmarks/perf-suite.sh:184`)
and silently falls back to plain NFSv4.2.

We need a structurally correct pNFS implementation before perf-cluster
spend produces meaningful pNFS numbers (target: ≥ 1.5× single-MDS read
throughput at ≥ 3 storage nodes, RFC 5661 §13.10 expected speedup).

## Decision

### D1. Layout type: **Flexible Files Layout (RFC 8435), not File Layout**

| Aspect | File Layout (RFC 5661 §13) | Flexible Files (RFC 8435) | Choice |
|---|---|---|---|
| DS protocol | NFSv4.1 with file-layout extensions | Any NFS version + URI per stripe | **FFL** |
| Chunked content-addressed backend fit | Forces DS to expose per-file fh4 | Per-stripe `nfl_uri` lets DS handle resolution internally | **FFL** |
| Linux client support (≥ 5.4) | Yes | Yes | tie |
| State complexity (MDS owns layout-statid + DS owns open-statid) | Both | Both, but FFL allows tight-coupled mode (`tightly_coupled = true`, MDS owns all state) | **FFL tight-coupled** |
| Failure: DS unreachable | Client fallback per RFC §13.7 | Client fallback per RFC 8435 §6 (also reports via `ff_iostats4`) | tie (FFL gives us telemetry) |
| Encryption | Per-DS — DS sees ciphertext or plaintext | Per-DS, but tight-coupled MDS controls — DS reads plaintext via `GatewayOps::read` | **FFL** matches existing path |

**Consequence**: layout body is `ff_layout4` (RFC 8435 §5.1).
Each stripe carries an `nfsv4_1_file_layout_ds_addr4`-style mirror
list with a single mirror (no client-side mirroring; replication is
handled below the gateway by Raft+EC).

Block layout (RFC 5663) and SCSI layout (RFC 8154) are rejected:
chunks are content-addressed and encrypted, not raw block extents.

### D2. DS hosting: **co-located with `kiseki-server` on a separate listener**

A new listener `ds_addr` is added to `kiseki-server` config (default
port `2052`, separate from MDS port `2049` and S3 `9000`). Each
storage node runs both an MDS and a DS endpoint; clients direct
LAYOUTGET to any node (which becomes their MDS), and that MDS
fans-out stripe references to all nodes' DS listeners.

Rejected alternative — running DS inside the existing MDS listener
on port 2049: violates RFC 8435 §2.1 ("each DS is independently
addressable") and breaks Linux client connection-tracking
(client opens distinct sessions per DS endpoint).

Rejected alternative — running DS in `kiseki-gateway` only (gateway
crate hosts both MDS and DS): wastes one network hop, and
defeats the purpose of pNFS (clients are supposed to bypass MDS for
data, not hop through another gateway).

### D3. State ownership: **MDS-authoritative, DS stateless**

Tight-coupled mode (`ff_flags4` includes `FF_FLAGS_NO_LAYOUTCOMMIT`
and `tightly_coupled=true`):

- **MDS owns**: layout-statid, open-statid, lock-statid, byte-range
  open state. All recovery on MDS restart goes through standard
  NFSv4.1 session reclaim (NFS4ERR_STALE_STATEID → client re-opens →
  fresh LAYOUTGET).
- **DS owns**: nothing persistent. A DS receives `(fh4, offset, length)`,
  decodes the fh4 into `(tenant_id, namespace_id, composition_id, byte_range)`,
  calls `GatewayOps::read`/`write`, and returns. No DS-side opens, no
  DS-side stateid table, no DS-side lock table.

This means **a DS is a stateless content service**. Crash recovery
of a DS is a no-op (subsequent client RPC re-enters with the same
fh4). Crash recovery of an MDS uses existing NFSv4.1 session
reclaim — no new mechanism.

### D4. Authentication: **NFS-over-TLS (RFC 9289) by default, audited plaintext fallback, plus tenant-scoped fh4**

#### D4.1 Transport authentication

Both the MDS NFS listener (`nfs_addr`) and the DS NFS listener
(`ds_addr`) terminate **mTLS using the existing Cluster CA**
(ADR-009) and the existing `kiseki_transport::TlsConfig::server_config`
machinery already used by the gRPC data path. This is RFC 9289
NFS-over-TLS using `xprtsec=mtls`.

Implementation is straightforward: replace the
`std::net::TcpListener` in `crates/kiseki-gateway/src/nfs_server.rs`
with a TLS-wrapping listener built from the same cert/key/CA bundle
already loaded at boot in `runtime.rs`. **No new crypto code**, no
new key material, no new trust roots.

**Client kernel requirement**: NFS-over-TLS lands in mainline Linux
kernel 6.5 (Aug 2023) and stabilizes in 6.7 (Jan 2024). pNFS+TLS
specifically is solid as of 6.7+. As of 2026, mainstream distros
covering this floor are: RHEL/Rocky 9.5+, Ubuntu 24.04 LTS, Debian 13,
SUSE Leap 15.6+. Kernels older than 6.5 cannot mount with `xprtsec=`.

#### D4.2 Plaintext fallback (opt-in, audited)

For deployments on older kernels that cannot honor `xprtsec=`, the
operator may opt into plaintext NFS by setting **both**:

```toml
# In server config
[security]
allow_plaintext_nfs = true   # default false

# AND environment variable at process start
KISEKI_INSECURE_NFS=true
```

Requiring both prevents accidental enablement (a config-file leak or
an env-var typo alone is insufficient). When enabled, kiseki-server
**MUST**:

1. Emit a STARTUP banner: `WARN: NFS path is PLAINTEXT — fh4s and
   data are observable on the network. Mitigations: VPC isolation,
   firewall ingress restrictions. Compliance: this configuration
   violates I-PN7-default and is acceptable only with documented
   compensating controls.`
2. Emit an audit event `SecurityDowngradeEnabled{reason="plaintext_nfs",
   admin_signature=...}` to the cluster audit shard at every boot
   (not once — every boot).
3. Halve the layout TTL from 300s → 60s in plaintext mode (compensates
   for fh4 replay window — see threat model below).
4. Reject the configuration if `tenant_count > 1` for any namespace
   served (plaintext is single-tenant only).

The fallback is **explicitly accepted security risk**, not a
shortcut. It is intended for: dev clusters, single-tenant private
labs, perf-bench clusters on isolated VPCs.

#### D4.3 fh4 construction and MAC encoding

The fh4 is a MAC'd, expiring bearer token. Construction:

```
PnfsFileHandle (60 bytes total before MAC):
  tenant_id_bytes      : 16  (raw uuid::Uuid bytes, RFC 4122 byte order)
  namespace_id_bytes   : 16  (raw uuid::Uuid bytes)
  composition_id_bytes : 16  (raw uuid::Uuid bytes)
  stripe_index_be      :  4  (big-endian u32)
  expiry_ms_be         :  8  (big-endian u64, ms since Unix epoch)

mac = HMAC-SHA256(K_layout, mac_input)[..16]   // truncate to leftmost 16 bytes

mac_input = b"kiseki/pnfs-fh/v1\x00" || PnfsFileHandle_60bytes
```

Notes:
- `K_layout` is derived once at boot via `HKDF-SHA256(master_key,
  salt=cluster_id_bytes, info=b"kiseki/pnfs-fh/v1")`.
- The leading `kiseki/pnfs-fh/v1\x00` domain-separation tag in
  `mac_input` ensures the MAC is bound to its purpose, even if some
  future caller uses the same `K_layout` differently.
- Field widths are **fixed** by ID type definitions in
  `kiseki-common::ids` — UUIDs only. If those types ever change to
  variable-length strings, this ADR must be revised; the validator
  must reject any non-UUID-shaped ID.
- `stripe_index` and `expiry_ms` are big-endian: NIST SP 800-90A and
  network-byte-order conventions; matches existing AAD in
  `kiseki-keymanager/src/persistent_store.rs`.
- Truncation to 16 bytes is per NIST SP 800-107 §5.1 (HMAC truncation
  to ≥ 64 bits is sufficient for authentication; 128 bits provides
  comfortable margin).

Wire size: 60 bytes payload + 16 bytes MAC = **76 bytes**. NFSv4 fh4
maximum is 128 bytes (RFC 5661 §5).

#### D4.4 Threat model

**fh4 is a bearer token, not a secret.** It is signed (MAC-validated)
but not encrypted. An attacker who observes a valid fh4 in transit
can replay it until expiry. This is RFC-standard pNFS behavior.

| Threat | Mitigation (TLS default) | Mitigation (plaintext fallback) |
|---|---|---|
| Forge fh4 without observation | HMAC-SHA256/16 = 128-bit security | Same — MAC strength unaffected |
| Capture and replay valid fh4 | mTLS prevents on-path capture | TTL halved to 60s; VPC/firewall is operator's responsibility |
| Cross-tenant fh4 reuse | tenant_id in MAC input; mTLS peer ≠ token tenant rejected | tenant_id in MAC input; **plaintext mode is single-tenant only (D4.2.4)** |
| Compromised tenant credential floods DS | mTLS peer identity → per-peer rate limit (see §D12) | Same — DS rate limit applies regardless of transport |
| Layout outlives drain/recall | I-PN5 recall + I-PN4 TTL fallback | Same |

DS validates on every op:
1. MAC matches (constant-time compare).
2. `expiry_ms > now_ms`.
3. mTLS peer identity (when TLS active) corresponds to a tenant
   permitted on the namespace in the fh4. (In plaintext mode this
   check is skipped — operator-accepted risk.)

Forged, expired, or peer-mismatched fh4s → NFS4ERR_BADHANDLE.

LAYOUTRETURN invalidates by adding fh4 to a small recently-revoked
LRU on each DS (default 16k entries; oldest evicted on overflow).

### D5. Chunk encryption boundary: **DS decrypts**

DS reads plaintext to clients (matching today's NFS gateway
behavior — decryption happens server-side, not client-side, since
the protocol is plaintext NFS-on-the-wire). DS calls
`GatewayOps::read` which already returns plaintext via `kiseki-gateway`
→ `kiseki-composition` → `kiseki-chunk` → `kiseki-crypto::decrypt`.

DS writes (LAYOUTIOMODE4_RW): plaintext from client → `GatewayOps::write`
→ encrypt → chunk store. This is the same path as the current MDS
WRITE op; the DS handler is a thin XDR-decode wrapper around the
existing `GatewayOps::write` call.

This implies **all stripes for the same composition share the same
DEK** (the composition's DEK), since stripes are byte-range views of
the same composition. No new key material is introduced.

### D6. Stripe pattern: **fixed 1-MiB stripes, round-robin across all
healthy storage nodes in the namespace's shard set**

Stripe size: 1 MiB (matches current `LayoutManager` and Linux client
default rsize).

Mirror count: 1 (no client-side mirroring; replication via Raft + EC).

Membership: the live set of nodes hosting any shard for the
composition's namespace, queried via existing
`GetNamespaceShardMap` (ADR-033). Nodes in `Drain` state (ADR-035)
are excluded from new layouts but continue serving in-flight ones
until LAYOUTRECALL fires.

LAYOUTRECALL is issued when:
- A node enters/exits `Drain` (via ADR-035 hooks)
- The shard set changes (split/merge — ADR-033/034 hooks)
- Cluster CA rotation invalidates the fh4 MAC key

### D7. Failure semantics

| Failure | Client behavior | Server behavior |
|---|---|---|
| DS unreachable (TCP timeout) | Per RFC 8435 §6: report via `ff_iostats4`, fall back to MDS for that stripe | MDS returns NFS4_OK with stripe served via local I/O (already-implemented MDS READ path) |
| DS returns NFS4ERR_BADHANDLE (expired fh4) | Client issues fresh LAYOUTGET | Standard NFSv4.1 |
| MDS restart mid-I/O | Client session-reclaim → fresh LAYOUTGET | NFSv4.1 session reclaim |
| Cluster CA rotation | LAYOUTRECALL fires; client gets fresh LAYOUTGET | MDS broadcasts recall to all session-holders |
| Composition deleted with active layout | LAYOUTRECALL → DS rejects subsequent ops with NFS4ERR_STALE | Standard |

### D8. Build phasing

Three sub-phases:

**15a — DS surface** (no client-visible behavior change):
- New listener `ds_addr` on storage nodes
- DS-only NFSv4.1 op subset: PUTFH, READ, WRITE, COMMIT, GETATTR,
  EXCHANGE_ID, CREATE_SESSION, DESTROY_SESSION (no OPEN, no LOOKUP,
  no readdir)
- fh4 MAC validation
- New crate? **No.** Lives in `kiseki-gateway` as `pnfs_ds_server.rs`,
  reusing the XDR codec from `nfs_xdr.rs`.

**15b — MDS layout wire-up**:
- Replace `op_layoutget` body with RFC 8435 `ff_layout4` encoding
- Implement `op_getdeviceinfo` (op 47)
- Wire fh4 MAC construction
- Layout cache (replaces in-memory `LayoutManager`) keyed by
  `(composition_id, byte_range)` with 5-min TTL

**15c — LAYOUTRECALL + integration**:
- Wire ADR-035 drain hooks → LAYOUTRECALL
- Wire ADR-033/034 shard hooks → LAYOUTRECALL
- BDD scenarios in `specs/features/pnfs-rfc8435.feature`
- Linux client integration test in `tests/e2e/test_pnfs.py`
  (assert `LAYOUTGET` AND DS READ via `/proc/self/mountstats`)

### D9. Configuration

```toml
[pnfs]
enabled = true                       # default true
ds_addr = "0.0.0.0:2052"             # DS listener (mTLS, see [security])
stripe_size_bytes = 1048576          # 1 MiB
layout_ttl_seconds = 300             # default 5 min (auto-halved to 60s
                                     # if allow_plaintext_nfs=true)
fh_mac_key_id = "kiseki/pnfs-fh/v1"  # HKDF info string
layout_cache_max_entries = 100000    # I-PN8 cap
layout_cache_sweep_interval_seconds = 75  # default = layout_ttl/4

[security]
allow_plaintext_nfs = false          # see §D4.2 — must also set
                                     # KISEKI_INSECURE_NFS=true to take effect
```

Disabling pNFS (`enabled = false`) makes the MDS return
NFS4ERR_LAYOUTUNAVAILABLE on LAYOUTGET, forcing standard NFSv4.2 —
the current behavior.

### D10. Topology event bus (resolves ADV-038-3 and -8)

ADR-035 drain, ADR-033 split, ADR-034 merge, and composition
deletion need to deliver a fan-out signal to the gateway-resident
`LayoutManagerOps` so it can fire LAYOUTRECALL within the I-PN5 1-sec
SLA. Today these emit audit events only (ADR-035 §5) which are not
subscribable from the gateway crate.

**Decision**: introduce a new `TopologyEventBus` owned by the
control-plane runtime. Lives in `kiseki-control` (which is already
in the gateway's dependency closure for shard-map lookups, so no
new cycle is created).

```rust
// In kiseki-control:
pub enum TopologyEvent {
    NodeDraining   { node_id: NodeId,           hlc: HLC },
    NodeRestored   { node_id: NodeId,           hlc: HLC },
    ShardSplit     { parent: ShardId, children: [ShardId; 2], hlc: HLC },
    ShardMerged    { input_ids: Vec<ShardId>, merged: ShardId, hlc: HLC },
    CompositionDeleted { tenant: OrgId, namespace: NamespaceId, composition: CompositionId, hlc: HLC },
    KeyRotation    { old_key_id: KeyId, new_key_id: KeyId, hlc: HLC },
}

pub struct TopologyEventBus {
    sender: tokio::sync::broadcast::Sender<TopologyEvent>, // capacity 1024
}

impl TopologyEventBus {
    pub fn subscribe(&self) -> broadcast::Receiver<TopologyEvent>;
    pub(crate) fn emit(&self, event: TopologyEvent);
}
```

**Producers**: drain orchestrator, namespace shard-map mutator
(split/merge), composition delete handler, key rotation handler. All
emit **after** the underlying control-Raft commit, ensuring no event
fires for an aborted operation.

**Subscriber semantics**:
- `kiseki-gateway::LayoutManager` subscribes at startup.
- Receiver lag (broadcast channel overflow) is reported via a metric
  `pnfs_topology_event_lag_total{reason="recv_lag"}`. **On lag, the
  layout cache is fully invalidated** (cheap: it's bounded by
  I-PN8). Clients then re-issue LAYOUTGET. This trades a brief
  layout-churn burst for safety after subscriber lag — preferable to
  silent recall miss.
- The 5-min I-PN4 TTL remains the ultimate safety net.

This wiring is implemented in **Phase 15d** (added to build-phases),
which must complete before Phase 15c's recall integration.

### D11. Layout cache eviction (resolves ADV-038-7)

`LayoutManagerOps` MUST bound its in-memory layout cache with two
mechanisms:

**Capacity cap**: `layout_cache_max_entries` (default 100,000).
On insert when cap is hit, evict the entry with the smallest
`issued_at_ms` (LRU on issuance).

**Background sweeper**: a tokio task running every
`layout_cache_sweep_interval_seconds` (default 75s, = `layout_ttl/4`).
Removes entries where `now_ms > issued_at_ms + ttl_ms`. Sweeps are
cheap (single pass over the map; no MDS-RPC).

Eviction triggers I-PN5 recall **only when explicitly invalidated**
(drain, split, etc., via §D10). Routine TTL eviction does NOT fire
recalls — clients learn of expiry via NFS4ERR_BADSTATEID on next op
and re-LAYOUTGET, which is RFC-standard.

Memory bound at the cap: with 100k entries × ~200 B base + ~64 KiB
of stripes per layout (1024 stripes for a 1 GiB file at 1 MiB stripe
size), worst-case ~6.4 GiB per MDS. Operators should tune the cap if
their workload is dominated by large files; default suffices for
the perf cluster (≤ 50k active layouts at any time per measurement
of similar systems).

### D12. DS rate limiting (resolves ADV-038-5; non-blocking)

Per ADV-038-5: DS-side per-tenant rate limiting is required for
production multi-tenant deployments, because the pNFS data path
bypasses MDS-side advisory budgets (ADR-021).

**Decision for Phase 15**: out of scope for the initial perf cluster
(single-tenant, isolated VPC). ADR-038 §D12 is a placeholder: a
follow-up ADR-039 or extension to ADR-021 §7 will add per-DS,
per-tenant token buckets before any multi-tenant production use.

A startup check enforces this: kiseki-server refuses to start with
`pnfs.enabled = true` AND `tenant_count > 1` AND
`ds_rate_limit_enabled = false` (default). Single-tenant deployments
bypass the check. This makes the gap **structurally enforced** at
config-load time rather than discovered after deployment.

## Consequences

### Positive

- Real pNFS acceleration available for the perf cluster (target:
  ≥ 1.5× single-MDS at 3 storage nodes; ≥ 2× at 5 nodes).
- DS is stateless — operationally simple, identical recovery story
  to current data-path.
- No new wire format invented; FFL is RFC-standard and Linux-supported.
- No new crate; lives within existing `kiseki-gateway` (small surface
  expansion: ~600 LoC estimate).

### Negative

- New listener (`2052`) means new firewall hole. ADR-027 firewall
  policy for storage nodes must be updated. Same Cluster CA, no new
  trust roots.
- fh4 MAC validation adds ~1µs per DS op. Acceptable: dominates by
  network and chunk-fetch latency.
- LAYOUTRECALL adds wiring debt to ADR-033/034/035 (drain, split,
  merge events). Hooks must be added to those flows; cleanly bounded
  but non-zero.

### Mitigated risks

- **RFC fidelity**: Linux pNFS client conformance is the litmus test.
  e2e test must mount a real Linux pNFS client and assert via
  `/proc/self/mountstats` that `LAYOUTGET`, `GETDEVICEINFO`, and
  per-DS `READ` counters all increment. If absent, ADR-038 is
  considered unimplemented regardless of unit tests.
- **fh4 forgery**: MAC over (tenant ‖ ns ‖ comp ‖ stripe ‖ expiry).
  Per ADR-005 envelope encryption invariants, this is sufficient —
  fh4 is not a secret, only an authenticator.
- **Stale layouts after split/merge**: bounded by `layout_ttl_seconds`
  even without LAYOUTRECALL, so recall failure degrades to "5-min
  stale routing" not "permanent breakage". I-PN5 below.

## Open

- Whether to support `tightly_coupled = false` (DS-side opens) for
  multi-cluster federation. Out of scope for ADR-038; revisit when
  ADR-022 federation surfaces this.

## References

- RFC 5661 (NFSv4.1 base)
- RFC 8435 (Flexible File Layout)
- ADR-013 (Protocol gateway)
- ADR-027 (firewall policy)
- ADR-033 (initial shard topology, NamespaceShardMap)
- ADR-035 (node lifecycle drain)
- `crates/kiseki-gateway/src/pnfs.rs` (current LayoutManager — to be replaced)
- `crates/kiseki-gateway/src/nfs4_server.rs:632-735` (current op stubs)
