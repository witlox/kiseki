# ADR-042: Native Gateway Data Service — gRPC data-plane for native clients

**Status**: Proposed (gate-1 review pending)
**Date**: 2026-05-05
**Deciders**: Architect (this ADR), Analyst (cycle 2026-05-05 spec layer)
**Adversarial review**: Pending. Gate-0 audit on the analyst output landed 2026-05-05 with 1C + 6H + 8M + 4L resolved in place; this ADR is the architect's first cut against the resolved spec.
**Context**: ADR-002 (encryption), ADR-008 (native client fabric discovery), ADR-013 (POSIX semantics), ADR-014 (S3 scope), ADR-015 (observability), ADR-016 (federation), ADR-019 (gateway deployment), ADR-023 (RFC compliance + CRL), ADR-026 (Raft topology), ADR-031 (client-side cache), ADR-032 (async GatewayOps), ADR-033 (shard topology), ADR-035 (drain), ADR-038 (pNFS), ADR-040 rev 3 (composition store + write-behind), ADR-041 (multiplexed Raft transport), I-T1/I-T2 (time invariants), I-L5 (composition durability), I-L8 (cross-shard EXDEV), I-K3 (crypto-shred propagation), F-CC3 (cached plaintext exposure window), the analyst output (16 ubiquitous-language terms, A-NG1..A-NG20, I-NG1..I-NG14, F-NG1..F-NG11, `specs/features/native-gateway.feature`).

## Problem

Today every kiseki client path — S3, NFS, FUSE, the Rust SDK marketed as "native" — funnels through HTTP/1 to the S3 gateway (S3, FUSE, native SDK) or raw NFS RPC framing (NFSv3/v4/pNFS). There is no `GatewayDataService` gRPC. The 2026-05-05 single-host profile measured S3 GET at 14 635 op/s (post the post-redb perf-fix sweep) — protocol layer accounts for ~87 % of the gap from the in-process gateway floor (114 995 op/s, post-spike 139 335 op/s). HPC and AI training workloads compare native-client paths; without one, kiseki cannot meaningfully claim parity with Lustre-DoM / VAST / WekaFS.

Strategic intent (set during the 2026-05-05 design conversation): aim for WekaFS-class throughput without sacrificing the security, audit, and durability invariants kiseki commits to. Performance targets pinned in A-NG11: **GET ≥ 80 k op/s**, **PUT ≥ 56 k op/s** per node on the local profile harness (in-process floor × 0.7 gRPC tax, where the in-process floor was lifted to 80 k PUT / 139 k GET by the 2026-05-05 spike).

## Decision

Land a new gRPC service `GatewayDataService` on the existing data-path port (`KISEKI_DATA_ADDR`, default 9100), exposing both object-flavored and POSIX-flavored verbs over a single tonic schema. The service mirrors the in-process `GatewayOps` trait one-to-one, with extensions for streaming, lease-based RMW, and topology discovery.

The full design is the union of:

1. **The 16 ubiquitous-language terms** added by the analyst in the "Native gateway data service" section of `specs/ubiquitous-language.md`.
2. **Invariants I-NG1..I-NG14** in `specs/invariants.md`.
3. **Assumptions A-NG1..A-NG20** in `specs/assumptions.md`.
4. **Failure modes F-NG1..F-NG11** in `specs/failure-modes.md`.
5. **Behavioral scenarios** in `specs/features/native-gateway.feature` (~33 Gherkin scenarios).
6. **The architect-level decisions** below, which fill in the protocol shape, service placement, module integration, and primitive choices.

---

## §1 Service surface — proto schema (`gateway_data.proto`)

A new file `specs/architecture/proto/kiseki/v1/gateway_data.proto` defines the service. Skeleton:

```proto
syntax = "proto3";

package kiseki.v1;

import "kiseki/v1/common.proto";

service GatewayDataService {
  // ---- Object verbs (commit-on-close) ----
  rpc PutObject(PutObjectRequest) returns (PutObjectResponse);              // unary, ≤ inline_threshold
  rpc PutObjectStream(stream PutObjectChunk) returns (PutObjectResponse);   // streaming
  rpc GetObject(GetObjectRequest) returns (GetObjectResponse);              // unary, ≤ inline_threshold OR range
  rpc GetObjectStream(GetObjectRequest) returns (stream GetObjectChunk);    // streaming
  rpc DeleteObject(DeleteObjectRequest) returns (DeleteObjectResponse);
  rpc HeadObject(HeadObjectRequest) returns (HeadObjectResponse);
  rpc ListObjects(ListObjectsRequest) returns (ListObjectsResponse);
  rpc LookupByName(LookupByNameRequest) returns (LookupByNameResponse);

  // ---- Multipart (above 64 MiB per-stream cap, A-NG5) ----
  rpc InitMultipart(InitMultipartRequest) returns (InitMultipartResponse);
  rpc PutPart(stream PutPartChunk) returns (PutPartResponse);
  rpc CompleteMultipart(CompleteMultipartRequest) returns (CompleteMultipartResponse);
  rpc AbortMultipart(AbortMultipartRequest) returns (AbortMultipartResponse);

  // ---- POSIX verbs (handle-token-based, partial-visible-on-fsync) ----
  rpc PathLookup(PathLookupRequest) returns (PathLookupResponse);          // (parent_inode, name) → inode
  rpc Open(OpenRequest) returns (OpenResponse);                             // returns handle_token
  rpc Read(ReadRequest) returns (ReadResponse);                             // unary, small
  rpc ReadStream(ReadRequest) returns (stream ReadChunk);                   // streaming
  rpc Write(WriteRequest) returns (WriteResponse);                          // unary, ≤ inline_threshold
  rpc WriteStream(stream WriteChunk) returns (WriteResponse);               // streaming
  rpc Fsync(FsyncRequest) returns (FsyncResponse);                          // visibility barrier (I-NG3)
  rpc Close(CloseRequest) returns (CloseResponse);
  rpc Setattr(SetattrRequest) returns (SetattrResponse);
  rpc Getattr(GetattrRequest) returns (GetattrResponse);
  rpc ReadDir(ReadDirRequest) returns (stream ReadDirEntry);
  rpc Mkdir(MkdirRequest) returns (MkdirResponse);
  rpc Unlink(UnlinkRequest) returns (UnlinkResponse);
  rpc RenameWithinShard(RenameRequest) returns (RenameResponse);            // EXDEV across shards (I-NG4)

  // ---- Lease-based RMW (I-NG10, I-NG12, I-NG14) ----
  rpc AcquireLease(AcquireLeaseRequest) returns (AcquireLeaseResponse);     // returns ttl + fencing_token
  rpc RenewLease(RenewLeaseRequest) returns (RenewLeaseResponse);
  rpc ReleaseLease(ReleaseLeaseRequest) returns (ReleaseLeaseResponse);

  // ---- Encryption boundary (A-NG7, I-NG6a/b/c) ----
  rpc FetchDek(FetchDekRequest) returns (FetchDekResponse);                 // TrustedCompute, single chunk
  rpc BatchFetchDek(BatchFetchDekRequest) returns (BatchFetchDekResponse);  // resolves F-H2 — single round-trip per Read

  // ---- Topology discovery (I-NG13, A-NG13) ----
  rpc GetTopology(GetTopologyRequest) returns (TopologyInfo);
}

// --- Common per-call control envelope (A-NG6) ---
//
// Every request that mutates state carries this. Server validates
// idempotency_key length ≤ 64 bytes (I-NG1, I-NG5) and tenant_id
// matches the cert SAN (I-NG1).
message ControlFields {
  TenantId tenant_id = 1;             // canonicalized form
  bytes idempotency_key = 2;           // 1..=64 bytes opaque
  string workflow_ref = 3;             // empty string defaults to "unattributed"
  CacheHint cache_hint = 4;
  oneof conditional {                  // optional
    Empty if_none_match = 5;           // S3 If-None-Match: *
    Etag if_match = 6;                 // S3 If-Match: <etag>
    VersionId if_version_match = 7;    // by version
  }
}

message CacheHint {
  bool force_revalidate = 1;
  bool skip_cache = 2;
  bool pin_after_read = 3;             // Slurm staging hint
}

// --- Response trailer (delivered as gRPC trailing metadata) ---
//
// Every response carries `topology_version` so clients can detect
// stale topology caches without an extra discovery RPC. Clients
// MUST refresh on mismatch (I-NG13, A-NG13).
//
// Implemented via `tonic::metadata::MetadataMap` on the response
// trailers — not a proto message — so it adds zero proto schema
// surface but reaches every method uniformly.

// --- Topology discovery ---

message GetTopologyRequest {
  uint64 known_topology_version = 1;   // 0 = unknown, server returns full
}

message TopologyInfo {
  uint64 topology_version = 1;
  repeated NodeInfo nodes = 2;
  repeated ShardLeadership shards = 3;
}

message NodeInfo {
  uint64 node_id = 1;
  string data_addr = 2;                // "host:port" (or "[ipv6]:port")
  NodeState state = 3;                  // Active/Degraded/Failed/Draining/Evicted
}

message ShardLeadership {
  ShardId shard_id = 1;
  uint64 leader_node_id = 2;
  bytes range_start = 3;
  bytes range_end = 4;
}

// --- Lease ---

message AcquireLeaseRequest {
  ControlFields control = 1;
  NamespaceId namespace_id = 2;
  uint64 inode = 3;
  LeaseMode mode = 4;                  // Write
  uint64 requested_ttl_ms = 5;          // server clamps to its config max
}

message AcquireLeaseResponse {
  oneof outcome {
    LeaseGrant grant = 1;
    LeaseHeld held = 2;
    NodeDraining draining = 3;          // I-NG14
  }
}

message LeaseGrant {
  bytes lease_id = 1;                  // opaque, 16 bytes
  uint64 fencing_token = 2;             // I-NG12
  uint64 ttl_ms = 3;                   // server-granted, may be < requested
  google.protobuf.Timestamp expires_at = 4;
}

message LeaseHeld {
  string holder_principal = 1;          // opaque cert SAN URI of holder
  uint64 ttl_remaining_ms = 2;
}

message NodeDraining {
  uint64 quiesce_window_remaining_ms = 1;
}

// --- DEK fetch (TrustedCompute) ---
//
// Server returns the per-chunk DEK, gated by the at-Read-time
// crypto_boundary mode encoded in the DEK-fetch ticket (I-NG6a,
// resolves F-H5). Ticket is HMAC-signed under the system DEK.

message FetchDekRequest {
  ControlFields control = 1;
  bytes dek_fetch_ticket = 2;           // opaque, server-signed
}

message FetchDekResponse {
  oneof outcome {
    Dek dek = 1;
    InvalidDekTicket invalid = 2;
    NamespaceModeChanged mode_changed = 3;
  }
}

message Dek {
  bytes key_bytes = 1;                  // 32-byte AES-256 DEK
  uint64 dek_validity_ms = 2;          // bounded by namespace policy
}

// (Other message types — PutObjectRequest / GetObjectChunk / etc. —
// follow the existing GatewayOps shape; full schemas in the
// implementation phase.)
```

**Codec choice**: standard tonic + prost (protobuf v3). No custom codec. The existing kiseki-proto crate generates Rust bindings; ADR-042 adds `gateway_data.rs` to the generated set.

**Tenancy**: the cert SAN URI is the source of truth. The `tenant_id` on `ControlFields` is the byte-equal redundant copy validated at the proto-handler boundary per I-NG1 (cross-check defense-in-depth).

**Per-stream flow control**: HTTP/2 stream window 16 MiB, connection window 32 MiB (matches the existing data-port server config from ADR-041, set in `kiseki-server::runtime::run_data_path`). No per-method overrides.

**Per-stream cap**: 64 MiB per `PutObjectStream` / `GetObjectStream` body (I-NG9). Above that, clients use `InitMultipart` / `PutPart` / `CompleteMultipart`. Server enforces the cap at the proto-handler boundary.

**Concurrent stream cap**: 256 in-flight per tenant by default (I-NG11, A-NG14), configurable via `KISEKI_NATIVE_STREAM_CAP`. Excess returns `ResourceExhausted{native_concurrent_stream_cap}` before any staging buffer is allocated.

---

## §2 Module placement

The new service spans three crates:

| Crate | Addition |
|---|---|
| **kiseki-proto** | Generated Rust bindings for `gateway_data.proto` (`gateway_data.rs`, `gateway_data_service_server.rs`, `gateway_data_service_client.rs`). |
| **kiseki-gateway** | `kiseki_gateway::native::{NativeGatewayService, ServerImpl, SanInterceptor, HandleToken, LeaseTable, IdempotencyDedup}`. The existing `GatewayOps` trait is the work-doer; the new service is a thin tonic wrapper. |
| **kiseki-client** | `kiseki_client::native::{NativeClient, TopologyCache, LeaseManager}`. Replaces `RemoteHttpGateway` for the SDK-direct path. FUSE migration is a separate work-stream (A-NG1, captured in `docs/performance/optimization-backlog.md`). |
| **kiseki-server** | Wires the new service into `runtime::run_data_path` alongside `ControlService`, `KeyManagerService`, etc. Reuses the existing mTLS + flow-control window config (A-NG9). |

Module-graph addition:

```
kiseki-client::native ──▶ kiseki-proto::gateway_data (client) ──▶ tonic ──▶ kiseki-server::runtime::run_data_path
                                                                                       │
                                                                                       ▼
                                                                kiseki-gateway::native::ServerImpl
                                                                                       │
                                                                                       ▼
                                                                        kiseki-gateway::ops::GatewayOps  (in-process)
                                                                                       │
                                                                ┌──────────────────────┼──────────────────────┐
                                                                ▼                      ▼                      ▼
                                                  kiseki-composition       kiseki-chunk            kiseki-log
```

No new bounded contexts. The native gateway is a new *ingress* on the existing Gateway / Composition / Chunk / Audit / Encryption contexts.

---

## §3 Authentication — SAN canonicalization + interceptor

A new tonic interceptor `kiseki_gateway::native::SanInterceptor` runs on every method call:

1. Extract the client cert from the connection's TLS state (existing tonic + tokio-rustls plumbing).
2. Read the SAN URI extension. Reject `Unauthenticated{no_san_uri}` if absent.
3. Canonicalize per the rules in the *Canonical SAN URI form* ubiquitous-language entry: lowercased scheme, lowercased authority, no trailing slash, percent-decoded unreserved characters, NFC-normalized tenant id, ASCII-only.
4. Stash the canonical form in the tonic request extensions for downstream handler inspection.

The handler's first job (`ServerImpl` for each RPC):

1. Decode `ControlFields.tenant_id` and canonicalize.
2. Compare byte-equal against the SAN canonical form. Mismatch returns `PermissionDenied{san_canonicalization_mismatch}` and emits a security-failure audit event (I-NG7).
3. Validate `idempotency_key` length 1..=64. Out of range returns `InvalidArgument{idempotency_key_length}`.

This interceptor is **distinct** from the existing `cluster_chunk_san_interceptor` (which validates cluster-node-role SANs, ADR-041). The two interceptors share the canonicalization helper but enforce different SAN role grammars.

**Multiple SAN URIs (resolves F-M4)**: x.509 certs may carry multiple SAN URI extensions. The interceptor scans for SAN URIs matching the kiseki tenant pattern (`spiffe://kiseki/tenant/<org_id>` after canonicalization). The cert MUST contain **exactly one** matching URI. Zero matches → `Unauthenticated{no_kiseki_tenant_san}`. Two or more matches → `Unauthenticated{ambiguous_kiseki_tenant_san}` (defends against a malicious issuer placing multiple tenant URIs to pivot which one validates). Non-kiseki SAN URIs (e.g., DNS names for service-mesh interop) are ignored.

**Inode allocation discipline (resolves F-M3)**: inodes are allocated by the composition store via a per-namespace monotonic 64-bit counter (`next_inode_per_namespace`), persisted in the redb `meta` table alongside `last_applied_seq`. Inodes are **never reused**, even after unlink + GC. At 4 billion inode allocations / sec for ~146 years before u64 wraps, this is bounded by physics not policy. Stale handle tokens whose `inode` no longer exists in the namespace return `Unauthenticated{inode_orphaned}` on next op + emit `kiseki_native_handle_orphaned_inode_total{tenant}`.

**Cert revocation mid-session** (A-NG17 / F-NG8): the interceptor calls into the existing CRL/OCSP source (ADR-023) on connection establishment; the long-stream re-validation runs on a `tokio::time::interval` of `KISEKI_CERT_REVAL_INTERVAL_MS` (default 60 s) per active stream. On revocation the interceptor closes the gRPC stream with `Unauthenticated{cert_revoked}`.

---

## §4 Hybrid leader routing — `topology_version` push + safety-net TTL

Per I-NG13 / A-NG13 + I-NG8:

- **Server side**: every `ServerImpl` method appends `kiseki-topology-version: <u64>` to the response's tonic trailing metadata. The cluster increments the version monotonically on shard-leader change, namespace-shard-map mutation, or split/merge event (publishers are `kiseki-control` + `kiseki-log`).
- **Client side**: `kiseki_client::native::TopologyCache` holds `(version, Vec<ShardLeadership>)` behind a `parking_lot::RwLock`. Every native client RPC checks the trailing-metadata version on response; on mismatch it re-issues `GetTopology` to refresh.
- **TTL safety net** (I-NG8 default 30 s): if the topology version channel regresses (operator error / clock skew), the cache invalidates after 30 s anyway.
- **Direct dial vs proxy fallback**: client picks the leader from the cache and dials it directly. On `NotLeader{leader=X}` or `LeaderUnavailable`, client refreshes and retries; if the dialed node has the proxy-fallback path enabled (configurable per cluster, default off — explicit-routing-only), it proxies in-process to the actual leader. When proxying, the trailing topology version is the leader's so the client cache catches up in one round-trip.

**`GetTopology` contract**: when `known_topology_version > 0` and equals the responding node's current version, server returns 304-equivalent (empty `TopologyInfo` with `topology_version` set to current). When `0` or stale, returns the topology — **scoped to the calling tenant** (resolves F-H3): the `shards` list filters to only shards that own at least one of the caller's namespaces. The `nodes` list still includes every node the client may need to dial (since shards are spread across nodes), but the *cross-tenant shard-leader map* is hidden.

**Consistency model** (resolves F-M8): the version returned reflects the *responding node's* view, which may be stale by up to one Raft heartbeat (default 100 ms, ADR-026). Clients treat the version as eventually consistent within this bound. Metric `kiseki_native_topology_staleness_seconds{node}` exposes the per-node lag for ops alarms.

**Channel multiplexing model** (A-NG15): the client maintains one tonic channel per *node it currently believes hosts at least one shard leader the client cares about*. Channels are reused via HTTP/2 multiplexing across all such shards. Idle channels are closed when the client's topology cache no longer references them.

---

## §5 Streaming + multipart shape

## §5.0 Multi-chunk PUT atomicity (resolves F-C2)

A PutObjectStream of size > `MAX_PLAINTEXT_PER_CHUNK` (default 4 MiB) splits into multiple chunks. Different chunks hash to different `chunk_id`s and the cluster places them on different shards (rendezvous-hash placement, ADR-033). The CommitStream contract (no partial visible, I-NG2) requires explicit specification of the multi-chunk failure mode:

**Contract**:
1. **Chunk write parallelism**: chunks within a single PutObjectStream / PutPart write in parallel up to `KISEKI_PUT_CHUNK_PARALLELISM` (default 4). Higher values trade memory for latency on large objects.
2. **CommitStream barrier**: server returns `Ok` ONLY when ALL referenced chunks have been confirmed durable on `min_acks` peers per the pool's durability strategy (preserves I-L5).
3. **Partial-failure aborts the whole stream**: if ANY chunk fails (timeout, quorum loss, etc.), CommitStream returns `Aborted{reason=partial_chunk_failure, succeeded=N, total=M}`. The client retries with the same `idempotency_key` (per A-NG10) — the dedup table short-circuits to the original outcome (the abort), or the client uses a fresh key for a new attempt.
4. **Staged-chunk cleanup**: chunks that landed before the failure are NOT linked into a composition record. They have refcount = 1 (set by `write_chunk` for new chunks) but no composition references them. The existing **orphan-fragment scrub** (ADR-005, F-D7 mitigation) reclaims them after a 24-hour TTL.
5. **No partial state visible at any point** (I-NG2). Readers observing the namespace before the abort or during the abort never see a half-written composition; the composition row only exists if CommitStream succeeded.

**New failure mode F-NG12** (added to `specs/failure-modes.md`):

| Field | Value |
|---|---|
| **F-NG12 Description** | Multi-chunk PUT/PutPart writes some chunks successfully, then a later chunk fails (shard quorum loss, timeout, etc.). |
| **Blast radius** | The single PUT. Successful chunks orphan with refcount=1 (no composition reference). |
| **Detection** | CommitStream returns `Aborted{partial_chunk_failure}`. Metric `kiseki_native_put_partial_chunk_failure_total{tenant}`. |
| **Degradation** | Reader visibility unaffected (composition never created). Storage cost: orphaned chunks for 24 h. |
| **Recovery** | Orphan-fragment scrub reclaims the chunks. Client retries via idempotency_key path. |
| **Severity** | **P3** (per-PUT, bounded) |

**Streaming threshold** (I-NG9): the `PutObject` / `GetObject` / `Write` / `Read` unary forms accept payloads ≤ inline_threshold (default 8 KiB, ADR-006). Above, clients use the `Stream` form.

**Per-stream cap** (I-NG9): 64 MiB per stream. Server tracks the cumulative bytes for each open `PutObjectStream` and rejects the stream with `OutOfRange{stream_cap_exceeded}` when a frame would push past 64 MiB.

**Multipart** (above 64 MiB): clients use S3-style flow:
- `InitMultipart(namespace_id, name) → upload_id` — server-side reservation + idempotency_key dedup
- `PutPart(upload_id, part_number, data)` — streaming, per-part
- `CompleteMultipart(upload_id, parts: Vec<PartETag>)` — finalize; server validates etags + emits the composition delta atomically
- `AbortMultipart(upload_id)` — reclaim staged parts

The proto / wire shape mirrors S3 multipart (the existing `kiseki-gateway::s3` impl already speaks this); ADR-042 leverages the same in-process `start_multipart` / `upload_part` / `complete_multipart` APIs that S3 calls into.

**Resume-token** (handoff Q3): NOT in ADR-042 v1. A future amendment may add a session-resume path on top, but correctness is already provided by `idempotency_key` + fresh-stream retry (A-NG10 / I-NG5). Captured in `docs/performance/optimization-backlog.md`.

**Repeated stream-`first` messages (resolves F-M1)**: every streaming RPC has a `first` oneof variant carrying the request envelope (with `ControlFields`, including `idempotency_key`). The server's stream handler accepts EXACTLY ONE `first` per stream. A second `first` returns `InvalidArgument{stream_already_initialized}` and aborts the stream. Any frames before the first `first` return `InvalidArgument{stream_not_initialized}`. The client SDK enforces the single-`first` invariant; this server-side check defends against malicious or buggy clients.

---

## §6 Idempotency — Raft-replicated dedup state

Per I-NG5 + A-NG10 (resolves F-H2):

- **Dedup table** lives in the per-shard openraft state machine alongside the chunk-state and composition-state tables.
- Each accepted write writes a row `(tenant_id, namespace_id, idempotency_key) → (response_summary, expires_at_ms)` keyed by the triple, value carrying the bytes the original response returned (composition_id, etag, etc.) + a TTL.
- Every voter applies the same row in the apply phase. Retries arriving after a leader change deduplicate against the new leader's local replica.
- A periodic sweep (`kiseki-log::dedup_gc`) trims expired rows. Default TTL 5 min (I-NG5); configurable per tenant.

**Ordering vs lease fencing (resolves F-H4)**: when a request carries a `fencing_token` (lease-bound write), the server MUST validate the token **before** consulting the dedup table. The check order is:

1. SAN canonicalization + payload tenant_id match (I-NG1).
2. **Fencing token check** (if request has one): reject with `LeaseFenced{current_token}` if stale. **Do not consult dedup.**
3. Idempotency dedup: if a row matches and is unexpired, return the original response.
4. Otherwise: process the request normally.

This ordering closes the F-H4 race where a partitioned old lease holder could replay a successful pre-partition write and bypass the new fencing_token. The fencing check dominates the dedup short-circuit. The dedup row is written only after step 4 succeeds, with the fencing_token included in the row's `request_meta` (for audit + post-mortem).

**Cost analysis**: each row is ≤ 256 bytes (a UUID composition_id + a u64 + a u32 etag). At 100 k writes/sec/shard with 5 min TTL: 30 M rows × 256 B = 7.7 GiB per shard worst-case (zero dedup hits). In practice TTL eviction keeps it small. Per-tenant rate limits + the 64-byte cap on `idempotency_key` (I-NG1) bound the worst case.

**Per-stream cap interaction**: in-flight streams that haven't called `CommitStream` are NOT in the dedup table — they're in a separate per-leader staging map (also bounded by I-NG11's 256 concurrent streams per tenant).

---

## §7 Lease lifecycle — TTL, fencing, drain

Per I-NG10 + I-NG12 + I-NG14:

- **`AcquireLease`**: server checks `(namespace_id, inode)` — if no lease or expired, grants `LeaseGrant{lease_id, fencing_token, ttl_ms}`. The `fencing_token` is a per-(tenant, namespace, inode) monotonic 64-bit counter, persisted as part of the dedup table's apply phase so it survives leader change.
- **`RenewLease`**: validates the presented `lease_id` matches the current grant; server extends the TTL to `now + ttl_ms` and returns the same fencing_token. Renewal cadence (recommended): **1/3 of TTL** (default 30 s TTL → 10 s renewal cadence). Documented in the Operator manual; not enforced by the server.
- **`ReleaseLease`**: voluntary. Server clears the lease + invalidates pending uncommitted writes for the inode.
- **Lease holder death**: TTL expiry (no renewal in `ttl_ms`). Server's lease tracker has a `tokio::time::interval` per active lease that fires expiry on timeout; expiry runs through the per-shard Raft state machine so every replica sees the same revocation event.
- **Forced revoke** (admin op): a separate admin RPC `kiseki-control::admin::ForceRevokeLease(namespace_id, inode)` clears the lease without waiting for TTL. Out of scope for ADR-042 v1; defer to a future operations ADR.
- **Fencing on writes**: every `Write` / `WriteStream` carries `fencing_token` in the per-call control. Server rejects with `LeaseFenced{current_token}` if the presented token < the current lease's token. Audit event records the rejected fencing_token and the principal (I-NG12).
- **Drain interaction** (I-NG14): when a node enters `Draining`, its lease tracker:
  1. Refuses new `AcquireLease` requests if the requested TTL would outlast the configured drain quiesce window. Returns `Unavailable{node_draining}`.
  2. Existing leases continue until expiry / release. Drain protocol waits.
  3. Operator can choose `ForceRevokeLease` to bound the drain duration; but the default is "wait."

---

## §8 Encryption boundary — server-decrypt default + opt-in client-decrypt + DEK-fetch ticket

Per I-NG6a/b/c + A-NG7 (resolves F-C1, F-H5):

- **`crypto_boundary = ServerOnly` (default)**: every `GetObject` / `GetObjectStream` / `Read` / `ReadStream` returns plaintext. The wire is mTLS-encrypted; no plaintext escapes the server's TLS context.
- **`crypto_boundary = TrustedCompute`**: requires the namespace to ALSO declare `crypto_shred_policy = best_effort` (I-NG6c). Server returns sealed envelopes (`ciphertext + nonce + tag`) plus a `dek_fetch_ticket` per chunk. Client calls `FetchDek(dek_fetch_ticket)` to obtain the per-chunk DEK and decrypts locally. Unlocks GPU-direct + zero-copy paths.
- **DEK-fetch ticket**: HMAC-SHA256-signed under the **dek-fetch ticket signing key** (see §"Cryptographic key derivations" below); commits to `(tenant_id, namespace_id, composition_id, chunk_id, namespace crypto_boundary at Read time, master_key_epoch, expires_at)`. Keymanager re-derives the same signing key from the master key, validates the HMAC, validates the at-Read-time mode against the namespace's current policy, and validates the master_key_epoch against the current epoch ± grace window; on mismatch returns `NamespaceModeChanged` or `Unauthenticated{ticket_epoch_stale}`.
- **Mode flip on object verbs (I-NG6a)**: object reads commit the at-Read-time mode into the ticket. A flip from `TrustedCompute` → `ServerOnly` between Read and `FetchDek` / `BatchFetchDek` does NOT break in-flight reads — the keymanager honors the ticket's at-Read-time mode.
- **Multi-chunk Reads (resolves F-H2)**: a single Read whose response contains N sealed chunks returns N tickets. Clients MUST call `BatchFetchDek(repeated dek_fetch_ticket)` instead of N separate `FetchDek` calls. The keymanager validates all tickets in one round-trip and returns the matching `repeated Dek` array. Per-Read latency stays O(1) keymanager calls regardless of chunk count. Single-chunk Reads MAY use either RPC; clients SHOULD use `BatchFetchDek` uniformly for simpler code paths.
- **Mode flip on POSIX verbs (I-NG6b)**: handled by handle tokens (see §9).
- **Crypto-shred under TrustedCompute**: tenant-issued shred destroys the master key in the keymanager; future `FetchDek` / `BatchFetchDek` calls fail. Already-fetched DEKs in client RAM are NOT retroactively revocable — operators acknowledge this by setting `crypto_shred_policy = best_effort` on the namespace (I-NG6c). The keymanager invalidates new DEK fetches; client-side caches (when implemented in a future ADR) must respect a wipe signal.
- **`crypto_boundary` flag mutation auth (resolves F-M6)**: setting `crypto_boundary = TrustedCompute` (and the dual `crypto_shred_policy = best_effort`, I-NG6c) requires **cluster-admin** authentication, NOT tenant admin. The reason: flipping to TrustedCompute weakens the crypto-shred contract for the namespace's data — a compromised tenant admin could otherwise extend the residual-exposure window for their own data without operator oversight. Tenant admins MAY *request* the change via a separate `RequestCryptoBoundaryChange` admin RPC (deferred to a future operations ADR — out of scope for ADR-042 v1); the cluster admin reviews + applies. The control plane (`kiseki-control`) enforces this by rejecting `Setattr{crypto_boundary=TrustedCompute}` calls whose authenticating principal is not in the cluster-admin role.
- **DEK cache (out of scope)**: ADR-042 explicitly does NOT introduce a DEK cache. See §11.

---

## §9 POSIX handle tokens

POSIX inode handles (`Open` → `handle_token`) are opaque, **server-signed** tokens that encode at-open-time state, removing the need for server-side per-handle session state (A-NG6). Token contents:

```rust
struct HandleToken {
    schema_version: u8,                  // bump on any wire-format change
    namespace_id: NamespaceId,
    inode: u64,
    open_mode: OpenMode,                 // Read / Write / ReadWrite
    crypto_boundary_at_open: CryptoBoundary,  // I-NG6b
    cert_san_canonical: String,          // resolves F-H1 — token bound to issuer
    master_key_epoch: u64,               // resolves F-C1 — invalidate on master rotation
    issued_at: SystemTime,
    issuance_nonce: [u8; 16],
}
// serialized as postcard, then HMAC-SHA256-tagged with the
// handle-token signing key (see §"Cryptographic key derivations").
```

- **`Open`**: server allocates inode (or resolves), captures the at-open-time mode + the connection's canonical SAN URI + the current master_key_epoch, signs the token, returns it.
- **Subsequent ops** (`Read`, `Write`, `Fsync`, etc.) carry the token. Server validates the HMAC, validates the master_key_epoch is within the grace window (default ±1 epoch), validates the token's `cert_san_canonical` matches the connection's current SAN (resolves F-H1), decodes the mode, and operates accordingly. Mode flips on the namespace AFTER the token was issued do not affect this handle (I-NG6b).
- **`Close`**: voluntary. The token's `issued_at + token_max_lifetime` (default 1 hour) bounds residual exposure.
- **Cert revocation**: a revoked cert cannot establish a new mTLS connection (per ADR-023). A held token is also invalidated implicitly — the client cannot present it to the server because it cannot establish the connection that would carry it. F-H1 closed.
- **Per-handle state** (e.g., file position) is **client-side**, not server. Server is stateless except for inode-monotonic-counter-state in the composition store.

## §9.1 Cryptographic key derivations (resolves F-C1)

The architect-handoff phrase "system DEK" (used in earlier drafts of §8 / §9) was shorthand and ambiguous: per ADR-003 there is no singleton "system DEK" — system DEKs are derived per-chunk via HKDF(master_key, chunk_id). Handle-token and DEK-fetch-ticket signing requires distinct, well-named keys derived from the system master key.

ADR-042 specifies four signing keys, each derived once per process at startup from the system master key (held in mlock'd memory, ADR-002 / I-K8):

| Key name | Derivation | Holder | Purpose |
|---|---|---|---|
| `handle_token_signing_key` | `HKDF-SHA256(master_key, salt="kiseki-handle-token-v1", okm_len=32)` | Gateway (every gateway process derives the same value from the same master_key) | HMAC-SHA256 sign / verify HandleToken |
| `dek_fetch_ticket_signing_key` | `HKDF-SHA256(master_key, salt="kiseki-dek-fetch-ticket-v1", okm_len=32)` | Gateway (signs) and keymanager (verifies) — both derive from master_key | HMAC-SHA256 sign / verify DEK-fetch ticket |
| `topology_signing_key` | reserved, future use | — | (Not used in v1) |
| `multipart_upload_signing_key` | `HKDF-SHA256(master_key, salt="kiseki-multipart-upload-v1", okm_len=32)` | Gateway | HMAC-SHA256 sign / verify multipart `upload_id` opacity |

**Rotation discipline**:
- The master key has an epoch (ADR-007). Tokens / tickets carry the epoch.
- On verification, the server / keymanager re-derives the signing key from the **current** master key AND from the **previous** master key (kept in memory during the rotation grace window, default 5 min, configurable per `KISEKI_MASTER_KEY_ROTATION_GRACE_MS`). In-flight tokens issued under the previous epoch validate during the grace window; after, they fail with `Unauthenticated{token_epoch_stale}`.
- Clients re-Open / re-establish on `token_epoch_stale`. New tokens carry the new epoch.

**Compromise model**:
- Compromise of the master key → all four signing keys are derivable → full game-over. ADR-002 / I-K8 already governs master-key protection.
- Compromise of a signing key alone (e.g., a memory disclosure CVE in the gateway process) → only that signing key's tokens are forgeable. The other keys + future tokens (after master-key rotation) are unaffected.

**Where each derivation happens**:
- Gateway: at process startup in `kiseki-gateway::native::SigningKeys::new(master_key)`. Held in `Zeroizing<[u8; 32]>` (ADR-002).
- Keymanager: at process startup in `kiseki-keymanager::native::TicketVerifier::new(master_key)` for `dek_fetch_ticket_signing_key` only. Other signing keys are not derived in the keymanager (it doesn't sign / verify them).

---

## §10 Audit

Per I-NG7:

- **Gateway-dispatched paths** auto-fire through the existing audit pipeline (ADR-009) because every native handler calls into the same `GatewayOps` trait that S3 / NFS / FUSE use. No new audit wiring on these paths.
- **`workflow_ref` policy** (resolves F-H5): the per-tenant policy field `workflow_ref_required_for_writes` (set on the tenant via the existing control plane, ADR-020 / ADR-021) drives the server's behavior on writes that omit `workflow_ref`:
  - `workflow_ref_required_for_writes = true` (compliance-critical tenants): server REJECTS the write with `InvalidArgument{workflow_ref_required}` at the proto-handler boundary, **before** any storage work runs. Audit pipeline emits a security-failure event recording the principal + the rejection reason.
  - `workflow_ref_required_for_writes = false` (default): server substitutes the literal token `unattributed` for the workflow_ref on the audit event. The write proceeds. Operators / auditors track tenants with high `unattributed` counts via `kiseki_native_writes_unattributed_total{tenant}` to flag gradual policy drift.
- **`AcquireLease` / `RenewLease` audit volume** (resolves F-M2): `AcquireLease` and `ReleaseLease` produce audit events as usual. `RenewLease` does NOT — instead, the metric `kiseki_native_lease_renewals_total{tenant, namespace}` increments on each successful renewal. Renewals at 1/3-TTL cadence on 10 000 active leases would otherwise produce 3.6 M audit events per hour; the counter conveys the same SLO signal at fixed cost.
- **Proto-handler-boundary rejections** (I-NG1 SAN/payload mismatch, I-NG6c missing dual-flag, I-NG11 stream cap exceeded, I-NG12 fenced write, I-NG14 lease-against-draining-node) emit a `security-failure` audit event via an explicit `audit_emit_at_proto_boundary` hook. The hook lives in the same `SanInterceptor` module so it runs uniformly across every method; principal = canonical SAN URI; reason = mapped from the rejection code.
- **Lease writes**: every successful lease-bound `Write` records `lease_fencing_token` alongside the principal and workflow_ref (I-NG12).

---

## §11 Out of scope (explicit non-goals)

ADR-042 deliberately does NOT introduce:

1. **DEK caching** — a complementary optimization that would amortize HKDF on read-repeat workloads. Captured in `docs/performance/optimization-backlog.md` (B5) and as a future ADR-04X amending ADR-002 / ADR-011 with a unified crypto-cache discipline. Implementer MUST NOT add a DEK cache as an "obvious optimization" without that ADR.
2. **Resume tokens for streaming writes** — fresh-stream + idempotency_key (A-NG10) is sufficient for correctness; bandwidth-resume is a future amendment.
3. **Cross-site federation** — native ops are single-cluster only. Cross-site replication remains async per ADR-016. Architect handoff Q6 confirmed.
4. **FUSE migration to native** — separate work-stream (A-NG1). FUSE keeps using `RemoteHttpGateway` (HTTP) until the migration commits in a follow-up ADR. Diverging behavior between FUSE and native is explicitly accepted for the duration.
5. **`CompositionStore` sharding (DashMap)** — would push the in-process PUT floor through 100 k op/s. Captured as B1 in the optimization backlog. Not a precursor to ADR-042 — ADR-042 ships with the post-spike floor (80 k PUT, 139 k GET).
6. **Forced lease revocation by admin** — operations ADR; ADR-042 v1 only specifies TTL-based and voluntary release.

---

## §11.1 Schema discipline (resolves F-M7 — pre-1.0 scope)

Kiseki is **pre-production**: there are no deployed clients to preserve and no backward-compat contract to enforce. ADR-042 doesn't pretend otherwise. The schema discipline below is **internal hygiene** for the implementer + reviewers, not a stability promise:

- **proto fields and enum values use sane numbering**. Don't reuse field numbers within a single proto file (proto3 will accept it but the generated code paths get confusing). New variants append to the end; deprecated fields can be deleted outright since there are no on-the-wire deployments to break.
- **Opaque tokens carry a 1-byte schema_version** so a future incompatible change can flip the version and reject older formats with a typed error (`Unauthenticated{token_schema_too_new}`). Tokens are always re-issuable from the server; clients re-Open / re-Init on bump. Cheap to keep, useful for sanity-checking corruption / forgery.
- **Master-key-epoch in tokens** (per §9.1) covers cryptographic rotation independently of the schema_version — preserved because the reasoning is independent of compat.
- **Once we declare 1.0** (out of ADR-042's scope; will land in a separate "wire-stability" ADR), the discipline tightens: append-only fields, never-reuse numbers, formal deprecation tagging, etc. Until then, *delete and rename freely* during the iteration cycle, just bump the proto file's package version comment in the same commit so reviewers can see the intent.

## §12 Performance budget (commitments)

A-NG11's targets, post-2026-05-05 spike floor measurement:

| Metric | Target (per-node, on the spike workstation hardware) | Rationale |
|---|---:|---|
| Native GET 64 KiB | **≥ 80 k op/s** | In-process floor 139 k × 0.7 gRPC tax |
| Native PUT 64 KiB | **≥ 56 k op/s** | In-process floor 80 k × 0.7 gRPC tax |
| Native PUT p99 64 KiB | ≤ 10 ms | Spike measured 2.8 ms in-process; gRPC adds ~3 ms |
| Native GET p99 64 KiB | ≤ 5 ms | Spike measured 2.5 ms in-process; gRPC adds ~2 ms |
| Concurrent in-flight streams (per tenant) | 256 default | I-NG11 / A-NG14 |

Architect's primitive choices to land within the budget:

- **tonic codec**: prost (default). No reason to deviate.
- **Allocation discipline on Read (resolves F-M5)**: tonic + prost's framing layer copies bytes once on encode (a true end-to-end zero-copy claim would require a custom tonic codec, deferred). Within the gateway, application-layer types use `bytes::Bytes` so the chunk-store → AEAD → tonic-encode chain skips intermediate `Vec<u8>` allocations on the gateway side; `Vec::into()` produces a `Bytes` zero-copy when the source is already on the heap. The honest performance contract: "minimize per-op gateway-side allocations on the Read path" — measure and budget against the post-spike floor (125 k op/s GET in-process); a future codec audit may close the remaining tonic-side copies.
- **Hot-path instrumentation**: respects `KISEKI_OBSERVABILITY=on/off` (existing knob from the May 2026 sweep). When `=off`, the `InstrumentedLogOps` and `InstrumentedKeyManager` wrappers are bypassed; the new native path skips its own histogram observation similarly. Default: on.
- **Per-tenant concurrent-stream counter (resolves F-H6)**: `dashmap::DashMap<TenantId, Arc<AtomicUsize>>` — sharded by tenant, so increments / decrements on different tenants never block each other. The cap-then-allocate sequence is two atomic ops: `fetch_add(1, Acquire)` to claim a slot, compare against the cap, on overflow `fetch_sub(1, Release)` and return `ResourceExhausted`. No mutex on the hot path. Cap-checking at proto-handler boundary BEFORE staging buffer allocation (I-NG11). For the on-the-fly entry creation (a tenant making its first concurrent stream), the DashMap entry guard is held briefly during `or_insert`.
- **Idempotency dedup state in Raft state machine**: piggybacks on the existing apply phase; no new Raft proposal cost.

---

## §13 Adversary gate-1 hot spots (pre-emptive)

Architect anticipates the gate-1 review will challenge:

1. **SAN canonicalization** — Unicode / IDN / percent-encoding edge cases. Implementation MUST use a single canonicalization helper, applied to BOTH cert SAN AND payload tenant_id, with byte-exact compare of the canonicalized forms. Helper has its own unit tests covering the 6 near-miss cases in `native-gateway.feature`'s Scenario Outline.
2. **Idempotency dedup state on shard merge / split** — the dedup table is in the per-shard Raft state machine. ADR-033 §3 / §4 describe the merge / split apply hooks. Architect should ensure the dedup TTL semantics survive: on merge, both shards' dedup tables coalesce; on split, each child shard inherits the relevant subset (key range determines partition).
3. **Lease + drain race** — between `AcquireLease` accept and the node entering `Draining`, the lease may already be granted with a TTL longer than the quiesce window. ADR-042 §7 says drain "waits"; architect should verify the ADR-035 drain protocol can tolerate up to one TTL of additional drain latency without spurious failure.
4. **TrustedCompute mode flip during `FetchDek`** — server signs the ticket at Read time; keymanager validates against current namespace mode. Architect should verify there is no window where a flip from `ServerOnly` → `TrustedCompute` (on the rare reverse case) leaves an in-flight ServerOnly Read pending without a valid ticket. Mitigation: ServerOnly responses don't issue tickets at all, so the reverse flip has no ticket to validate.
5. **Topology-version regress under operator error** — if a leader change happens but the version isn't incremented (bug), clients see stale topology indefinitely. The 30 s TTL safety net is the ultimate fallback. Architect should add metric `kiseki_native_topology_version_mismatch_total` so a regression surfaces quickly.
6. **Per-tenant stream cap counter atomicity** — the cap-then-staging-allocate sequence MUST be atomic w.r.t. the counter. Using a single `parking_lot::Mutex<HashMap<TenantId, ConcurrentStreams>>` ensures this; an `AtomicUsize` with separate increment + check would race. Architect prefers the mutex.
7. **Cert revocation + long-running streams** — mid-stream tear-down on revocation interacts with the partial-state-not-visible (I-NG2) contract. Architect verifies the existing in-flight staging buffer cleanup catches the torn-down stream identically to F-NG2's interrupt-cleanup path.
8. **`compositions_handle` external sharing** — ADR-040 rev 3's hydrator + the new ADR-042 service both share `Arc<parking_lot::Mutex<CompositionStore>>`. Concurrent native PUTs + hydrator batches can stall each other under heavy fan-in. The DashMap optimization (B1) is the eventual fix; ADR-042 v1 accepts the contention bound.

---

## §14 Migration / coexistence

ADR-042 introduces a NEW service alongside the existing S3 / NFS / FUSE paths. Coexistence rules:

- **No deprecation of existing paths.** S3 / NFS / FUSE continue to work unchanged. They share the same in-process `GatewayOps` so behavior is consistent.
- **kiseki-client::native** replaces the SDK-direct path that previously went through `RemoteHttpGateway`. The legacy `RemoteHttpGateway` stays for FUSE pending the FUSE migration (separate work-stream).
- **Python / C++ / C-FFI bindings** continue to surface the existing `KisekiClient` API. Internally they switch to `NativeClient`. No SDK-API breakage.
- **Cert issuance**: tenant clients need certs with SPIFFE-format SAN URIs (`spiffe://kiseki/tenant/<org_id>`). Operators using cluster-internal certs (cluster-node-role) must NOT use those for client-side native ops; the SAN format differs and the SAN-role interceptor will reject.

**Build phase ordering** (for `specs/architecture/build-phases.md` update):

1. Add `gateway_data.proto` + generated Rust bindings to kiseki-proto.
2. Implement `kiseki_gateway::native::ServerImpl` thin wrapper over `GatewayOps`.
3. Add the `SanInterceptor` (proto-handler-boundary validation + audit emission).
4. Wire into `kiseki-server::runtime::run_data_path` alongside existing services.
5. Implement `kiseki_client::native::NativeClient` + `TopologyCache`.
6. BDD steps for `native-gateway.feature` against a real spawned cluster (use the existing `ClusterHarness`).
7. `kiseki-profile --protocol native` driver.
8. Re-measure against A-NG11 targets; gate-1 perf check.

---

## §15 Consequences

**Wins**:
- Native client reaches the gateway-floor headroom: ~5× S3 GET, ~16× S3 PUT compared to today's HTTP path.
- A unified protocol surface for HPC SDK consumers (Python, C++, FUSE eventual). Reduces spec divergence.
- Audit, durability, encryption invariants preserved (every native op flows through the same `GatewayOps` trait that S3/NFS/FUSE use).
- TrustedCompute mode unlocks GPU-direct + zero-copy on namespaces whose operators accept the residual-exposure trade-off.

**Trade-offs accepted**:
- 7 new gRPC RPCs on the data port. Small ops surface; tonic per-service queue isolation bounds inter-service starvation (A-NG9).
- Two crypto-cache disciplines until ADR-04X unifies: existing plaintext cache (TTL+Zeroize+wipe) vs. future DEK cache.
- FUSE keeps the slower HTTP path until the FUSE-native migration ADR.
- Cert issuance becomes an operator concern (SPIFFE format; per-tenant certs; rotation cadence).

**Costs**:
- Roughly 3 days of architect-validated implementation + 1 day of BDD + 0.5 day of gate-2 audit.
- Adversary review (gate-1) likely surfaces 2–4 HIGH findings to round-trip; budget another 0.5 day for amendments.

---

## §16 Alternatives considered

### A1. Add the data-plane methods to ControlService instead of a new service

**Rejected** because ControlService is admin-shaped (org/namespace/device CRUD). Mixing data-plane (millions of ops/sec) with admin (low-rate) creates per-call routing ambiguity and hides the data-plane SLA contract. Two services on the same port keeps the SLAs separate.

### A2. Reuse the S3 HTTP gateway via a "low-overhead profile" (no SigV4)

**Rejected**. Even without SigV4 the HTTP/1 path pays the framing tax that the 2026-05-05 measurement quantified. HTTP/2 multiplexing + protobuf framing is materially cheaper (the gRPC tax estimate of ~30 % overhead is below the HTTP/1 tax of ~80 %).

### A3. Custom binary protocol on raw TCP (no gRPC)

**Rejected**. Possible 5–10 % faster than gRPC but loses the standard tooling (mTLS via tonic, observability via tracing instrumentation, generated client bindings for Python / C++). Throughput delta is outweighed by maintenance cost. Reconsider if a future perf measurement shows tonic itself is the binder.

### A4. Sharding compositions (DashMap) BEFORE shipping ADR-042

**Rejected as a precursor**. The 2026-05-05 spike showed the protocol-layer gap dominates today's regression; native gRPC closes most of it without composition sharding. Sharding becomes a follow-up ADR once we have a measured ceiling that sharding would lift.

### A5. Server-decrypt-only (drop TrustedCompute mode)

**Rejected**. HPC training workloads on tightly-controlled clusters can satisfy the trust assumption, and GPU-direct is the differentiator vs. server-decrypt-only systems. Making TrustedCompute opt-in (with explicit `crypto_shred_policy = best_effort` acknowledgment) is the right balance.

### A6. Lease semantics WITHOUT fencing tokens

**Rejected**. The 2026-05-05 gate-0 adversary review explicitly identified split-brain on partition-heal as an unacceptable correctness gap. Fencing tokens (Lamport pattern) are the standard fix; the audit-trail benefit (which writer issued each op) is also a win.

---

## §17 Status / open items for gate-1

- Architect: this draft is ready for adversary gate-1 review.
- Implementer: do not begin until gate-1 amendments land.
- Operator docs (`docs/admin/native-gateway.md`): pending the implementation, captures cert issuance, env vars, observability scrape paths.
- Build phases (`specs/architecture/build-phases.md`): append the 8 phases from §14.

The optimization backlog (`docs/performance/optimization-backlog.md`) lists the future ADRs that build on top of ADR-042 (DashMap composition sharding, DEK cache, FUSE migration).
