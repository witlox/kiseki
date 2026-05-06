# ADR-042: Native Gateway Data Service — Transport-Pluggable RPC Contract for Native Clients

**Status**: Accepted (gate-1 round-3 PASS — implementer phase 0 unblocked)
**Date**: 2026-05-05 (first draft); 2026-05-06 (transport-pluggable redesign + three rounds of gate-1 amendments)
**Deciders**: Architect (this ADR), Analyst (cycle 2026-05-05 spec layer)
**Adversarial review**: Gate-0 audit on the analyst output landed 2026-05-05 with 1C + 6H + 8M + 4L resolved in place. Gate-1 on the original draft landed 2026-05-05 with 2C + 6H + 8M + 4L resolved (round-2 PASS). The 2026-05-06 transport-pluggable redesign added the contract/binding split; gate-1-redesign-r1 (2026-05-06) found 0C + 4H + 6M + 3L + 3 cross-cutting, all closed in the first amendment pass. Round-2 of the redesign (2026-05-06) found 0C + 1H (cxi handshake DoS) + 6M + 4L + 3 cross-cutting, all closed. Round-3 (2026-05-06) found 0C + 0H + 4M + 4L + 3 cross-cutting, all closed in this revision. **PASS** — phase 0 implementation is unblocked; phases 9–10 (RDMA bindings) require the per-binding hard-close discipline (§3.2.2) and `DrainState` lifecycle (§1.7.1) to be in place.
**Context**: ADR-002 (encryption), ADR-008 (native client fabric discovery), ADR-013 (POSIX semantics), ADR-014 (S3 scope), ADR-015 (observability), ADR-016 (federation), ADR-019 (gateway deployment), ADR-022 (storage backend; rev-2/3/4 fjall migration), ADR-023 (RFC compliance + CRL), ADR-026 (Raft topology), ADR-029 (raw block allocator), ADR-030 (small-file placement), ADR-031 (client-side cache), ADR-032 (async GatewayOps), ADR-033 (shard topology), ADR-035 (drain), ADR-038 (pNFS), ADR-040 (composition store; rev-4 retired write-behind queue), ADR-041 (multiplexed Raft transport — proven length-prefixed-protobuf-over-TLS-over-TCP shape), I-T1/I-T2 (time invariants), I-L5 (composition durability), I-L8 (cross-shard EXDEV), I-K3 (crypto-shred propagation), F-CC3 (cached plaintext exposure window), the analyst output (16 ubiquitous-language terms, A-NG1..A-NG20, I-NG1..I-NG14, F-NG1..F-NG12, `specs/features/native-gateway.feature`).

## Problem

Today every kiseki client path — S3, NFS, FUSE, the Rust SDK marketed as "native" — funnels through HTTP/1 to the S3 gateway (S3, FUSE, native SDK) or raw NFS RPC framing (NFSv3/v4/pNFS). There is no native data-plane RPC path. The 2026-05-06 single-host profile measured S3 GET at 14 635 op/s vs the in-process gateway floor at 192 k op/s GET / 95 k op/s PUT (post ADR-022 rev-2/3/4 fjall sweep) — protocol layer accounts for ~85 % of the gap.

Strategic intent: aim for WekaFS-class single-host throughput AND HPC-fabric-class (Slingshot+Cassini, IB HDR, RoCEv2) cluster aggregate throughput, without sacrificing the security, audit, and durability invariants kiseki commits to. Performance targets in A-NG11 are per-binding (see §14) — gRPC/h2 has a fundamentally higher framing tax than length-prefixed-protobuf-over-TLS-over-TCP, which has a fundamentally higher tax than RDMA bypass.

The deployment shape kiseki targets — 10–100+ nodes for HPC and AI training — is **transport-heterogeneous**. A single deployment may have nodes wired with Slingshot+Cassini NICs (libfabric+cxi), nodes on commodity 10 GbE (TCP-framed-protobuf), and admin-side gRPC control-plane access from auditing tools. Hardcoding gRPC into the wire contract — which the 2026-05-05 first draft did — locks every deployment into the lowest-common-denominator transport.

## Decision

Split the native gateway into two layers:

1. **Service contract** — an abstract RPC surface (verbs, semantics, error categories, control envelope, lease/handle/idempotency primitives) defined once, in transport-agnostic Rust types. Every wire format MUST round-trip the contract types verbatim.
2. **Transport bindings** — concrete wire-format adapters that map the contract onto a specific over-the-wire protocol. Bindings ship in the same binary, gated only by hardware/OS detection at runtime; the operator does not choose at build time.

Land four bindings:

- **gRPC/h2** over rustls-over-TCP — operator-friendly, grpcurl-able, cross-language client tooling. Default fall-back; default for compliance / audit deployments.
- **TCP-framed-postcard** over rustls-over-TCP — same shape as the proven ADR-041 Raft fabric transport. Length-prefixed frames, postcard payload, mTLS via rustls. Default for commodity datacenter / general 10–100 GbE deployments.
- **ibverbs** — direct InfiniBand / RoCEv2 via the kernel rdma-core stack. Optional via runtime probe.
- **libfabric** — Cassini/Slingshot, EFA, generic RDMA via OFI. Optional via runtime probe.

At server startup, the runtime probes every compiled binding, lets each self-disqualify if its prerequisites aren't met (NIC absent, system .so missing, env var disabled, etc.), ranks the survivors by latency class, and listens on **all** that survive. Clients construct against any one or several; the topology advertises which bindings each node serves and the client picks the highest-ranked one mutually supported. An operator can pin a specific binding via `KISEKI_NATIVE_TRANSPORT={grpc|tcp|ibverbs|libfabric|auto}` for diagnosis or compliance audit.

The service contract is the union of:

1. **The 16 ubiquitous-language terms** added by the analyst in the "Native gateway data service" section of `specs/ubiquitous-language.md`.
2. **Invariants I-NG1..I-NG14** in `specs/invariants.md`.
3. **Assumptions A-NG1..A-NG20** in `specs/assumptions.md`.
4. **Failure modes F-NG1..F-NG12** in `specs/failure-modes.md`.
5. **Behavioral scenarios** in `specs/features/native-gateway.feature` (~33 Gherkin scenarios).
6. **The contract surface defined in §1 below**, which fills in the abstract method shapes.

Bindings are defined in §2; runtime selection in §3.

---

## §1 Service contract (transport-agnostic)

The contract is a Rust trait `NativeGatewayService` defined in `kiseki-proto::native_contract`. Every binding implements `NativeTransportServer<NativeGatewayService>` (server side) and `NativeTransportClient<NativeGatewayService>` (client side); no binding redefines verbs, request types, or response types.

### §1.1 Verb categories

```rust
trait NativeGatewayService {
    // ---- Object verbs (commit-on-close, S3-flavored) ----
    async fn put_object(&self, req: PutObjectRequest) -> Result<PutObjectResponse, NativeError>;
    async fn put_object_stream(&self, stream: BoxStream<PutObjectChunk>)
        -> Result<PutObjectResponse, NativeError>;
    async fn get_object(&self, req: GetObjectRequest) -> Result<GetObjectResponse, NativeError>;
    async fn get_object_stream(&self, req: GetObjectRequest)
        -> Result<BoxStream<GetObjectChunk>, NativeError>;
    async fn delete_object(&self, req: DeleteObjectRequest) -> Result<DeleteObjectResponse, NativeError>;
    async fn head_object(&self, req: HeadObjectRequest) -> Result<HeadObjectResponse, NativeError>;
    async fn list_objects(&self, req: ListObjectsRequest) -> Result<ListObjectsResponse, NativeError>;
    async fn lookup_by_name(&self, req: LookupByNameRequest) -> Result<LookupByNameResponse, NativeError>;

    // ---- Multipart (above 64 MiB per-stream cap, A-NG5) ----
    async fn init_multipart(&self, req: InitMultipartRequest) -> Result<InitMultipartResponse, NativeError>;
    async fn put_part(&self, stream: BoxStream<PutPartChunk>) -> Result<PutPartResponse, NativeError>;
    async fn complete_multipart(&self, req: CompleteMultipartRequest)
        -> Result<CompleteMultipartResponse, NativeError>;
    async fn abort_multipart(&self, req: AbortMultipartRequest) -> Result<AbortMultipartResponse, NativeError>;

    // ---- POSIX verbs (handle-token-based, partial-visible-on-fsync, I-NG3) ----
    async fn path_lookup(&self, req: PathLookupRequest) -> Result<PathLookupResponse, NativeError>;
    async fn open(&self, req: OpenRequest) -> Result<OpenResponse, NativeError>;
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, NativeError>;
    async fn read_stream(&self, req: ReadRequest) -> Result<BoxStream<ReadChunk>, NativeError>;
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, NativeError>;
    async fn write_stream(&self, stream: BoxStream<WriteChunk>) -> Result<WriteResponse, NativeError>;
    async fn fsync(&self, req: FsyncRequest) -> Result<FsyncResponse, NativeError>;
    async fn close(&self, req: CloseRequest) -> Result<CloseResponse, NativeError>;
    async fn setattr(&self, req: SetattrRequest) -> Result<SetattrResponse, NativeError>;
    async fn getattr(&self, req: GetattrRequest) -> Result<GetattrResponse, NativeError>;
    async fn read_dir(&self, req: ReadDirRequest) -> Result<BoxStream<ReadDirEntry>, NativeError>;
    async fn mkdir(&self, req: MkdirRequest) -> Result<MkdirResponse, NativeError>;
    async fn unlink(&self, req: UnlinkRequest) -> Result<UnlinkResponse, NativeError>;
    async fn rename_within_shard(&self, req: RenameRequest) -> Result<RenameResponse, NativeError>;

    // ---- Lease-based RMW (I-NG10, I-NG12, I-NG14) ----
    async fn acquire_lease(&self, req: AcquireLeaseRequest) -> Result<AcquireLeaseResponse, NativeError>;
    async fn renew_lease(&self, req: RenewLeaseRequest) -> Result<RenewLeaseResponse, NativeError>;
    async fn release_lease(&self, req: ReleaseLeaseRequest) -> Result<ReleaseLeaseResponse, NativeError>;

    // ---- Encryption boundary (A-NG7, I-NG6a/b/c) ----
    async fn fetch_dek(&self, req: FetchDekRequest) -> Result<FetchDekResponse, NativeError>;
    async fn batch_fetch_dek(&self, req: BatchFetchDekRequest)
        -> Result<BatchFetchDekResponse, NativeError>;

    // ---- Topology discovery (I-NG13, A-NG13) ----
    async fn get_topology(&self, req: GetTopologyRequest) -> Result<TopologyInfo, NativeError>;
}
```

The Rust types are the source of truth. `kiseki-proto` keeps a `.proto` file that mirrors them for the gRPC binding — derived from the Rust types, not the other way around. Postcard-shaped bindings serialize the same Rust types directly.

### §1.2 Control envelope (every mutating call)

```rust
struct ControlFields {
    tenant_id: TenantId,                // canonicalized form
    idempotency_key: Vec<u8>,            // 1..=64 bytes opaque (I-NG1, I-NG5)
    workflow_ref: Option<String>,        // None defaults to "unattributed"
    cache_hint: CacheHint,
    conditional: Option<WriteConditional>, // S3-flavored If-None-Match / If-Match / by version
}

enum WriteConditional {
    IfNoneMatch,
    IfMatch(Etag),
    IfVersionMatch(VersionId),
}

struct CacheHint {
    force_revalidate: bool,
    skip_cache: bool,
    pin_after_read: bool,                // Slurm staging hint
}
```

### §1.3 Response trailer (every response)

Every response carries `topology_version: u64` so clients detect stale topology caches without an extra discovery round-trip. Each binding maps this onto its native trailer/metadata mechanism (gRPC trailing metadata; TCP-framed appended footer; RDMA tagged completion). Clients MUST refresh on mismatch (I-NG13, A-NG13).

### §1.4 Error taxonomy

`NativeError` is a transport-agnostic enum. Bindings map it onto their wire-level error mechanism. Categories:

| Variant | Meaning | Common bindings map to |
|---|---|---|
| `Unauthenticated { reason }` | mTLS / SAN / token failure | gRPC `Status::unauthenticated`, TCP-framed status byte 0x10, ibverbs reject |
| `PermissionDenied { reason }` | tenant mismatch, ACL | gRPC `permission_denied`, status byte 0x11 |
| `InvalidArgument { reason }` | malformed payload | gRPC `invalid_argument`, status byte 0x12 |
| `NotFound { what }` | namespace / inode / composition / object | gRPC `not_found`, status byte 0x13 |
| `AlreadyExists { what }` | If-None-Match: * conflict | gRPC `already_exists`, status byte 0x14 |
| `PreconditionFailed { reason }` | conditional check rejected | gRPC `failed_precondition`, status byte 0x15 |
| `OutOfRange { reason }` | stream cap, byte range | gRPC `out_of_range`, status byte 0x16 |
| `ResourceExhausted { reason }` | tenant stream cap, dedup table cap | gRPC `resource_exhausted`, status byte 0x17 |
| `Aborted { reason }` | partial-chunk-failure, lease fenced | gRPC `aborted`, status byte 0x18 |
| `Unavailable { reason }` | node draining, leader unknown | gRPC `unavailable`, status byte 0x19 |
| `NotLeader { leader_node_id }` | redirect | gRPC `failed_precondition` w/ metadata; status byte 0x1A w/ payload |
| `Internal { reason }` | unhandled bug | gRPC `internal`, status byte 0x1F |

The variant + reason string is the canonical signal; bindings preserve both. The mapping table above is `error-taxonomy.md` material.

### §1.5 Per-stream cap

64 MiB per `put_object_stream` / `get_object_stream` body (I-NG9). Above that, clients use `init_multipart` / `put_part` / `complete_multipart`. Bindings enforce at the wire level; the cap is contract-level and uniform across bindings.

### §1.6 Per-tenant concurrent stream cap

256 in-flight per tenant by default (I-NG11, A-NG14). Configurable per `KISEKI_NATIVE_STREAM_CAP`. Excess returns `ResourceExhausted{native_concurrent_stream_cap}` before any staging buffer is allocated. The cap is enforced contract-level by the gateway; bindings cooperate by passing the tenant_id from authentication into the cap counter before staging memory.

### §1.7 Topology + binding-endpoint types

Topology discovery returns per-node binding endpoints so heterogeneous clusters work cleanly (mixed-NIC deployments, partial RDMA upgrades). The Rust shape (carried verbatim by every binding):

```rust
struct TopologyInfo {
    topology_version: u64,
    nodes: Vec<NodeBindings>,
    shards: Vec<ShardLeadership>,
}

struct NodeBindings {
    node_id: NodeId,
    state: NodeState,                  // Active / Degraded / Failed / Draining / Evicted
    bindings: Vec<BindingEndpoint>,
}

struct BindingEndpoint {
    binding_id: BindingId,             // Grpc | TcpFramed | Ibverbs | Libfabric { provider }
    addr: ListenAddr,                  // "host:port" or fabric-specific descriptor
    latency_class: LatencyClass,       // Standard / Low / Rdma
    drain_state: Option<DrainState>,   // Some only when node.state == Draining
}

struct DrainState {
    quiesce_window_remaining_ms: u64,  // mirrors the §9 lease-tracker view
    accepts_new_work: bool,            // false during quiesce; true (briefly) during graceful release
}

enum BindingId {
    Grpc,
    TcpFramed,
    Ibverbs,
    Libfabric { provider: LibfabricProvider }, // Verbs | Cxi | Efa | Sockets | Tcp
}

enum LatencyClass { Standard, Low, Rdma }
```

**State-vs-bindings rules** (resolves R2-M6 — Draining was undefined):

| `state` | `bindings` | Client behavior |
|---|---|---|
| `Active` | non-empty | dial freely per §3.2 per-edge selection |
| `Degraded` | non-empty | dial; serving but with reduced capacity (operator alarm signal) |
| `Draining` | non-empty, **with `drain_state = Some(_)`** | dial ONLY for in-flight work that already has a lease/handle on this leader (lease-bound writes, in-progress multipart). New opens / new lease acquisitions skip this node and dial a different shard leader. |
| `Failed` | empty | don't dial; clients route around |
| `Evicted` | empty | don't dial; clients route around |

The `drain_state.accepts_new_work` flag carries the fine-grained signal: during the quiesce window it's `false` (drain in progress, finish what's already in flight); during the brief graceful-release phase before the node leaves the cluster it flips to `true` so any straggler in-flight work can complete. Mirrors ADR-035 drain protocol. Clients treat `Some(state) && !state.accepts_new_work` as "in-flight only" — matches §9.1 lease-tracker behavior on draining nodes.

**Close-on-state-change** (resolves R3-O1): on state transition to `Failed` or `Evicted`, clients SHOULD close existing connections to the node within one round-trip of observing the transition (typically the very next response trailer that reflects the new topology version). Implementations that don't enforce this still self-correct because Failed/Evicted nodes don't accept new requests — but the explicit close avoids dangling sockets and frees client-side connection-pool slots immediately.

#### §1.7.1 `DrainState` lifecycle (resolves R3-M3)

The `DrainState.accepts_new_work` flag is owned by `kiseki-control` (the cluster control plane, ADR-021) acting as the drain coordinator per ADR-035. Two transition points:

1. **Drain start** (operator-triggered): `kiseki-control` transitions the node to `Draining` AND sets `accepts_new_work = false` AND starts the quiesce window timer. Topology version bumps. Clients reading the new version stop routing new work to the node; in-flight work continues.

2. **Quiesce expiry OR all-in-flight-done** (whichever first): `kiseki-control` flips `accepts_new_work` to `true` for `KISEKI_NATIVE_DRAIN_GRACEFUL_RELEASE_MS` (default 5000 ms). The brief `true` window lets stragglers (e.g. clients with stale topology that already chose this node before the version bump) complete their final ops without bouncing through the lease tracker. Topology version bumps again on this transition. After the graceful-release window, `kiseki-control` transitions the node to `Evicted` (per the §1.7 state table — bindings empty, clients stop dialing). Topology version bumps a third time.

**Race-window discipline**: every transition bumps `topology_version` BEFORE the state change is observable from any binding's response. Clients reading any version always see consistent `(state, drain_state)` pairs; there is no readable intermediate state where `state == Draining` but `drain_state == None`, or where `state == Evicted` but bindings are non-empty.

**Lease-tracker integration**: when `accepts_new_work == false`, the per-shard lease tracker (§9) refuses new `acquire_lease` requests with `Unavailable{node_draining}` regardless of which binding the request arrived on. Existing leases continue until expiry / release. When `accepts_new_work` flips to `true`, the lease tracker resumes accepting (briefly) for the graceful-release window — but in practice a `Draining → graceful_release → Evicted` cycle is fast enough that few new acquisitions land there.

This needs to be wired before phase 5 (`NativeClient` + `TopologyCache`) implementation; otherwise the client doesn't know which transitions to act on.

### §1.8 `RequestPrincipal` extractor

Binding-specific connection hooks stash the canonical SAN form per-connection at handshake time (§5). Handler-side code reads it ONLY through this trait — never reaches into binding-specific stash locations directly:

```rust
trait RequestPrincipal {
    fn cert_san_canonical(&self) -> &str;
    fn binding_id(&self) -> BindingId;
    fn connection_id(&self) -> ConnectionId;  // for audit + correlation
}
```

Each binding's request-handler entry point packages its stashed SAN into a `RequestPrincipal` impl and passes a reference to `ServerImpl`. Mandate: `ServerImpl` accepts `&dyn RequestPrincipal` everywhere it would otherwise accept binding-specific request metadata. No binding-specific code in the handler.

A unit test in the contract layer round-trips `RequestPrincipal` per binding asserting the same SAN survives. Implementer adds one BDD scenario per binding asserting tenant_id mismatch still rejects (closes the per-binding handoff hazard from gate-1 hot spot 2).

**Enforcement** (resolves R2-L2 — the "ServerImpl reads only via this trait" rule is implementer discipline that no compiler check enforces): the implementer adds a CI gate that fails the build if `kiseki-gateway::native::server` (the crate path holding `ServerImpl`) references `tonic::Request`, `tcp_framed::ConnectionContext`, `cxi::AttestationContext`, or any other binding-specific request-metadata type directly. A simple `cargo run --bin arch-check` rule (the workspace already runs `arch-check` in `make verify`) greps for these forbidden paths within the `ServerImpl` module. A future contributor adding a binding-specific shortcut method to `ServerImpl` fails CI before review.

---

## §2 Transport bindings

Each binding is a module under `kiseki-transport::native::<binding>`. Each implements:

```rust
trait NativeTransportServer<S: NativeGatewayService> {
    fn binding_id(&self) -> BindingId;
    fn probe(&self) -> ProbeResult;     // self-disqualification
    async fn serve(&self, addr: ListenAddr, service: Arc<S>) -> Result<(), TransportError>;
    fn endpoint_advertise(&self) -> EndpointDescriptor; // for topology
}

trait NativeTransportClient<C: NativeGatewayServiceClient> {
    fn binding_id(&self) -> BindingId;
    async fn connect(&self, endpoint: &EndpointDescriptor)
        -> Result<C, TransportError>;
}
```

`ProbeResult` is `Available { latency_class }` or `Unavailable { reason }`. `BindingId` is an enum: `Grpc | TcpFramed | Ibverbs | Libfabric`. The runtime ranks `Available` bindings by `latency_class` (see §3).

### §2.1 gRPC/h2 over rustls-over-TCP

**When**: default fall-back. Always available on Linux/macOS/Windows. Required for compliance audits where operators want grpcurl introspection or for cross-language clients (Python / C++ via grpc-tools).

**Wire**: standard tonic + prost, protobuf v3. The `.proto` file lives at `specs/architecture/proto/kiseki/v1/native_gateway.proto`. Generated Rust bindings in `kiseki-proto` use prost. The `gateway_data.rs` codegen is the implementation; the `.proto` is derived.

**Auth**: mTLS via tokio-rustls. SAN canonicalization at a tonic interceptor (see §5).

**Listen address**: `KISEKI_NATIVE_GRPC_ADDR`, default 9100.

**Latency class**: `Standard` (~5–7× tax over the in-process floor on commodity hardware; primarily h2 framing + tonic).

**Probe**: trivially `Available` everywhere. Self-disqualifies only if `KISEKI_NATIVE_GRPC_ADDR=disabled`.

### §2.2 TCP-framed-postcard over rustls-over-TCP

**When**: default for commodity datacenter deployments. Same wire shape as the ADR-041 Raft fabric transport — proven, reusable, low-overhead.

**Wire**:
- Outer frame: `[length: u32 BE][version: u8][postcard(RpcEnvelope)]`
- `RpcEnvelope` = `(request_id: u64, verb_tag: String, payload_bytes: Vec<u8>)`
- Inner payload: postcard-encoded request type
- Response: `[length: u32 BE][version: u8][status: u8][postcard payload bytes]`
- One TCP connection per (client, node); requests multiplex via `request_id` like h2 streams. Connection-per-node not connection-per-stream — same multiplex shape h2 provides without h2's framing tax.

**Auth**: mTLS via tokio-rustls (same setup as the Raft fabric). SAN canonicalization at the connection-acceptance hook before the first frame is parsed.

**Listen address**: `KISEKI_NATIVE_TCP_ADDR`, default 9101.

**Latency class**: `Low` (~1.5–2× tax over the in-process floor; per the ADR-041 fabric measurements).

**Probe**: trivially `Available` everywhere. Self-disqualifies only if `KISEKI_NATIVE_TCP_ADDR=disabled`.

**Migration value**: this is the binding that gives kiseki its native-class throughput on commodity hardware without committing to RDMA. The 5–7× h2 tax measured against the rev-4 in-process-persistent floor is the gap this binding is designed to close.

### §2.3 ibverbs (direct InfiniBand / RoCEv2)

**When**: clusters with InfiniBand or RoCEv2-capable NICs (Mellanox/NVIDIA ConnectX, Intel/Cornelis Omni-Path with RoCE, etc.). The default for in-rack HPC training clusters that don't have Slingshot.

**Wire**: TBD in implementation phase — RDMA verbs `send`/`recv` for control messages, `rdma_read`/`rdma_write` for bulk payload bytes. Postcard-encoded request envelope on the send queue; response on the receive queue. Memory pre-registration for the bulk path.

**Auth**: mTLS-over-rdma-cm. The kernel rdma-cm subsystem ≥ Linux 6.x exposes a TLS extension that runs the standard handshake before the QP is fully promoted to RTR. The handshake uses the cluster CA chain configured for the gRPC and TCP-framed bindings — operators don't manage two PKIs. SAN canonicalization runs against the validated cert in the QP-establishment ULP. **Required kernel**: Linux ≥ 6.4 with rdma-core ≥ 50.0 (the version where rdma-cm TLS landed mainline). The probe verifies kernel version + rdma-cm capability via `RDMA_CM_TLS_CAPS` queries; self-disqualifies on older kernels with `Unavailable { reason: "rdma-cm TLS unsupported by kernel" }`.

**Listen address**: `KISEKI_NATIVE_IBVERBS_DEV` (HCA device, e.g. `mlx5_0`) + `KISEKI_NATIVE_IBVERBS_PORT` (1).

**Latency class**: `RDMA` (sub-microsecond; RDMA verbs bypass the kernel entirely on data-path).

**Probe**:
- `cfg(target_os = "linux")` only; on other OSes self-disqualifies.
- `dlopen` of the system library at an absolute path. The probe searches a fixed list of probable paths in order, taking the first that satisfies the ownership/permissions check (resolves R2-M2 — Debian-only `/usr/lib/x86_64-linux-gnu/` default failed RHEL/SUSE/Alpine + ARM64/POWER hosts):
  ```
  /usr/lib/${arch}-linux-gnu/libibverbs.so.1     (Debian/Ubuntu)
  /usr/lib64/libibverbs.so.1                      (RHEL/SUSE/Rocky)
  /usr/lib/libibverbs.so.1                        (Alpine, others)
  ```
  `${arch}` is determined at probe time via `cfg!(target_arch=...)` (`x86_64`, `aarch64`, `powerpc64le`). The env var `KISEKI_NATIVE_IBVERBS_LIB` is the operator escape hatch when the auto-search fails (custom install paths, container layouts, etc.).
- Probe verifies the resolved file is owned by root (uid 0) and not group/world-writable; refuses to dlopen otherwise. Audit-logs the resolved absolute path so a path-injection attempt leaves a trace. (Closes round-1 M1 — dlopen of bare names via `LD_LIBRARY_PATH` would expose a supply-chain attack surface; absolute-path + ownership/perms check eliminates it.)
- On dlopen failure across all candidate paths or path-validation failure, self-disqualifies with `Unavailable { reason: "libibverbs not present | not root-owned | path rejected" }` and includes the searched paths in the audit log so operators can diagnose.
- Probe `/sys/class/infiniband/*` for at least one device with an Active port and either `link_layer == InfiniBand` (IB) or RoCEv2 GID type (Ethernet + RoCEv2 GID).
- Self-disqualifies on no usable port.

**Build deps**: `libibverbs-dev` (header files for the FFI shim) on the build host. Not feature-gated — the FFI shim is small (~2–3K SLOC) and ships in every binary. `kiseki-transport` build-script verifies `libibverbs-dev` is installed on Linux build hosts and emits a clear error if missing (HPC&AI storage builds require it; not optional).

**Supported architectures**: `x86_64`, `aarch64` (ARM HPC — Grace Hopper, A64FX, Ampere Altra, Cobalt), `powerpc64le`. Any Linux target where Rust + libibverbs build. The dlopen path-search list above covers all three via `${arch}` substitution; `cargo` builds the FFI shim cleanly on any tier-1/tier-2 Rust Linux target.

### §2.4 libfabric (Cassini/Slingshot via cxi, generic OFI)

**When**: HPE/Cray Slingshot+Cassini clusters (`cxi` provider) and any deployment with libfabric's verbs provider installed. libfabric's provider model abstracts over multiple physical fabrics; v1 supports the providers listed in §2.4.4.

**Wire**: libfabric's tagged-message API maps onto the contract's `request_id` model — tag = request_id, message body = postcard-encoded request envelope. Memory pre-registration for bulk paths. The ULP looks similar to ibverbs but goes through libfabric's higher-level API; provider-specific tuning (`FI_MSG`, `FI_RMA`, `FI_ATOMIC` capability negotiation) is handled at startup.

**Listen address**: `KISEKI_NATIVE_LIBFABRIC_PROVIDER` (auto-pick from `fi_getinfo()` ranked per §2.4.4, or operator-pinned), `KISEKI_NATIVE_LIBFABRIC_DOMAIN` (NIC selection).

**Latency class**: `Rdma` on cxi/verbs providers; `Standard` on sockets/tcp fall-back providers.

**Probe**:
- `cfg(target_os = "linux")` only.
- `dlopen` of the system library — same auto-search list shape as §2.3 ibverbs (Debian, RHEL, Alpine layouts × `${arch}`), env-var override `KISEKI_NATIVE_LIBFABRIC_LIB`. Path validation: root-owned, not group/world-writable, audit-logged absolute path.
- Call `fi_getinfo()` filtered for the kiseki capability set. If at least one supported provider returns endpoints, we're available. Self-disqualifies on empty results.
- Provider-specific NIC presence checks (e.g. `/sys/class/net/.../device/cxi/` for cxi).

**Build deps**: `libfabric-dev` on the build host. Not feature-gated. `kiseki-transport` build-script verifies presence on Linux build hosts.

**Supported architectures**: same as §2.3 — `x86_64`, `aarch64`, `powerpc64le`. libfabric upstream supports all three; the cxi provider in particular targets HPE Cray Slingshot which ships on both `x86_64` (EX-class) and `aarch64` (Grace-Hopper-class) compute nodes.

#### §2.4.1 Per-provider trust matrix

| Provider | Trust anchor | v1 status |
|---|---|---|
| **verbs** (IB / RoCE via libfabric) | mTLS-over-rdma-cm — inherits §2.3 ibverbs flow | shipping |
| **cxi** (Slingshot+Cassini) | cxi auth-key (cluster-scoped, defense-in-depth) + `CxiAttestationEnvelope` (per-tenant, application-layer) — see §2.4.2 | shipping |
| **efa** (AWS EFA) | AWS-IAM integration | **deferred to follow-up ADR** — no validation hardware, IAM bridging not yet designed |
| **sockets**, **tcp** (fall-back) | mTLS via rustls (same as TCP-framed binding) | dev-only — auto-detect ranks below the TCP-framed binding for the same fabric so this is reached only via explicit pin |

Providers in `Available` outcome are ranked: cxi > verbs > sockets/tcp. Operator pins via `KISEKI_NATIVE_LIBFABRIC_PROVIDER=<provider>`.

#### §2.4.2 cxi attestation envelope

The cxi provider's auth-key layer establishes that a connection comes from a known cluster member; it doesn't carry per-tenant identity. `CxiAttestationEnvelope` is a one-shot application-layer attestation sent as the first message on every cxi connection, providing the tenant-identity layer that matches I-NG1.

```rust
struct CxiAttestationEnvelope {
    schema_version: u8,             // bump on incompatible changes (defaults to 1)
    cert_chain_der: Vec<Vec<u8>>,    // client's full x.509 chain (DER-encoded)
    canonical_san: String,           // canonical SAN URI from cert_chain_der[0]
    issued_at: SystemTime,           // ±30 s replay window vs server HLC
    nonce: [u8; 32],                 // CSPRNG, per attestation
    signature: Vec<u8>,              // ECDSA-P256 over the canonical message
}
```

**Canonical message signed**:
`b"kiseki/cxi-attestation/v1" || schema_version || canonical_san_bytes || issued_at_be8 || nonce`

**Server-side validation flow** (executed in the cxi connection-acceptance hook before any other RPC frame):

1. Read first message; if not a valid `CxiAttestationEnvelope` decode, close with `Unauthenticated{cxi_attestation_missing}`. If decode succeeds but `schema_version > 1` (the current supported version), close with `Unauthenticated{cxi_attestation_schema_too_new}` — fail-closed against operators running an older binary against a newer client (resolves R2-L3, matches the ADR-022 fjall encoding + ADR-040 composition encoding + ADR-041 raft transport version-byte discipline).
2. Validate `cert_chain_der[0]` chains to the cluster CA (same trust root every other binding's mTLS uses).
3. Validate cert not expired and not on the CRL/OCSP revocation list (existing ADR-023 plumbing).
4. Run the SAN canonicalization helper on `cert_chain_der[0]`; assert byte-equal match with `canonical_san` field. Mismatch → `Unauthenticated{cxi_san_mismatch}`.
5. Validate `issued_at` within ±30 s of server HLC. Drift outside window → `Unauthenticated{cxi_attestation_clock_skew}`.
6. Verify ECDSA signature against the cert's public key. Failure → `Unauthenticated{cxi_attestation_signature_invalid}`.
7. Per-(canonical_san, nonce) bloom filter for last 60 s. Duplicate nonce inside window → `Unauthenticated{cxi_attestation_replay}`.
8. Stash `canonical_san` per-connection; subsequent messages on this connection use the stashed principal via `RequestPrincipal` (§1.8).

**Replay-protection bloom filter** is per-server, sized for 1 M entries × 60 s window (≈16 MiB at 0.1% FPR). FPR-induced rejections are bounded; clients retry with a fresh nonce.

**Why this is sound**:

- **Trust anchor is the cluster CA** — identical to mTLS bindings. Operators don't manage two PKIs.
- **Possession of cert private key proven** by the signature, semantically equivalent to TLS client-auth.
- **Replay protection** via `(timestamp ±30 s) ∩ (per-key nonce bloom 60 s)`.
- **Revocation** via the existing CRL/OCSP path; same `KISEKI_CERT_REVAL_INTERVAL_MS` cadence as mTLS bindings.
- **Forward secrecy** — not provided by sign-once. The threat model on a private HPC fabric (cxi auth-key already validated cluster membership at the provider level) doesn't require it. Operator concern about passive-replay defense in this position should escalate to the cluster security architect; if it lands, a follow-up ADR adds an X3DH-style ephemeral-key extension.

**Cross-node replay surface** (acknowledged residual, gate-1 round-2 R2-M4): the bloom filter is per-server. A captured envelope can be replayed on every other cluster node within the 60-s window — one new connection per node. The exposure is bounded:

- Connection-establishment only — no application-layer messages succeed without the cert's matching private key (every signed/keyed verb still gates on it).
- Idle connections close on `KISEKI_NATIVE_IDLE_TIMEOUT_MS` (default 5 min); attacker dwell time is bounded.
- N nodes × 1 establishment per replay = O(N) attack volume per captured envelope, not per-replay-attempt.

A cluster-wide nonce store (Raft-replicated seen-nonces, one round-trip per cxi connect) is the alternative; rejected for v1 because the per-establishment Raft cost outweighs the bounded residual exposure on an internally-trusted HPC fabric. Captured as an alternative for the future-ADR list; v1 ships per-server.

**Schema versioning**: §2.4.2 step 1 rejects `schema_version > 1` with `Unauthenticated{cxi_attestation_schema_too_new}` — same fail-closed discipline as the rest of the codebase (ADR-022 fjall encoding, ADR-040 composition encoding, ADR-041 raft transport version byte).

**New invariant I-NG25 (proposed)**: *On the libfabric/cxi binding, every connection MUST present a valid `CxiAttestationEnvelope` as its first message before any other RPC frame. The server MUST close connections that fail attestation with `Unauthenticated{cxi_attestation_*}` before any application data is processed.*

**Audit**: every cxi connection emits an audit event with the canonical SAN + connection_id + cert serial number on attestation success or failure (security-failure event on failure).

#### §2.4.2.1 cxi handshake DoS mitigations (resolves R2-H1)

The `CxiAttestationEnvelope` validation flow runs ECDSA-P256 verify (~50 µs) + X.509 path validation (100 µs–10 ms) **pre-auth** — before the connection has been authenticated to a tenant. cxi's auth-key layer authenticates connections at the libfabric provider as cluster members, but **a compromised peer node IS a cluster member** by construction. That's exactly the threat model per-tenant attestation is supposed to bound.

A compromised peer flooding the cxi listener with malformed envelopes costs the server ~1 ms of CPU per reject; ~1 000 envelopes/sec saturates a server core before any legitimate work runs. Without the controls below, the cxi binding is a pre-auth CPU DoS surface against compromised internal peers.

**Required mitigations** (all enforced at the cxi connection-acceptance hook, before §2.4.2 step 1 runs):

1. **Per-source connection-establishment rate limit**: at most `KISEKI_NATIVE_CXI_ATTEST_RATE_PER_SOURCE` attestation attempts per source (cluster peer node identifier from the cxi auth-key context) per 60-s sliding window. Default 100. Excess returns `ResourceExhausted{cxi_attestation_rate_limit}` without parsing the envelope. Implementation: `dashmap::DashMap<SourceId, RateBucket>` — sharded so per-source rate state doesn't contend across sources.
2. **Envelope size caps** (enforced at frame-decode boundary, before postcard parses):
   - `CxiAttestationEnvelope` total body ≤ **16 KiB**.
   - `cert_chain_der` total ≤ **8 KiB** (5 certs × ~1.6 KiB typical for ECDSA-P256 with reasonable distinguished-name lengths).
   - `nonce` exactly 32 bytes (the `[u8; 32]` type enforces; reject postcard variants that disagree).
   - `signature` ≤ **128 bytes** (DER-encoded ECDSA-P256 max ~72 bytes; cap with headroom).
   Oversized → `InvalidArgument{cxi_attestation_oversize}` and immediate connection close.
3. **Failed-attestation per-source cooldown**: after `KISEKI_NATIVE_CXI_ATTEST_FAIL_THRESHOLD` (default 10) consecutive failures from the same source, the source is parked for `KISEKI_NATIVE_CXI_ATTEST_COOLDOWN_MS` (default 60 000 ms). New connections from the source during cooldown return `ResourceExhausted{cxi_attestation_source_cooldown}` immediately. Counter resets on the first successful attestation.
4. **Bounded ECDSA-verify thread pool**: §2.4.2 step 6 (signature verify) runs on a dedicated `tokio::task::spawn_blocking` pool with `KISEKI_NATIVE_CXI_VERIFY_WORKERS` (default 16) workers and a bounded queue of `KISEKI_NATIVE_CXI_VERIFY_QUEUE_DEPTH` (default 64). Pool overflow returns `ResourceExhausted{cxi_attestation_queue_full}` immediately — fails fast rather than letting verify work crowd out the runtime's reactor.

**Sizing rationale** (resolves R3-L3): the default 64 queue depth assumes peak legitimate establishment rate ≤ 100 cxi connections/sec across the cluster, which is generous for steady-state HPC workloads (mostly long-lived connections). On larger clusters or workloads with high churn (e.g. transient batch jobs), raise via `KISEKI_NATIVE_CXI_VERIFY_QUEUE_DEPTH`; the in-memory cost is ~256 bytes per queued verify (envelope reference + waker), bounded at `queue_depth × workers` slots. Operators alarm on `kiseki_native_cxi_verify_queue_depth ≥ 0.8 × queue_depth` sustained for > 30 s as the early-warning signal.

**Metrics** (additions to §12.1):

- `kiseki_native_cxi_attestation_throttled_total{source, reason ∈ {rate_limit, source_cooldown, queue_full, oversize}}` — surface compromised-peer abuse before it cascades. **Cardinality note** (R3-L2): on clusters with > 1000 nodes the `source` label may overwhelm Prometheus storage (one timeseries per peer × 4 reasons). Operators on mega-clusters can drop the `source` label at scrape time and rely on the throttled-rate aggregate; the per-source attribution becomes operator-on-demand via a dedicated debug endpoint rather than always-scraped metric. Documented in operator docs.
- `kiseki_native_cxi_verify_queue_depth` (gauge) — operators alarm on sustained ≥ 0.8 × queue_depth (see sizing rationale above).
- `kiseki_native_cxi_verify_duration_seconds` (histogram) — per-verify latency for the bounded pool.

**Why these are sufficient**: rate-limit + cooldown bounds the attack volume per source; size caps bound the per-envelope cost; bounded verify pool isolates verify CPU from the runtime. A compromised peer can still consume its rate-limit budget on each interval, but the envelope of damage is bounded and observable. Architect's compliance requirement: no cxi binding deployment ships without all four controls.

#### §2.4.3 efa provider — deferred

AWS EFA's IAM-based identity model is materially different from the cluster-CA trust root used for cxi/verbs/mTLS. Designing the AWS-IAM ↔ kiseki-tenant mapping, the SRD-with-IAM auth flow, and the operational story for credential rotation is its own ADR. Scope-out for v1; the libfabric binding's probe self-disqualifies the efa provider with `Unavailable { reason: "efa provider requires AWS-IAM integration; deferred to ADR-04X" }` even when `fi_getinfo()` would otherwise return it.

#### §2.4.4 Provider ranking + selection

Auto-detect ranking inside libfabric: cxi > verbs > sockets/tcp (efa excluded per §2.4.3). The selector picks the highest-ranked Available provider. Operator pin `KISEKI_NATIVE_LIBFABRIC_PROVIDER=<provider>` overrides; pinning to a deferred provider (efa) returns `TransportError::PinnedProviderDeferred` at startup.

---

## §3 Runtime binding selection

### §3.1 Server-side startup — three phases

Startup is split into three explicit phases with defined error handling at each. The implementer follows this ordering verbatim.

#### Phase 1 — Probe (read-only)

For each compiled binding, call `probe()` with a per-binding timeout. The probe is read-only: it dlopen's system libraries (path-validated per §2.3 / §2.4), inspects `/sys/class/infiniband/*` or `fi_getinfo()`, but does NOT bind ports or acquire fabric resources. On `Available { latency_class }`, record `(binding, latency_class, listen_addr)`. On `Unavailable { reason }` or timeout (`probe_timeout_exceeded`), log at info level and skip.

**Probe timeout** is `KISEKI_NATIVE_PROBE_TIMEOUT_MS` per binding, default **3000** (3 s) — tightened from the originally-drafted 5 s default to keep total worst-case startup well under the Kubernetes startup-probe defaults (resolves R2-M3). With four bindings × 3 s = 12 s phase-1 worst case; phase 3 listener-spawn adds 2–5 s for RDMA fabric init; total worst-case startup ≈ 17–20 s, well inside the typical 30 s `startupProbe` `failureThreshold × periodSeconds` window. Operators on slow / contended hosts (`dlopen` taking seconds under load) raise the env var; operator docs prominently document the recommended k8s `startupProbe` setting (≥ 60 s window for safety margin).

Probes are sequential — never parallel. dlopen contention on heavily loaded hosts is real (libfabric in particular has been observed to take seconds under provider-discovery contention); parallel probes would compound it.

**Probe-duration observability**: emit `kiseki_native_binding_probe_duration_seconds{binding}` (histogram) on every probe completion regardless of outcome, so operators can right-size the timeout per their environment based on measured data rather than guessing.

#### Phase 2 — Port-conflict check

Assert all recorded `listen_addr`s are pairwise distinct across the `Available` set. Collisions (operator misconfig, e.g. `KISEKI_NATIVE_GRPC_ADDR` and `KISEKI_NATIVE_TCP_ADDR` both pointing at the same `host:port`) are fatal:

```
[transport.native] FATAL: port collision between bindings A and B at <addr>; refusing to start
```

Exit code 2.

#### Phase 3 — Listener spawn

For each `Available` binding, spawn its listener (TCP `bind()`, RDMA QP setup, etc.). Per-binding bind-failure (e.g. another process holds the rdma device) is **not fatal** for the whole server: that single binding is downgraded to `Unavailable { reason: "listener spawn failed: <err>" }` and logged at warn level. The server continues with the bindings that did spawn.

The server only fails to start (exit code 3) if **all** bindings end up `Unavailable` after phase 3.

#### Banner

After phase 3, emit the structured startup banner:

```text
[transport.native] available bindings (in priority order):
  1. libfabric/cxi    (latency_class=Rdma, addr=cxi0:0,    Slingshot+Cassini detected)
  2. ibverbs          (latency_class=Rdma, addr=mlx5_0:1,  InfiniBand HDR detected, rdma-cm TLS ok)
  3. tcp-framed       (latency_class=Low,  addr=10.0.0.42:9101)
  4. grpc-h2          (latency_class=Std,  addr=10.0.0.42:9100)
[transport.native] all 4 listening; clients select per their topology
[transport.native] override available via KISEKI_NATIVE_TRANSPORT={grpc|tcp|ibverbs|libfabric|auto}
```

#### Operator pin

`KISEKI_NATIVE_TRANSPORT=<binding>` pins to a single binding. The other bindings still execute Phase 1 + Phase 2 (collision detection still relevant for diagnosis) but Phase 3 only spawns the pinned binding's listener. Pinning to an `Unavailable` binding is fatal at startup with `TransportError::PinnedBindingUnavailable` and exit code 4.

`KISEKI_NATIVE_TRANSPORT=auto` is the literal string for "no pin"; equivalent to leaving the env var unset. Documented in operator docs as the recommended setting.

Server-side metric `kiseki_native_binding_pinned_total{binding}` increments at server startup whenever a pin is in effect, so dashboards surface deployments that have been pinned (and may have forgotten to remove the pin after diagnosis).

**Tenant policy**: env-var pin is operator-side only. Per-tenant binding *requirement* (e.g. "tenant X must use mTLS-via-TCP-framed only") is out of scope for ADR-042 v1; deferred to a future operations ADR.

### §3.2 Client-side selection (per-edge)

Native client construction (`NativeClient::connect(seed_addrs)`) does the parallel of the server probe:

1. Each compiled binding's client probes the local environment (NIC presence, library presence — same path-validation discipline as server probes). Bindings that self-disqualify locally are dropped.
2. The client connects to a seed node via any transport that responded; reads the topology.
3. The topology advertises, per node, which bindings the node serves at which addresses (`NodeBindings` from §1.7).
4. **Per-edge selection**: each client → node connection picks **independently**, taking the highest-ranked latency_class mutually supported by (a) the local environment, (b) the bindings that node advertises. A multi-node operation may use libfabric/cxi to nodes 1–4 and TCP-framed to nodes 5–10 in the same session — the contract types are identical, so request_id + idempotency_key carry across; the binding selection is per-connection, not per-session.
5. If `KISEKI_NATIVE_TRANSPORT=<binding>` is set, the client pins that binding for **every** edge. Pinning to a binding that no needed node serves returns `TransportError::PinnedBindingUnavailable` for that operation. Operators can scope pins narrowly (per-process env) for diagnosis.

Client-side ranking matches server-side: `Rdma > Low > Standard`. `auto` is the literal string for "no pin".

#### §3.2.1 Connection-pool lifecycle on binding-set changes (resolves R2-M5)

A client maintains a connection pool keyed by `(node, binding)`. When a topology refresh signals a binding-set change for a node — the node's binding listener crashed (§3.4), or an operator removed an RDMA NIC and restarted, or the node downgraded its binding-set for any reason — the client may end up holding open connections on bindings the node no longer serves.

**Diff trigger** (resolves R3-M4): the client runs the binding-set diff **on every observed `topology_version` change**, regardless of source. Sources are response-trailer mismatches (§6 — already on the response path for staleness detection, no extra branching cost) AND explicit `get_topology` polls. The trigger is "version mismatch detected"; the source is irrelevant. This guarantees bound-by-one-RTT detection across implementations and avoids cross-implementation behavior divergence.

**Graceful drain protocol**:

1. On topology version bump that changes node N's `bindings` set, the client diffs against its open connections to N.
2. Each open connection on a now-removed binding enters **drain mode**:
   - No new requests dispatch to it. The client routes new work for N to the highest-ranked still-available binding for N (per the §3.2 per-edge selector, re-evaluated against the new topology).
   - In-flight requests run to completion (success, error, or timeout — same retry semantics as the contract layer's normal failure handling).
   - The connection closes when the in-flight count reaches zero.
3. **Drain budget**: each draining connection has at most `KISEKI_NATIVE_DRAIN_BUDGET_MS` (default 30 000 ms) to finish. Past budget, hard-close the connection per §3.2.2. Remaining in-flight requests fail with `Aborted{binding_drain_timeout}`; clients retry via `idempotency_key` (A-NG10), which deduplicates against the original request's outcome on whichever binding ultimately succeeds.

**In-flight definition** (resolves R3-M2): for drain accounting, a request is **in-flight** when its RPC envelope has been written to the binding's wire — meaning:
- gRPC/h2: the h2 stream has advanced past `HEADERS` (the `Frame::Headers` has been written to the codec's send buffer).
- TCP-framed: `tokio::AsyncWriteExt::write_all` for the request frame has returned `Ok(())` (bytes are in the kernel's TCP send buffer or beyond).
- ibverbs / libfabric: `ibv_post_send` / `fi_send` has returned successfully (request is on the send queue).

A request that's only **client-queued** (still in the connection's application-level send buffer at drain start, not yet handed to the binding's wire writer) is NOT in-flight. Client-queued requests are immediately re-dispatched on a fresh connection using the same `idempotency_key` — the server never saw them, so the dedup table short-circuits naturally on the receiving side. This collapses the two cases cleanly: queued-but-unsent → re-issue, in-flight-but-incomplete → drain-to-completion.

**Why drain rather than abort-and-retry immediately**: lease-bound writes carrying a fencing_token are most efficient when they complete on the binding that issued them — the per-(tenant, namespace, inode) `fencing_token` survives across bindings (it's part of the contract types), but eager retry on a different binding wastes the in-flight work. The drain budget is the cost ceiling.

**Long-running request caveat** (R3-L1): the default 30 s budget is tuned for typical request sizes (≤ 64 MiB streaming, sub-second multipart parts). Long-running multi-GB streaming uploads exceeding this budget will hard-close on drain and require client-side restart of the multipart session via the same `idempotency_key` (the existing in-flight parts are reclaimed by the orphan-fragment scrub per F-NG12; the multipart's `upload_id` is still valid post-restart). Operators on workloads with multi-GB streaming uploads should raise the budget via `KISEKI_NATIVE_DRAIN_BUDGET_MS` — the cost is a longer drain tail at upgrade time. A future amendment may add resume tokens (§13 out-of-scope item 2) for streaming-bandwidth-resume, but pre-1.0 the idempotency_key restart path is sufficient.

**Client metric**: `kiseki_native_client_binding_drain_total{binding, reason ∈ {listener_crashed, binding_set_change, hard_close_budget}}` so dashboards distinguish "transient flap" (listener_crashed → restart-and-recover) from "operator downgrade" (binding_set_change) from "real timeout" (hard_close_budget).

#### §3.2.2 Per-binding hard-close discipline (resolves R3-M1)

"Hard-close" semantics differ materially across bindings. For TCP-based bindings, kernel cleanup is reliable; for RDMA bindings, wrong-order or skipped cleanup leaks kernel-side resources (memory regions stay pinned, QP slots stay allocated, completion-queue entries stay queued). On a node that goes through many binding restarts in a session, leaked QPs accumulate until NIC limits are hit.

Per-binding hard-close steps:

| Binding | Hard-close steps |
|---|---|
| **gRPC/h2** | Drop the tonic channel; tokio-rustls handles the TLS close-notify; TCP socket close (kernel handles cleanup). |
| **TCP-framed** | Drop the framed codec; rustls TLS close-notify; TCP socket close. |
| **ibverbs** | (1) Stop sender (no more `ibv_post_send`); (2) drain the CQ for outstanding WCs, dropping any results past the budget; (3) `ibv_modify_qp` to ERR; (4) `ibv_destroy_qp`; (5) `ibv_dereg_mr` for each pre-registered MR; (6) `ibv_destroy_cq`; (7) `ibv_dealloc_pd`. Each step's error is logged but doesn't abort the cleanup chain — leak-on-error is preferred to half-cleaned state. |
| **libfabric** | Provider-specific via `fi_close()` on the FID hierarchy in **reverse-creation order**: endpoint → completion queue → memory regions → access domain → fabric. cxi and verbs differ on whether MR deregistration is automatic on `fi_close(ep)` — implementer documents per provider in `docs/admin/native-gateway.md`. |

**Stress-test mandate**: phase 9–10 implementer tests include a 1000-close-cycle stress test on each RDMA binding. After 1000 close cycles, `ibv_devinfo` (or libfabric provider equivalent) MR/QP counters must be back at baseline ± epsilon. Drift indicates a leak; gate-2 audit blocks the release until the leak is fixed.

This is implementer-phase work; the spec names the discipline so implementer doesn't ship a leaky cleanup path.

### §3.3 Heterogeneous deployments

Two cases the design must handle cleanly:

**Mixed-NIC clusters**: some nodes have Slingshot; some don't. Topology advertises per-node bindings. The client picks the best mutually-supported binding for each direct dial: it may speak libfabric/cxi to nodes 1–4 (Slingshot peers) and TCP-framed to nodes 5–10 (commodity peers) within the same session.

**Cross-WAN clients**: clients reaching from outside the HPC fabric (operator laptop, audit pipeline, public cloud → on-prem cluster) use gRPC/h2 even if the cluster's preferred binding is libfabric/cxi. The topology marks RDMA endpoints as fabric-local; clients dialing from outside the fabric automatically fall back to gRPC/h2.

### §3.4 Failure model

If a binding's listener crashes mid-flight (e.g. libfabric provider error), the runtime:

1. Removes the binding from the topology advertisement.
2. Bumps `topology_version` so connected clients refresh.
3. Logs at error-level and emits `kiseki_native_binding_listener_crashed_total{binding}`.
4. Spawns a backoff-restart for the failed listener (default 5 s, capped at 60 s). On successful restart, re-advertises and bumps version.

Clients seeing a binding disappear from topology gracefully fall back to the next-best mutually-supported binding for the affected nodes (per the per-edge selection in §3.2).

**Topology-refresh attribution**: binding-restart-induced version bumps are visible to clients as routine refreshes; they don't indicate shard movement. Metric `kiseki_native_topology_refresh_total{reason}` carries `reason ∈ {leader_change, split, merge, binding_restart, binding_set_change, other}` so dashboards distinguish "shard moved" from "binding flapped". Operators correlate against `kiseki_native_binding_listener_crashed_total{binding}` to root-cause refresh storms.

### §3.5 Probe-time edge cases

Documented behavior for the corners H2 surfaced:

- **`dlopen` timeout**: each system-library `dlopen` runs inside the per-binding 5 s probe budget. Timeout returns `Unavailable { reason: "dlopen timeout" }`; the binding is skipped; phase 1 continues.
- **Container / pod environments with masked `/sys`**: the probe distinguishes "library loadable but `/sys` evidence missing" (legitimate self-disqualify) from "library not loadable" (reasons differ in the `Unavailable` message). Operators inspecting probe logs can tell which side of the bridge is missing.
- **`fi_getinfo()` hang**: bounded by the same 5 s probe budget; libfabric's API doesn't expose a per-call timeout, so the implementer wraps it in a `tokio::time::timeout` on a `spawn_blocking` worker.
- **dlopen path-validation failure**: the probe rejects with `Unavailable { reason: "dlopen path failed validation: <details>" }` and audit-logs the violating path. Operators configuring custom paths via `KISEKI_NATIVE_*_LIB` env vars see clear failures rather than silent fallthrough.

### §3.6 Latency-class ranking is coarse by design

The class-based ranking (`Rdma > Low > Standard`) is deliberately coarse for v1. It hides:

- Per-NIC bandwidth variance (a 100 GbE link with TCP-framed beats 1 GbE with TCP-framed dramatically; ranking ignores).
- Provider variance inside `Rdma` (cxi vs verbs vs sockets-fall-back can be orders of magnitude apart).
- Per-tenant workload bias (bandwidth-bound vs latency-bound).

The v1 escape hatches are operator pin (`KISEKI_NATIVE_TRANSPORT`) and per-binding `KISEKI_NATIVE_LIBFABRIC_PROVIDER`. **Adaptive binding selection** — measured-latency probes at startup or periodically, weighted ranking, per-tenant tier-pinning — is captured in `docs/performance/optimization-backlog.md` as a future-ADR item ("adaptive binding selection"), not a v1 obligation.

---

## §4 Module placement

| Crate | Addition |
|---|---|
| **kiseki-proto** | Defines the Rust types backing the contract (`PutObjectRequest`, `OpenResponse`, `NativeError`, etc.) — transport-agnostic. The `.proto` file lives here as the gRPC binding's wire format, derived from the Rust types. |
| **kiseki-transport** | Existing crate gains a `native::` module with submodules `grpc`, `tcp_framed`, `ibverbs`, `libfabric`. Each implements `NativeTransportServer` + `NativeTransportClient`. The probe + selection logic lives in `native::selector`. |
| **kiseki-gateway** | `kiseki_gateway::native::ServerImpl` — the binding-agnostic handler. Implements `NativeGatewayService`, calling into the existing `GatewayOps` trait. SAN canonicalization helper lives here. The interceptor implementation is per-binding (gRPC interceptor in the gRPC submodule; equivalent connection hook in the TCP-framed and RDMA submodules). |
| **kiseki-client** | `kiseki_client::native::NativeClient` — internally holds `Box<dyn NativeTransportClient>`. The `connect` constructor picks the binding per §3.2. |
| **kiseki-server** | `runtime::run_data_path` calls `kiseki-transport::native::selector::start_all(server_impl)` which probes every binding, spawns listeners for the survivors, and returns a future that resolves on shutdown. |

Module-graph addition:

```
kiseki-client::native ─┬─▶ kiseki-transport::native::grpc        (client)
                       ├─▶ kiseki-transport::native::tcp_framed
                       ├─▶ kiseki-transport::native::ibverbs    (linux only)
                       └─▶ kiseki-transport::native::libfabric  (linux only)
                                                  │
                                                  ▼ wire
                                kiseki-transport::native::<binding> (server)
                                                  │
                                                  ▼
                                kiseki-gateway::native::ServerImpl
                                                  │
                                                  ▼
                                kiseki-gateway::ops::GatewayOps   (in-process)
                                                  │
                            ┌─────────────────────┼─────────────────────┐
                            ▼                     ▼                     ▼
                    kiseki-composition     kiseki-chunk           kiseki-log
```

No new bounded contexts. The native gateway is a new ingress — across multiple bindings — on the existing Gateway / Composition / Chunk / Audit / Encryption contexts.

---

## §5 Authentication — SAN canonicalization (binding-agnostic)

SAN canonicalization is defined contract-level. Each binding plugs its own connection-establishment hook; the canonicalization logic itself is shared.

The canonicalization helper lives in `kiseki-gateway::native::san`. Rules from the *Canonical SAN URI form* ubiquitous-language entry: lowercased scheme, lowercased authority, no trailing slash, percent-decoded unreserved characters, NFC-normalized tenant id, ASCII-only.

### §5.1 Per-binding hook

Every binding stashes the canonical SAN form per-connection at handshake time and exposes it to handlers via the `RequestPrincipal` trait (§1.8). The canonicalization helper itself is shared; the **trust anchor** differs per binding/provider — see the per-provider trust matrix in §2.4.1, which is the authoritative source. This section lists only the canonical-SAN **extraction site** for each binding (where in the connection lifecycle the canonicalized form gets stashed). Trust-anchor changes go in §2.4.1, not here, to avoid the documentation-drift hazard a duplicated table would create (resolves R2-L1).

| Binding/provider | Canonical-SAN extraction site |
|---|---|
| gRPC/h2 | tonic interceptor (`SanInterceptor`); cert from `tonic::Request::extensions()`; canonicalize; stash in tonic request extensions. |
| TCP-framed | `kiseki-transport::native::tcp_framed::accept_hook`; runs after the rustls handshake, before the first frame. Stash in per-connection context. |
| ibverbs | QP-establishment ULP — mTLS-over-rdma-cm handshake completes during RTR transition; cert extracted; stash in per-QP context. |
| libfabric/verbs | inherits ibverbs (same flow). |
| libfabric/cxi | `accept_hook` reads first message as `CxiAttestationEnvelope` (§2.4.2); validates per the 8-step flow; stash canonical SAN in per-connection context. |
| libfabric/sockets,tcp | inherits TCP-framed (same flow). |

The `RequestPrincipal` extractor (§1.8) reads the stashed SAN through one trait surface — `ServerImpl` never reaches into binding-specific stash locations. Implementer adds one BDD scenario per binding asserting tenant_id mismatch still rejects (closes gate-1 hot spot 2).

### §5.2 Per-handler check

Every handler (in `ServerImpl`) does:

1. Read `principal.cert_san_canonical()` via `&dyn RequestPrincipal`.
2. Decode `ControlFields.tenant_id` and canonicalize.
3. Compare byte-equal against the principal's canonical SAN. Mismatch returns `PermissionDenied{san_canonicalization_mismatch}` and emits a security-failure audit event (I-NG7).
4. Validate `idempotency_key` length 1..=64. Out of range returns `InvalidArgument{idempotency_key_length}`.

### §5.3 Multiple SAN URIs (resolves F-M4)

x.509 certs may carry multiple SAN URI extensions. The canonicalization helper scans for SAN URIs matching the kiseki tenant pattern (`spiffe://kiseki/tenant/<org_id>` after canonicalization). The cert MUST contain **exactly one** matching URI. Zero matches → `Unauthenticated{no_kiseki_tenant_san}`. Two or more matches → `Unauthenticated{ambiguous_kiseki_tenant_san}` (defends against a malicious issuer placing multiple tenant URIs to pivot which one validates). Non-kiseki SAN URIs (e.g., DNS names for service-mesh interop) are ignored.

### §5.4 Inode allocation discipline (resolves F-M3)

Inodes are allocated by the composition store via a per-namespace monotonic 64-bit counter (`next_inode_per_namespace`), persisted in the fjall meta keyspace alongside `last_applied_seq`. Inodes are **never reused**, even after unlink + GC. At 4 billion inode allocations / sec for ~146 years before u64 wraps. Stale handle tokens whose `inode` no longer exists in the namespace return `Unauthenticated{inode_orphaned}` on next op + emit `kiseki_native_handle_orphaned_inode_total{tenant}`.

### §5.5 Cert revocation mid-session (A-NG17 / F-NG8)

Each binding's connection hook calls into the existing CRL/OCSP source (ADR-023) on connection establishment. A re-validation worker runs on a `tokio::time::interval` of `KISEKI_CERT_REVAL_INTERVAL_MS` (default 60 s) per active connection. On revocation, the binding's connection hook closes the connection with `Unauthenticated{cert_revoked}`. RDMA bindings translate the connection close to a QP transition to ERR + SAN-revoked tagged-message reply.

---

## §6 Hybrid leader routing — `topology_version` push + safety-net TTL

Per I-NG13 / A-NG13 + I-NG8:

- **Server side**: every `ServerImpl` method appends `topology_version: u64` to the response trailer. Each binding maps this onto its native trailer (gRPC trailing metadata; TCP-framed appended footer; RDMA tagged completion). The cluster increments the version monotonically on:
  - shard-leader change (publisher: `kiseki-log`)
  - namespace-shard-map mutation (publisher: `kiseki-control`)
  - split / merge event (publisher: `kiseki-log`)
  - **binding-set change** on any node (publisher: `kiseki-server::runtime` via the binding-listener lifecycle hooks defined in §3.4 — covers add (operator wires a new NIC and restarts), remove (listener crash), restart success).
- **Client side**: `kiseki_client::native::TopologyCache` holds `(version, Vec<NodeBindings>, Vec<ShardLeadership>)` behind a `parking_lot::RwLock`. Every native client RPC checks the trailing topology version on response; on mismatch, re-issues `get_topology` to refresh. `NodeBindings` (defined in §1.7) advertises per-node binding endpoints so heterogeneous clusters work via per-edge selection (§3.2).
- **Version is global across bindings**, reflecting the *responding node's* view. Clients use the highest seen version across all responses regardless of which binding delivered it.
- **TTL safety net** (I-NG8 default 30 s): if the topology version channel regresses (operator error / clock skew), the cache invalidates after 30 s anyway.
- **Direct dial vs proxy fallback**: client picks the leader from the cache and dials it directly via the highest-ranked mutually-supported binding (§3.2). On `NotLeader{leader=X}` or `Unavailable`, client refreshes and retries.

`get_topology` contract (resolves F-H3): when `known_topology_version > 0` and equals the responding node's current version, server returns 304-equivalent (empty `TopologyInfo` with `topology_version` set to current). When `0` or stale, returns the topology — **scoped to the calling tenant**: the `shards` list filters to only shards that own at least one of the caller's namespaces. The `nodes` list still includes every node the client may need to dial, with the per-node binding advertisement.

**Consistency model** (resolves F-M8): the version returned reflects the *responding node's* view, which may be stale by up to one Raft heartbeat (default 100 ms, ADR-026). Clients treat the version as eventually consistent within this bound. Metric `kiseki_native_topology_staleness_seconds{node}` exposes the per-node lag for ops alarms.

**Channel multiplexing** (A-NG15): the client maintains one connection per *(node, binding)* it currently believes hosts at least one shard leader the client cares about. Per-binding multiplexing is the binding's job (h2 streams for gRPC, request_id multiplexing for TCP-framed, QP for RDMA). Idle connections are closed when the client's topology cache no longer references them.

---

## §7 Streaming + multipart shape

### §7.1 Multi-chunk PUT atomicity (resolves F-C2)

A `put_object_stream` of size > `MAX_PLAINTEXT_PER_CHUNK` (default 4 MiB) splits into multiple chunks. Different chunks hash to different `chunk_id`s and the cluster places them on different shards (rendezvous-hash placement, ADR-033). The `commit_stream` contract (no partial visible, I-NG2) requires explicit specification of the multi-chunk failure mode:

1. **Chunk write parallelism**: chunks within a single stream write in parallel up to `KISEKI_PUT_CHUNK_PARALLELISM` (default 4).
2. **`commit_stream` barrier**: server returns `Ok` ONLY when ALL referenced chunks have been confirmed durable on `min_acks` peers per the pool's durability strategy (preserves I-L5).
3. **Partial-failure aborts the whole stream**: if ANY chunk fails (timeout, quorum loss, etc.), `commit_stream` returns `Aborted{reason=partial_chunk_failure, succeeded=N, total=M}`. Client retries with the same `idempotency_key` (per A-NG10) — dedup table short-circuits to the original outcome (the abort), or fresh key for a new attempt.
4. **Staged-chunk cleanup**: chunks that landed before the failure are NOT linked into a composition record. They have refcount = 1 (set by `write_chunk` for new chunks) but no composition references them. The orphan-fragment scrub (ADR-005, F-D7 mitigation) reclaims them after a 24-hour TTL.
5. **No partial state visible at any point** (I-NG2). Readers observing the namespace before, during, or after the abort never see a half-written composition.

**Failure mode F-NG12** (in `specs/failure-modes.md`): multi-chunk PUT/PutPart writes some chunks successfully, then a later chunk fails. P3 severity; per-PUT bounded; recovery via orphan-fragment scrub.

### §7.2 Streaming threshold

The `put_object` / `get_object` / `write` / `read` unary forms accept payloads ≤ inline_threshold (default 8 KiB, ADR-006). Above, clients use the `stream` form. **Per-stream cap**: 64 MiB per stream (I-NG9). Server tracks cumulative bytes for each open stream and rejects with `OutOfRange{stream_cap_exceeded}` when a frame would push past.

### §7.3 Multipart (above 64 MiB)

S3-style flow: `init_multipart(namespace_id, name) → upload_id`, `put_part(upload_id, part_number, data)` (streaming, per-part), `complete_multipart(upload_id, parts: Vec<PartETag>)`, `abort_multipart(upload_id)`. The proto / wire shape mirrors S3 multipart; ADR-042 leverages the same in-process `start_multipart` / `upload_part` / `complete_multipart` APIs that S3 calls into.

### §7.4 Resume-token (handoff Q3)

NOT in ADR-042 v1. A future amendment may add a session-resume path on top, but correctness is already provided by `idempotency_key` + fresh-stream retry (A-NG10 / I-NG5). Captured in `docs/performance/optimization-backlog.md`.

### §7.5 Repeated stream-`first` messages (resolves F-M1)

Every streaming verb has a `first` oneof carrying the request envelope (with `ControlFields`, including `idempotency_key`). The server's stream handler accepts EXACTLY ONE `first` per stream. A second `first` returns `InvalidArgument{stream_already_initialized}` and aborts. Any frames before the first `first` return `InvalidArgument{stream_not_initialized}`. The client SDK enforces single-`first`; this server-side check defends against malicious or buggy clients across all bindings.

---

## §8 Idempotency — Raft-replicated dedup state

Per I-NG5 + A-NG10 (resolves F-H2):

- **Dedup table** lives in the per-shard openraft state machine alongside the chunk-state and composition-state tables.
- Each accepted write writes a row `(tenant_id, namespace_id, idempotency_key) → (response_summary, expires_at_ms)` keyed by the triple.
- Every voter applies the same row in the apply phase. Retries arriving after a leader change deduplicate against the new leader's local replica.
- Periodic sweep (`kiseki-log::dedup_gc`) trims expired rows. Default TTL 5 min (I-NG5); configurable per tenant.

### §8.1 Ordering vs lease fencing (resolves F-H4)

When a request carries a `fencing_token` (lease-bound write), the server MUST validate the token **before** consulting the dedup table. The check order is:

1. SAN canonicalization + payload tenant_id match (I-NG1).
2. **Fencing token check** (if request has one): reject with `Aborted{lease_fenced, current_token}` if stale. Do NOT consult dedup.
3. Idempotency dedup: if a row matches and is unexpired, return the original response.
4. Otherwise: process the request normally.

This ordering closes the F-H4 race where a partitioned old lease holder could replay a successful pre-partition write and bypass the new fencing_token. The dedup row is written only after step 4 succeeds, with the fencing_token included in the row's `request_meta` (for audit + post-mortem).

### §8.2 Cost analysis

Each row is ≤ 256 bytes (a UUID composition_id + a u64 + a u32 etag). At 100 k writes/sec/shard with 5 min TTL: 30 M rows × 256 B = 7.7 GiB per shard worst-case (zero dedup hits). In practice TTL eviction keeps it small. Per-tenant rate limits + the 64-byte cap on `idempotency_key` (I-NG1) bound the worst case.

---

## §9 Lease lifecycle — TTL, fencing, drain

Per I-NG10 + I-NG12 + I-NG14:

- **`acquire_lease`**: server checks `(namespace_id, inode)` — if no lease or expired, grants `LeaseGrant{lease_id, fencing_token, ttl_ms}`. The `fencing_token` is a per-(tenant, namespace, inode) monotonic 64-bit counter, persisted as part of the dedup table's apply phase.
- **`renew_lease`**: validates the presented `lease_id` matches the current grant; server extends TTL and returns the same fencing_token. Renewal cadence (recommended): 1/3 of TTL.
- **`release_lease`**: voluntary. Server clears the lease + invalidates pending uncommitted writes for the inode.
- **TTL expiry**: server's lease tracker has a `tokio::time::interval` per active lease that fires on timeout; expiry runs through the per-shard Raft state machine so every replica sees the same revocation event.
- **Forced revoke**: out of scope for ADR-042 v1; defer to a future operations ADR.
- **Fencing on writes**: every `write` / `write_stream` carries `fencing_token` in `ControlFields`. Server rejects with `Aborted{lease_fenced, current_token}` if presented token < current. Audit event records the rejected token + principal (I-NG12).
- **Drain interaction** (I-NG14): when a node enters `Draining`, its lease tracker:
  1. Refuses new `acquire_lease` requests if requested TTL would outlast the drain quiesce window. Returns `Unavailable{node_draining}`.
  2. Existing leases continue until expiry / release. Drain protocol waits.

---

## §10 Encryption boundary — server-decrypt default + opt-in client-decrypt

Per I-NG6a/b/c + A-NG7 (resolves F-C1, F-H5):

- **`crypto_boundary = ServerOnly` (default)**: every read returns plaintext. The wire is mTLS-encrypted (or RDMA-attested-equivalent for the RDMA bindings); no plaintext escapes the server's TLS / RDMA-secured context.
- **`crypto_boundary = TrustedCompute`**: requires the namespace to ALSO declare `crypto_shred_policy = best_effort` (I-NG6c). Server returns sealed envelopes (`ciphertext + nonce + tag`) plus a `dek_fetch_ticket` per chunk. Client calls `fetch_dek(ticket)` (or `batch_fetch_dek` for multi-chunk reads) to obtain the per-chunk DEK and decrypts locally. Unlocks GPU-direct + zero-copy paths.
- **DEK-fetch ticket**: HMAC-SHA256-signed under the `dek_fetch_ticket_signing_key` (see §11.1); commits to `(tenant_id, namespace_id, composition_id, chunk_id, namespace crypto_boundary at Read time, master_key_epoch, expires_at)`. Keymanager re-derives the signing key, validates HMAC, validates at-Read-time mode against namespace's current policy, validates master_key_epoch ± grace. On mismatch returns `NamespaceModeChanged` or `Unauthenticated{ticket_epoch_stale}`.
- **Mode flip on object verbs (I-NG6a)**: object reads commit the at-Read-time mode into the ticket. A flip from `TrustedCompute` → `ServerOnly` between Read and `fetch_dek` does NOT break in-flight reads — the keymanager honors the ticket's at-Read-time mode.
- **Multi-chunk reads (resolves F-H2)**: a single read whose response contains N sealed chunks returns N tickets. Clients MUST call `batch_fetch_dek(tickets)` instead of N separate `fetch_dek` calls. Per-Read latency stays O(1) keymanager calls.
- **Mode flip on POSIX verbs (I-NG6b)**: handled by handle tokens (see §11).
- **Crypto-shred under TrustedCompute**: tenant-issued shred destroys the master key in the keymanager; future `fetch_dek` calls fail. Already-fetched DEKs in client RAM are NOT retroactively revocable — operators acknowledge by setting `crypto_shred_policy = best_effort` (I-NG6c).
- **`crypto_boundary` flag mutation auth (resolves F-M6)**: setting `crypto_boundary = TrustedCompute` requires **cluster-admin** authentication, NOT tenant admin. Tenant admins request the change via a separate admin RPC; cluster admin reviews + applies.
- **DEK cache (out of scope)**: ADR-042 explicitly does NOT introduce a DEK cache. Captured in `docs/performance/optimization-backlog.md` (B5) and as a future ADR-04X amending ADR-002 / ADR-011.

---

## §11 POSIX handle tokens

POSIX inode handles (`open` → `handle_token`) are opaque, server-signed tokens that encode at-open-time state. Token contents:

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
// handle_token_signing_key (see §11.1).
```

- **`open`**: server allocates inode (or resolves), captures at-open-time mode + connection's canonical SAN URI + current master_key_epoch, signs the token, returns it.
- **Subsequent ops** (`read`, `write`, `fsync`, etc.) carry the token. Server validates HMAC, validates master_key_epoch ± grace, validates token's `cert_san_canonical` matches connection's current SAN (resolves F-H1), decodes mode, operates accordingly. Mode flips on the namespace AFTER the token was issued do not affect this handle (I-NG6b).
- **`close`**: voluntary. Token's `issued_at + token_max_lifetime` (default 1 hour) bounds residual exposure.
- **Cert revocation**: a revoked cert cannot establish a new connection; a held token is also implicitly invalidated — client cannot present it without first establishing the connection that would carry it.
- **Per-handle state** (file position, etc.) is **client-side**, not server.

### §11.1 Cryptographic key derivations (resolves F-C1)

ADR-042 specifies four signing keys, each derived once per process at startup from the system master key (held in mlock'd memory, ADR-002 / I-K8):

| Key name | Derivation | Holder | Purpose |
|---|---|---|---|
| `handle_token_signing_key` | `HKDF-SHA256(master_key, salt="kiseki-handle-token-v1", okm_len=32)` | Gateway | HMAC-SHA256 sign / verify HandleToken |
| `dek_fetch_ticket_signing_key` | `HKDF-SHA256(master_key, salt="kiseki-dek-fetch-ticket-v1", okm_len=32)` | Gateway (signs), keymanager (verifies) | HMAC-SHA256 sign / verify DEK-fetch ticket |
| `topology_signing_key` | reserved, future use | — | (Not used in v1) |
| `multipart_upload_signing_key` | `HKDF-SHA256(master_key, salt="kiseki-multipart-upload-v1", okm_len=32)` | Gateway | HMAC-SHA256 sign / verify multipart `upload_id` opacity |

**Rotation discipline**: master key has an epoch (ADR-007). Keymanager epochs storage moved to fjall in ADR-022 rev-3 — the rotation-grace semantics described here are unchanged; only the on-disk substrate changed. Tokens carry the epoch. On verification, server re-derives the signing key from the **current** master key AND from the **previous** master key (kept in memory during rotation grace, default 5 min, configurable). In-flight tokens issued under previous epoch validate during grace; after, fail with `Unauthenticated{token_epoch_stale}`. Clients re-Open / re-establish on stale.

**Compromise model**: master-key compromise → all four signing keys derivable → game-over (governed by ADR-002 / I-K8). Per-signing-key compromise → only that signing key's tokens are forgeable; future tokens (after master rotation) unaffected.

---

## §12 Audit

Per I-NG7:

- **Gateway-dispatched paths** auto-fire through the existing audit pipeline (ADR-009) because every native handler calls into the same `GatewayOps` trait that S3 / NFS / FUSE use. No new audit wiring on these paths.
- **`workflow_ref` policy** (resolves F-H5): per-tenant policy field `workflow_ref_required_for_writes` drives behavior on writes that omit `workflow_ref`:
  - `true`: server REJECTS the write with `InvalidArgument{workflow_ref_required}` at the proto-handler boundary, **before** any storage work runs. Audit emits security-failure.
  - `false` (default): server substitutes literal token `unattributed` for the workflow_ref on the audit event. Operators track `kiseki_native_writes_unattributed_total{tenant}`.
- **`acquire_lease` / `release_lease`** produce audit events. **`renew_lease`** does NOT — instead `kiseki_native_lease_renewals_total{tenant, namespace}` increments.
- **Proto-handler-boundary rejections** (I-NG1 SAN/payload mismatch, I-NG6c missing dual-flag, I-NG11 stream cap exceeded, I-NG12 fenced write, I-NG14 lease-against-draining-node) emit a `security-failure` audit event via an explicit `audit_emit_at_proto_boundary` hook in `ServerImpl`. Same hook fires regardless of binding.
- **Lease writes**: every successful lease-bound `write` records `lease_fencing_token` alongside principal and workflow_ref (I-NG12).

### §12.1 Per-binding observability

Contract-level metrics (workflow_ref counters, lease-renewal counters, audit events) ride the existing pipeline regardless of binding. **Per-binding** metrics are additionally required so dashboards can attribute connection-layer issues correctly:

| Metric | Labels | Purpose |
|---|---|---|
| `kiseki_native_binding_connections_active` | `{binding}` | live count of per-binding open connections |
| `kiseki_native_binding_connections_total` | `{binding, outcome ∈ {accept, reject, error}}` | cumulative connection accept/reject/error |
| `kiseki_native_binding_handshake_failures_total` | `{binding, reason}` — see vocabulary below | mTLS / cxi-attestation failures per binding |
| `kiseki_native_binding_listener_crashed_total` | `{binding}` | listener crashes that triggered the §3.4 backoff-restart |
| `kiseki_native_binding_pinned_total` | `{binding}` | server-side count of pinned-binding deployments (see §3.1) |
| `kiseki_native_topology_refresh_total` | `{reason ∈ {leader_change, split, merge, binding_restart, binding_set_change, other}}` | per the §3.4 attribution split |
| `kiseki_native_topology_staleness_seconds` | `{node}` | per-node lag for ops alarms (already in §6) |
| `kiseki_native_topology_version_mismatch_total` | (none) | cumulative mismatches that triggered a refresh (gate-1 hot spot 6 alarm) |

Implementer wires these alongside the existing `kiseki_native_*` contract-level metrics. Per-binding labels are stable enum values — the metric `{binding=Grpc | TcpFramed | Ibverbs | "Libfabric/Cxi" | "Libfabric/Verbs"}` carries the provider-qualified label for libfabric so dashboards can split cxi from verbs without a join.

#### `handshake_failures_total` reason vocabulary

The `reason` label is a stable enum so dashboards / alerts can route by exact value. Resolves R2-M1 — collapsing all cxi attestation failure modes into a single `attestation_failed` value would hide the diagnostic signal between bad-clock vs replay vs bad-cert vs forged-signature.

| `reason` value | Applies to | Source |
|---|---|---|
| `san_invalid` | mTLS bindings | cert SAN parses but doesn't match kiseki tenant pattern |
| `cert_expired` | mTLS bindings + cxi | x.509 NotAfter past current time |
| `cert_revoked` | mTLS bindings + cxi | CRL/OCSP returned revoked status |
| `cxi_attestation_missing` | cxi only | first message wasn't a valid `CxiAttestationEnvelope` |
| `cxi_attestation_schema_too_new` | cxi only | envelope `schema_version > 1` |
| `cxi_san_mismatch` | cxi only | envelope `canonical_san` ≠ cert-derived SAN |
| `cxi_attestation_clock_skew` | cxi only | `issued_at` outside ±30 s server HLC |
| `cxi_attestation_signature_invalid` | cxi only | ECDSA-P256 verify failed |
| `cxi_attestation_replay` | cxi only | nonce duplicate in 60-s window bloom filter |
| `cxi_attestation_oversize` | cxi only | envelope or cert chain exceeded the §2.4.2.1 caps |
| `dlopen_failed` | RDMA bindings | system .so missing or path validation failed |
| `tls_handshake_failed` | mTLS bindings | rustls handshake error |
| `other` | any | catch-all for unclassified failures |

Cardinality: ~13 stable values across all bindings. Operators alarm on `cxi_attestation_replay > threshold` (active attack indicator) independently from `cxi_attestation_clock_skew > threshold` (configuration drift indicator).

**Cross-node replay alarm** (resolves R3-O2): a captured cxi attestation envelope replayed across N different cluster nodes triggers each node's bloom filter independently. The per-node `cxi_attestation_replay` rate stays low (one replay per node) while the cluster-wide aggregate rate spikes. Operators monitoring for distributed-replay attacks should alert on the aggregate, not per-node:

```promql
sum by (cluster) (
  rate(kiseki_native_binding_handshake_failures_total{
    binding="Libfabric/Cxi",
    reason="cxi_attestation_replay"
  }[1m])
) > 1
```

A distributed replay shows as low per-node + high aggregate. The threshold (`> 1` per minute) is tunable per operator's threat model; even a single cross-node replay event is anomalous and worth investigating.

---

## §13 Out of scope (explicit non-goals)

ADR-042 deliberately does NOT introduce:

1. **DEK caching** — captured in `docs/performance/optimization-backlog.md` (B5). Future ADR-04X.
2. **Resume tokens for streaming writes** — fresh-stream + idempotency_key (A-NG10) is sufficient.
3. **Cross-site federation** — native ops are single-cluster only. ADR-016 covers cross-site.
4. **FUSE migration to native** — separate work-stream (A-NG1). FUSE keeps `RemoteHttpGateway` until follow-up ADR.
5. **`CompositionStore` sharding (DashMap)** — captured as B1 in optimization-backlog. Already partially landed via ADR-022 rev-2 fjall + atomic create_with_name; remaining headroom on the in-process path is core-bound, not lock-bound.
6. **Forced lease revocation by admin** — operations ADR.
7. **Per-tenant binding selection** — bindings are deployment infrastructure, not tenant-controllable.

### §13.1 Schema discipline (resolves F-M7 — pre-1.0 scope)

Kiseki is **pre-production**: there are no deployed clients to preserve and no backward-compat contract to enforce. The schema discipline below is **internal hygiene** for the implementer + reviewers, not a stability promise:

- **Contract Rust types** are the canonical shape. All bindings round-trip them verbatim. Adding/removing/renaming fields in the contract types means re-running codegen for the gRPC binding and rebuilding postcard schemas implicitly.
- **proto fields and enum values use sane numbering**. Don't reuse field numbers within a single proto file. New variants append; deprecated fields can be deleted outright since there are no on-the-wire deployments to break.
- **Opaque tokens carry a 1-byte schema_version**. A future incompatible change flips the version and rejects older formats with a typed error.
- **Master-key-epoch in tokens** (per §11.1) covers cryptographic rotation independently of schema_version.
- **TCP-framed and RDMA bindings carry their own version byte** at the wire level (matches the ADR-041 fabric pattern). Per-binding version bumps are independent of contract Rust-type changes — one can change without the other (e.g. a TCP-framed compression option flip would bump only that binding's version).
- **Once we declare 1.0** (out of ADR-042's scope; will land in a separate "wire-stability" ADR), the discipline tightens: append-only fields, never-reuse numbers, formal deprecation, etc. Until then, *delete and rename freely* during iteration.

### §13.2 Pre-1.0 stance: no rolling upgrades

Pre-1.0, **rolling upgrades are NOT supported across native-binding wire-version changes**. Operators stop the cluster, upgrade all nodes (binary + system libraries if applicable), then restart. No cross-version sessions; no on-the-fly compatibility shims.

This is consistent with the ADR-022 rev-2/3/4 wipe-and-re-replicate stance for storage-format changes. The cross-binding rolling-upgrade case (mixed binding wire versions across the cluster during the upgrade window, mid-flight cross-binding session integrity) is genuinely hard and not on the v1 critical path.

Per-binding wire versions follow these rules during pre-1.0:

- **TCP-framed binding**: bumps to v3 (V1=postcard rev-1, V2=current postcard rev-2 per ADR-041). Future changes bump major when incompatible. **Coupling to ADR-041** (resolves R2-O1): the TCP-framed binding's wire format derives from ADR-041's fabric transport (length-prefixed-protobuf-over-TLS-over-TCP). When ADR-041's framing version bumps (e.g. a future compression option, congestion-control hint, etc.), this binding's wire version bumps in lockstep in the same PR. Reviewers should reject ADR-041 changes that don't include the matching ADR-042 §13.2 update, and vice versa.
- **gRPC/h2 binding**: proto-level changes are tracked via the `.proto` file's package version comment; pre-1.0 we delete-and-rename.
- **ibverbs / libfabric bindings**: each carries its own application-layer version byte at the start of every framed message. Bumps independently of the others.
- When the contract Rust types change incompatibly, every binding's wire version bumps in lockstep (same release).

A future "wire-stability" ADR — landing alongside the 1.0 declaration — defines:

- Backwards-compatibility windows for rolling upgrades (likely N and N+1 binding versions tolerated concurrently).
- Per-binding deprecation policy.
- Cross-binding session-integrity rules during upgrade windows.

Until then: stop, upgrade, restart. State this explicitly in operator docs.

### §13.3 Future binding-specific extensions

The contract is the lowest common denominator across bindings. Some future verbs are **only** efficient on specific bindings:

- **RDMA atomics** (compare-and-swap on remote memory) — natural on ibverbs/libfabric; meaningless over gRPC/h2.
- **libfabric tagged-message guarantees** for ordered delivery — only meaningful on the libfabric binding.
- **Zero-copy GPU-direct receive** — natural on RDMA bindings + GPU-direct stack; absent on gRPC/h2.

Future ADRs MAY extend the contract with **optional** verbs that bindings declare support for. The contract layer would gain a `supported_verbs(): VerbSet` introspection method; clients pre-flight check before invoking optional verbs; missing-binding-support returns `Unimplemented{verb_not_supported_by_binding}`. Out of scope for ADR-042 v1; flagged here so future-architect-self doesn't accidentally close the door by hard-coding the v1 verb set as exhaustive.

---

## §14 Performance budget (per-binding)

A-NG11's targets, post-2026-05-06 perf-spike measurement:

| Binding | Latency class | GET 64 KiB target | PUT 64 KiB target | GET p99 target | PUT p99 target |
|---|---|---:|---:|---:|---:|
| In-process-persistent (floor) | n/a | 192 k op/s | 95 k op/s | 0.5 ms | 0.6 ms |
| **gRPC/h2** | Standard | ≥ 26 k op/s | ≥ 22 k op/s | ≤ 5 ms | ≤ 10 ms |
| **TCP-framed-postcard** | Low | ≥ 100 k op/s | ≥ 60 k op/s | ≤ 2 ms | ≤ 4 ms |
| **ibverbs** | Rdma | per §14.1 | per §14.1 | per §14.1 | per §14.1 |
| **libfabric/cxi** | Rdma | per §14.1 | per §14.1 | per §14.1 | per §14.1 |

Numbers per-node on the perf-spike workstation (8-core Ryzen 7 6800H). gRPC + TCP-framed targets are pinned against the in-process-persistent floor; RDMA targets follow the falsifiable procedure in §14.1.

The gRPC tax (5–7×) is the gap the TCP-framed binding closes for general datacenter deployments. The RDMA bindings close another order of magnitude on HPC fabrics. All four coexist; deployments that don't have RDMA hardware get TCP-framed automatically; deployments that need gRPC for compliance get gRPC-only via the operator pin.

### §14.1 RDMA perf-gate (falsifiable)

"Hardware-bound" is not falsifiable — an implementation that gets 1 k op/s on hardware that supports 1 M op/s technically meets it. RDMA bindings' perf gate uses an explicit fabric-peak measurement procedure:

1. **Baseline**: run the fabric's reference micro-benchmark on the same hardware. Use the **multi-stream** variant — single-stream micro-benchmarks (`ib_send_bw` without `-q`, `fi_bw` without `-p`) over-report per-NIC peak vs realistic kiseki workload (multiple concurrent QPs / streams contending for the same NIC). Resolves R2-L4.
   - ibverbs: `ib_send_bw -m -s 65536 -n 100000 -q 4 <peer>` (4 concurrent QPs) for throughput; `ib_send_lat -q 4 <peer>` for p99.
   - libfabric/cxi: `fi_bw -e msg -S 65536 -I 100000 -p 4 <peer>` (4 parallel processes); `fi_lat` similarly.
   - libfabric/verbs: same as libfabric/cxi, different provider selection.
   - Single-stream variants (without `-q` / `-p`) MAY also be reported for context, but the perf-gate evaluates against the multi-stream baseline.

2. **Record**: `fabric_peak_throughput_op_s` and `fabric_peak_p99_us` for the (provider, hardware) pair, **at the same concurrency level** kiseki's bench uses (8 by default in the next step).

3. **Run kiseki's bench**: `kiseki-profile --protocol native --binding=<binding> --shape=put-heavy --concurrency=8 --object-size=65536 --duration-secs=30` against the same hardware.

4. **Perf-gate criterion**: kiseki throughput ≥ **50 % of fabric_peak** measured by step 2 on the same hardware **at matched concurrency**. Below 50 % blocks the gate-2 perf check; the implementer must investigate (likely candidates: insufficient memory pre-registration, application-layer copies on the bulk path, single-threaded codec on the receive side).

5. **p99 criterion**: kiseki p99 ≤ `2 × fabric_peak_p99` from step 2. Above blocks the gate.

6. **Reporting**: phase 9/10 perf gates publish `(fabric_peak_throughput, fabric_peak_p99, kiseki_throughput, kiseki_p99, ratio, concurrency_level)` per (binding, hardware) pair. The ratio is the falsifiable number; the concurrency level disambiguates micro-benchmark variants.

Until phase 9/10 lands, the §14 row for RDMA bindings reads "deferred to phase 9/10" — not a vague "hardware-bound" promise. The 50 % / 2× ratios are conservative starting targets; once we have measurement data, follow-up amendments may tighten them per (binding, hardware) class.

**Single-node dev/test**: `ib_send_bw` requires a peer node. For dev-loop measurements, the most reliable path is **a two-VM pair on a local KVM host**, each VM getting a virtual HCA via SR-IOV passthrough or libvirt rdma-cm bridging. Loopback HCA pairs (one HCA talking to itself via the kernel software path) are possible on some hardware (older Mellanox + specific kernel knobs) but not consistently available on modern NVIDIA ConnectX cards — treat as a fallback only. The perf gate runs on real cluster hardware in CI; dev measurements are advisory.

### §14.2 Architect's primitive choices to land within the budget

- **gRPC binding codec**: prost (default). No reason to deviate.
- **TCP-framed binding codec**: postcard. Same backend the Raft fabric, composition store, and chunk meta encodings use. Single source of truth across the workspace; 5–10× faster than JSON on serde-derived types.
- **RDMA binding payload codec**: postcard, same as TCP-framed. RDMA primitives carry the bytes; the contract types serialize identically.
- **Allocation discipline on read (resolves F-M5)**: gateway types use `bytes::Bytes` so chunk-store → AEAD → wire chain skips intermediate `Vec<u8>` allocations on the gateway side. RDMA bindings additionally pre-register memory regions for zero-copy bulk transfers.
- **Hot-path instrumentation**: respects `KISEKI_OBSERVABILITY=on/off` (existing knob). When `=off`, the `InstrumentedLogOps` and `InstrumentedKeyManager` wrappers are bypassed.
- **Per-tenant concurrent-stream counter (resolves F-H6)**: `dashmap::DashMap<TenantId, Arc<AtomicUsize>>` — sharded by tenant. Cap-then-allocate is two atomic ops. No mutex on the hot path. Cap-checking at proto-handler boundary BEFORE staging buffer allocation (I-NG11). Same counter is used regardless of binding.
- **Idempotency dedup state in Raft state machine**: piggybacks on existing apply phase; no new Raft proposal cost.

---

## §15 Adversary gate-1 hot spots (pre-emptive)

Architect anticipates the gate-1 review will challenge:

1. **SAN canonicalization helper used uniformly across bindings** — Unicode / IDN / percent-encoding edge cases. Implementation MUST use the single helper applied to BOTH cert SAN AND payload tenant_id, with byte-exact compare. Per-binding hooks plug in but never reimplement canonicalization.
2. **Per-binding SAN handoff path** — gRPC interceptor stash, TCP-framed connection-context stash, RDMA QP-establishment stash. Each must put the canonical form somewhere `ServerImpl` reads with the same code. Architect mandates a `RequestPrincipal` extractor trait that every binding provides.
3. **Idempotency dedup state on shard merge / split** — dedup table is in per-shard Raft state machine. ADR-033 §3 / §4 describe merge / split apply hooks. On merge, both shards' dedup tables coalesce; on split, each child shard inherits the relevant subset.
4. **Lease + drain race** — between `acquire_lease` accept and the node entering `Draining`, the lease may already be granted. ADR-035 drain protocol must tolerate up to one TTL of additional drain latency without spurious failure.
5. **TrustedCompute mode flip during `fetch_dek`** — server signs the ticket at Read time; keymanager validates against current namespace mode. Architect should verify there is no window where a flip leaves an in-flight ServerOnly Read pending without a valid ticket. Mitigation: ServerOnly responses don't issue tickets at all.
6. **Topology-version regress under operator error** — if a leader change happens but version isn't incremented (bug), clients see stale topology indefinitely. 30 s TTL safety net is the ultimate fallback. Metric `kiseki_native_topology_version_mismatch_total` surfaces regressions.
7. **Per-tenant stream cap counter atomicity** — cap-then-staging-allocate sequence MUST be atomic w.r.t. the counter. Sharded `DashMap` ensures this; the binding-agnostic counter is shared across all bindings (same tenant_id; same cap regardless of how the client reached the server).
8. **Cert revocation + long-running streams** — mid-stream tear-down on revocation interacts with partial-state-not-visible (I-NG2). Architect verifies existing in-flight staging buffer cleanup catches the torn-down stream identically across all bindings.
9. **Binding-selection failure modes** — what if two bindings race during startup probe? What if `KISEKI_NATIVE_TRANSPORT=ibverbs` is set but `libibverbs.so` can't be dlopen'd? Architect mandates: probe is fully sequential; pinning to an unavailable binding is fatal at startup with a clear error; auto mode falls through gracefully.
10. **Heterogeneous-binding cluster + topology consistency** — when node A serves libfabric+tcp_framed and node B serves only tcp_framed, the topology must advertise per-node bindings. Client-side selection must handle this without crashing if its preferred binding isn't on a particular node. Failure mode F-NG13 added: "Client selects a binding that the dialed node doesn't serve" — recovery: client falls back to the highest-ranked common binding.
11. **RDMA security model heterogeneity** — different libfabric providers have different authentication models (cxi auth keys, verbs RDMA-cm, efa security groups). Application-level SAN canonicalization runs uniformly post-connection, but the *trust anchor* is provider-dependent. Architect mandates a per-binding `TrustModel` documentation section (see §5.1 table) so operators know what they're getting.
12. **Mixed-binding session hazards** — a client doing a multipart upload that switches binding mid-session (e.g. node failover, libfabric provider crash). The contract types are identical across bindings, so request_id + idempotency_key carry across; the binding switch is invisible at the contract level. Architect verifies the connection-pool layer in `kiseki-client::native` can transparently re-route in-flight requests.

---

## §16 Migration / coexistence

ADR-042 introduces a NEW service alongside the existing S3 / NFS / FUSE paths. Coexistence rules:

- **No deprecation of existing paths.** S3 / NFS / FUSE continue to work unchanged. They share the same in-process `GatewayOps` so behavior is consistent.
- **kiseki-client::native** replaces the SDK-direct path that previously went through `RemoteHttpGateway`. The legacy `RemoteHttpGateway` stays for FUSE pending the FUSE migration.
- **Python / C++ / C-FFI bindings** continue to surface the existing `KisekiClient` API. Internally they switch to `NativeClient`. No SDK-API breakage.
- **Cert issuance**: tenant clients need certs with SPIFFE-format SAN URIs. Operators using cluster-internal certs (cluster-node-role) must NOT use those for client-side native ops; SAN format differs and the SAN-role check rejects.

### §16.1 Build phase ordering (for `specs/architecture/build-phases.md` update)

0. **Contract types** — define Rust types in `kiseki-proto::native_contract` per §1.1 and §1.7 (`NodeBindings`, `BindingEndpoint`, `LatencyClass`, `RequestPrincipal` trait). Transport-agnostic.
1. **gRPC/h2 binding (existing)** — already implemented in the original ADR-042 draft; refactor into the new layout (`kiseki-transport::native::grpc`) implementing `NativeTransportServer`/`Client`. Generate `.proto` consistent with the contract types in step 0. Wire the gRPC-specific `SanInterceptor` via the `RequestPrincipal` extractor.
2. **`kiseki_gateway::native::ServerImpl`** — binding-agnostic handler reads the request principal via `&dyn RequestPrincipal` only. No binding-specific code in the handler.
3. **TCP-framed-postcard binding** — new. Implement `NativeTransportServer`/`Client` reusing the ADR-041 fabric framing primitives. Connection-acceptance hook stashes canonical SAN; `RequestPrincipal` impl reads it.
4. **`kiseki-transport::native::selector`** — runtime probe + selection logic per §3.1's three phases.
5. **`kiseki_client::native::NativeClient` + TopologyCache** — client-side per-edge selection + per-binding connection pool.
6. **BDD steps** for `native-gateway.feature` against a real spawned cluster (ClusterHarness). The harness uses runtime probe by default; per-binding scenarios are tagged `@binding=grpc`, `@binding=tcp_framed`, etc., via cucumber's TagOperation filter (cross-link to `specs/implementation/bdd-completion-plan.md` for the harness invocation pattern). One `@binding=*` scenario per binding asserts tenant_id mismatch still rejects (closes the per-binding handoff hazard from gate-1 hot spot 2).

   **Per-binding BDD coverage scope** (resolves R2-O2): full BDD parity is mandatory for `gRPC` and `TcpFramed` bindings in v1 (phases 0–8). RDMA bindings (phases 9–10) ship with a **reduced BDD subset** covering at minimum: handshake (mTLS-over-rdma-cm OR cxi attestation), tenant identity round-trip via `RequestPrincipal`, single-verb round-trip per category (one Object verb, one POSIX verb, one Lease verb), error mapping per §1.4 taxonomy, and the binding-specific failure scenarios per §15 hot spots 9–12. Full BDD parity for RDMA bindings is a phase-10 follow-up captured in `specs/implementation/bdd-completion-plan.md`.

   **Fault-injection scenarios** (resolves R2-O3): gate-1 hot spot 9 ("binding-selection failure modes") needs targeted BDD scenarios to catch class-9 issues at gate-1 perf-check rather than in production. Phase 6 BDD additions: probe-timeout simulation, listener-crash + restart-cycle, topology-version-regress under operator error, and (for cxi) attestation-replay-under-load. Cross-link to `specs/implementation/bdd-completion-plan.md`; specific scenario list to be added there during phase 6 work.

7. **`kiseki-profile --protocol native`** driver — per-binding mode (`--binding={grpc|tcp|ibverbs|libfabric|auto}`).
8. Re-measure against §14 targets per binding; gate-1 perf check.
9. **ibverbs binding** — implementer phase 2. RDMA verbs send/recv for control, rdma_read/write for bulk. mTLS-over-rdma-cm flow per §2.3. RDMA perf-gate per §14.1.
10. **libfabric binding (cxi + verbs providers, efa deferred)** — implementer phase 3. OFI provider auto-discovery via `fi_getinfo()`. Per-provider trust matrix per §2.4.1. cxi attestation envelope per §2.4.2 + §2.4.2.1 DoS mitigations. RDMA perf-gate per §14.1.

Phases 0–8 ship in the first deliverable. Phases 9–10 follow once the contract is validated and TCP-framed has been characterized; they require RDMA-class hardware to validate against §14.1.

---

## §17 Consequences

**Wins**:

- Native client reaches the gateway-floor headroom: ~5× S3 GET, ~16× S3 PUT compared to today's HTTP path on commodity hardware via the gRPC binding; ~20× via the TCP-framed binding; orders of magnitude more on RDMA hardware.
- Single binary covers cloud / commodity / HPC deployments. Operators don't pick at build time; auto-detection picks at startup.
- A unified protocol surface for HPC SDK consumers (Python, C++, FUSE eventual). Reduces spec divergence.
- Audit, durability, encryption invariants preserved (every native op flows through the same `GatewayOps` trait that S3/NFS/FUSE use). Same for every binding.
- TrustedCompute mode unlocks GPU-direct + zero-copy on namespaces whose operators accept the residual-exposure trade-off. RDMA bindings make this materially faster.

**Trade-offs accepted**:

- More code paths to test: per-binding BDD scenarios, per-binding flame profiles. Mitigated by parameterized BDD tags and the contract layer being shared.
- Two crypto-cache disciplines until ADR-04X unifies: existing plaintext cache (TTL+Zeroize+wipe) vs future DEK cache.
- FUSE keeps the slower HTTP path until the FUSE-native migration ADR.
- Cert issuance becomes an operator concern (SPIFFE format; per-tenant certs; rotation cadence). Same across all bindings.
- Build host gets two new system deps (`libibverbs-dev`, `libfabric-dev`). Standard for an HPC&AI storage project; no feature gate.

**Costs**:

- Phase 1 (contract layer extraction) + phase 4 (TCP-framed binding) + phase 5–6 (selector + client): roughly 5 days of architect-validated implementation + 1 day of BDD + 0.5 day of gate-2 audit.
- Adversary review (gate-1) likely surfaces 4–8 HIGH findings to round-trip; budget another 1 day for amendments.
- Phases 10–11 (RDMA bindings) require hardware to validate; not on the gate-1 critical path.

---

## §18 Alternatives considered

### A1. Add the data-plane methods to `ControlService` instead of a new service

**Rejected** because `ControlService` is admin-shaped (org/namespace/device CRUD). Mixing data-plane (millions of ops/sec) with admin (low-rate) creates per-call routing ambiguity and hides the data-plane SLA contract. Two services — even on the same port — keep the SLAs separate.

### A2. Reuse the S3 HTTP gateway via a "low-overhead profile" (no SigV4)

**Rejected**. Even without SigV4 the HTTP/1 path pays the framing tax. HTTP/2 multiplexing + protobuf framing is materially cheaper. RDMA bypasses framing entirely.

### A3. Bake gRPC into the contract (single transport)

**Rejected**. The 2026-05-05 first draft of this ADR did exactly this. The 2026-05-06 redesign separates contract from binding because the deployment shape (HPC&AI storage on heterogeneous fabrics) requires it. Locking the contract to gRPC forecloses Slingshot, IB, RoCEv2 — the exact deployments kiseki targets. The first-draft framing (gRPC tax math, alternative A3 "rejected: lose tooling") was correct *under the assumption that gRPC was the only binding*; once we ship multiple bindings, the tooling argument applies to the gRPC binding alone (which we keep), not to the contract.

### A4. Sharding compositions (DashMap) BEFORE shipping ADR-042

**Rejected as a precursor**. The 2026-05-06 measurement showed the protocol-layer gap dominates today's regression; native bindings close most of it without composition sharding. Sharding becomes a follow-up ADR if a measured ceiling warrants it.

### A5. Server-decrypt-only (drop TrustedCompute mode)

**Rejected**. HPC training workloads on tightly-controlled clusters can satisfy the trust assumption, and GPU-direct is the differentiator vs server-decrypt-only systems. TrustedCompute opt-in (with explicit `crypto_shred_policy = best_effort` acknowledgment) is the right balance.

### A6. Lease semantics WITHOUT fencing tokens

**Rejected**. The 2026-05-05 gate-0 adversary review identified split-brain on partition-heal as an unacceptable correctness gap. Fencing tokens (Lamport pattern) are the standard fix; the audit-trail benefit is also a win.

### A7. Operator-selected single binding at build time

**Rejected**. Operators know their hardware but shouldn't have to know kiseki's transport-binding taxonomy. Auto-detection at startup is what they actually want; single binary, runtime probe, banner that tells them what was picked. Build-time feature flags re-introduce the build-environment concerns the architecture is designed to abstract over.

**Note on build-host requirements**: this rejection is about *deployment-time* binding selection, not the build environment itself. The build host MUST have `libibverbs-dev` + `libfabric-dev` installed for the RDMA bindings to compile. This is a build-environment requirement (an HPC&AI storage project's reasonable baseline; identical to requiring `protoc` for gRPC codegen), not a build-time deployment decision. The same binary, built with these system deps, runs everywhere — no separate "kiseki-hpc" vs "kiseki-cloud" build flavors.

### A8. Per-tenant binding selection

**Rejected**. Bindings are deployment infrastructure: they reflect what NICs the operator wired and what `.so` files are on the host. Tenants don't get to pick at that layer. Per-tenant *quality-of-service* tiering (which deployments offer which latency class) is a future operations-ADR concern; per-tenant *binding selection* is not a useful abstraction.

---

## §19 Status / open items for gate-1

- Architect: this draft incorporates three rounds of gate-1 redesign findings (r1: 4 HIGH + 6 MEDIUM + 3 LOW + 3 cross-cutting; r2: 1 HIGH + 6 MEDIUM + 4 LOW + 3 cross-cutting; r3: 0 HIGH + 4 MEDIUM + 4 LOW + 3 cross-cutting), all closed in successive amendment passes. **PASS** — phase 0 implementation is unblocked.
- Implementer:
  - Phase 1 (gRPC binding) is already implemented (was the original draft's scope). It refactors into the new layout (contract types in `kiseki-proto::native_contract`, `NativeTransportServer` impl in `kiseki-transport::native::grpc`) — no business-logic rewrite, only restructuring.
  - Phases 0 + 2–8 are new work for v1 (contract types, ServerImpl, TCP-framed binding, selector, client, BDD with §16.1 phase 6 fault-injection scenarios, profile driver, perf re-measure).
  - Phases 9–10 (ibverbs + libfabric bindings) require RDMA hardware to validate against §14.1. **Hard prerequisites for shipping**: per-binding hard-close discipline (§3.2.2 — verified via 1000-close-cycle stress test); `DrainState` lifecycle (§1.7.1 — wired through `kiseki-control` per ADR-035); `RequestPrincipal` arch-check CI gate (§1.8); for phase 10 specifically, all four DoS mitigations from §2.4.2.1 (I-NG26).
- Operator docs (`docs/admin/native-gateway.md`): pending implementation. Captures cert issuance, env vars (including `KISEKI_NATIVE_PROBE_TIMEOUT_MS`, `KISEKI_NATIVE_DRAIN_BUDGET_MS`, `KISEKI_NATIVE_DRAIN_GRACEFUL_RELEASE_MS`, `KISEKI_NATIVE_CXI_ATTEST_*`, `KISEKI_NATIVE_CXI_VERIFY_*`, `KISEKI_NATIVE_*_LIB`), observability scrape paths, **per-binding deployment recipes** (mixed-NIC clusters per §3.2; system-library env vars per §2.3 / §2.4; `CxiAttestationEnvelope` rollout per §2.4.2; pre-1.0 no-rolling-upgrade discipline per §13.2; recommended k8s `startupProbe` settings per §3.1; cross-node replay aggregate-alarm PromQL per §12.1).
- Build phases (`specs/architecture/build-phases.md`): the 11 phases (0–10) from §16.1 added in the same revision.
- The optimization backlog (`docs/performance/optimization-backlog.md`) lists the future ADRs that build on top of ADR-042 (adaptive binding selection per §3.6, EFA libfabric provider per §2.4.3, optional binding-specific verbs per §13.3, cluster-wide cxi nonce store per §2.4.2 cross-node-replay note, DEK cache, FUSE migration, resume tokens for streaming uploads per §13).
- Invariants added to `specs/invariants.md`: **I-NG25** (cxi attestation), **I-NG26** (cxi DoS controls), **I-NG27** (contract/binding architecture), **I-NG28** (DrainState lifecycle).
- Failure modes added to `specs/failure-modes.md`: **F-NG14** (binding-probe failure, P3), **F-NG15** (RDMA QP-cleanup leak under churn, P2).
- Ubiquitous-language additions: service contract, transport binding, CXI attestation envelope, drain state, in-flight (drain accounting).
