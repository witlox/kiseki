# Fabric Transport Subplan: CXI + IB/RoCEv2 + Validation

**Date**: 2026-04-23
**Parent**: `specs/implementation/mvp-to-production-plan.md`
**Workstream items**: 4.1–4.6, 3.1, 7.3, 6.6, 9.1

## Baseline

Transport layer has:
- `Transport` trait (`kiseki-transport/src/traits.rs`) — pluggable abstraction
- `TcpTlsTransport` — reference impl with mTLS, peer identity extraction
- Feature-gated empty stubs: `cxi.rs`, `verbs.rs`
- `TransportSelector` in client — priority-based selection (RDMA > TCP > gRPC)
- Raft TCP transport (`kiseki-raft/src/tcp_transport.rs`) — length-prefixed JSON,
  MAX_RAFT_RPC_SIZE=128MB, TLS infrastructure wired but not activated
- TLS config: `TlsConfig`, `CrlCache`, `SpiffeId` all production-ready
- `#![deny(unsafe_code)]` on `kiseki-transport` — must be lifted for FFI modules only

Hardware available on separate machine (not CI). CI validates: compilation,
unit tests, SoftROCE (if available), mock transport test suites.

---

## Phase A: Foundation (WS 4.1 + 3.1)

**Goal**: Extend Transport trait for production use. Activate Raft mTLS.

### A1: Transport trait extensions

File: `kiseki-transport/src/traits.rs`

Add to `Transport` trait:
- `fn listen(&self, addr: SocketAddr) -> impl Future<...> + Send` — server-side accept
- Keep `connect()` as-is (client-side)

New file: `kiseki-transport/src/pool.rs`
- `ConnectionPool<T: Transport>` — per-endpoint connection pooling
  - `get(addr) -> Result<PooledConn<T::Conn>>` — reuse or create
  - `max_idle: usize` (default 4), `max_per_endpoint: usize` (default 8)
  - `idle_timeout: Duration` (default 30s) — evict stale connections
  - Health check: periodic ping on idle connections
- `PooledConn<C>` wrapper — returns to pool on drop instead of closing

New file: `kiseki-transport/src/health.rs`
- `TransportHealthTracker` — per-transport availability + latency tracking
  - `record_success(transport, latency: Duration)`
  - `record_failure(transport, error: &TransportError)`
  - `is_healthy(transport) -> bool` — circuit breaker (5 failures in 30s → unhealthy)
  - `current_latency(transport) -> Option<Duration>` — exponential moving average
  - `mark_for_reprobe(transport)` — schedule re-check

New file: `kiseki-transport/src/metrics.rs`
- `TransportMetrics` — counters and histograms (struct, not Prometheus yet)
  - `connections_opened: u64`, `connections_failed: u64`
  - `bytes_sent: u64`, `bytes_received: u64`
  - `rpc_count: u64`, `rpc_errors: u64`
  - `latency_samples: VecDeque<Duration>` (sliding window, last 1000)
  - `p50()`, `p99()`, `p999()` computed on demand

### A2: Raft mTLS activation

File: `kiseki-raft/src/tcp_transport.rs`

Currently `_tls_config` on `TcpNetwork` is unused. Activate it:
- `rpc_call()` → `rpc_call_tls()`: when `tls_config` is Some, wrap `TcpStream`
  in `TlsStream` before sending length-prefixed data
- `run_raft_rpc_server()`: when `tls_config` is Some, wrap accepted `TcpStream`
  in server-side `TlsStream` using `TlsAcceptor`
- Reject plaintext connections when mTLS is configured

File: `kiseki-server/src/runtime.rs`
- Build `rustls::ClientConfig` and `rustls::ServerConfig` from `cfg.tls`
- Pass to `TcpNetworkFactory::with_tls()` and `run_raft_rpc_server()`

### Validation A

| Check | Method | CI? |
|-------|--------|-----|
| Pool reuses connections | Unit test: connect twice, assert pool hit | Yes |
| Pool evicts idle connections | Unit test: sleep past idle_timeout, assert evicted | Yes |
| Circuit breaker trips after 5 failures | Unit test: inject 5 failures, assert unhealthy | Yes |
| Circuit breaker recovers on success | Unit test: trip → success → healthy again | Yes |
| Metrics p50/p99 correct | Unit test: feed known latencies, assert percentiles | Yes |
| Raft mTLS handshake succeeds | Integration test: 3-node in-process cluster with rcgen certs | Yes |
| Raft rejects plaintext when mTLS configured | Integration test: plaintext connect → rejected | Yes |
| Raft consensus works over TLS | Integration test: write → replicate → read on follower | Yes |
| No regression | `cargo test --workspace`, BDD count unchanged | Yes |

**Estimated effort**: 3–4 sessions

---

## Phase B: Fabric Implementations (WS 4.2 + 4.3)

**Goal**: Full RDMA verbs transport + CXI/libfabric transport. Parallel work.

### B1: RDMA verbs transport (InfiniBand + RoCEv2 shared layer)

File: `kiseki-transport/src/verbs.rs` (replace stub)

Dependencies (Cargo.toml):
- `rdma-sys = "0.4"` (or `rdma-core-sys`) — feature-gated behind `verbs`
- `libc` (already in workspace)

Implementation:
```
VerbsTransport {
    context: *mut ibv_context,         // from ibv_open_device()
    pd: *mut ibv_pd,                   // protection domain
    comp_channel: *mut ibv_comp_channel,
    cq: *mut ibv_cq,                   // completion queue
    port_num: u8,
    gid_index: u8,
}
```

Connection lifecycle:
1. `ibv_get_device_list()` → pick first device (or env `KISEKI_IB_DEVICE`)
2. `ibv_open_device()` → context
3. `ibv_alloc_pd()` → protection domain (per-tenant isolation possible)
4. `ibv_create_comp_channel()` + `ibv_create_cq()` → completion queue
5. `connect(addr)`:
   - `ibv_create_qp()` — RC (Reliable Connected) queue pair
   - Exchange QP info via TCP sideband (GID, QPN, PSN)
   - `ibv_modify_qp()`: RESET → INIT → RTR → RTS
   - Return `VerbsConnection { qp, mr_pool }`
6. Send/recv via `ibv_post_send()` / `ibv_post_recv()` with length-prefixed framing
7. Memory registration: `ibv_reg_mr()` for send/recv buffers
   - Pre-register a pool of buffers (avoid per-message registration overhead)

`VerbsConnection` implements `Connection` trait:
- `AsyncRead`/`AsyncWrite` bridge: post RDMA send/recv, poll CQ via tokio task
- `peer_identity()`: extracted during TCP sideband handshake (mTLS on sideband)

Safety: every `unsafe` block gets a `// SAFETY:` comment. Module-level
`#![allow(unsafe_code)]` only on `verbs.rs`, rest of crate stays `#![deny(unsafe_code)]`.

Bulk data path (future optimization, not MVP):
- RDMA Read for chunk data: register remote memory, one-sided read
- Avoids target CPU involvement for large transfers

### B2: CXI/libfabric transport (Slingshot)

File: `kiseki-transport/src/cxi.rs` (replace stub)

Dependencies (Cargo.toml):
- `libfabric-sys` — FFI to libfabric (feature-gated behind `cxi`)
  - If crate doesn't exist: raw FFI bindings via `bindgen` from `/usr/include/rdma/fabric.h`

Implementation:
```
CxiTransport {
    fabric: *mut fid_fabric,
    domain: *mut fid_domain,
    av: *mut fid_av,           // address vector
    eq: *mut fid_eq,           // event queue
    cq_tx: *mut fid_cq,       // TX completion queue
    cq_rx: *mut fid_cq,       // RX completion queue
}
```

Connection lifecycle:
1. `fi_getinfo(FI_EP_RDM, "cxi")` — discover CXI provider
2. `fi_fabric()` → fabric handle
3. `fi_domain()` → domain handle
4. `fi_av_open()` → address vector (peer addressing)
5. `fi_cq_open()` × 2 → TX and RX completion queues
6. `connect(addr)`:
   - `fi_endpoint()` — RDM (Reliable Datagram) endpoint
   - `fi_ep_bind()` — bind to AV, CQs, EQ
   - `fi_enable()` — activate endpoint
   - `fi_av_insert()` — resolve peer address (Service ID based, not IP)
   - Return `CxiConnection { ep, peer_fi_addr, mr_pool }`
7. Send/recv via `fi_send()` / `fi_recv()` with completion polling
8. Memory registration: `fi_mr_reg()` for zero-copy buffers

CXI-specific:
- VNI (Virtual Network Interface) for tenant isolation: `FI_OPT_CXI_VNI`
- Service ID addressing: `fi_av_insert()` with `FI_ADDR_CXI`
- Adaptive routing hints: `FI_OPT_CXI_ADAPTIVE_ROUTING`
- No IP/TCP needed — pure fabric addressing

`CxiConnection` implements `Connection`:
- `AsyncRead`/`AsyncWrite` bridge via tokio + CQ polling
- `peer_identity()`: from VNI + fabric-level authentication
- `remote_addr()`: synthetic SocketAddr from CXI service ID (for API compat)

### Validation B

| Check | Method | CI? |
|-------|--------|-----|
| `cargo check --features verbs` compiles | Feature gate CI job | Yes |
| `cargo check --features cxi` compiles | Feature gate CI job | Yes |
| Verbs unit tests with mock context | Unit tests (no hardware) | Yes |
| CXI unit tests with mock fabric | Unit tests (no hardware) | Yes |
| Abstract transport test suite passes (verbs) | Shared test harness | Yes* |
| Abstract transport test suite passes (cxi) | Shared test harness | Yes* |
| SoftROCE round-trip (1000 × 4KB messages) | Integration test (if SoftROCE avail) | Maybe |
| Verbs on real IB hardware | Manual: send 1GB, verify throughput | No |
| CXI on Slingshot hardware | Manual: send 1GB, verify throughput | No |
| Raft consensus over verbs | Manual: 3-node cluster, 100 commits | No |
| Raft consensus over CXI | Manual: 3-node cluster, 100 commits | No |
| Zero `unsafe` without SAFETY comment | `grep -n unsafe verbs.rs cxi.rs` | Yes |

*Abstract test suite uses mock transport that simulates the same message flow.

New file: `kiseki-transport/tests/transport_suite.rs`
- Shared test harness: any `Transport` impl can be plugged in
- Tests: connect/send/recv roundtrip, concurrent connections, large message,
  connection drop recovery, peer identity extraction
- Parameterized: `TcpTlsTransport` (always), `VerbsTransport` (feature),
  `CxiTransport` (feature)

**Estimated effort**: 5–8 sessions per transport (parallel)

---

## Phase C: RoCEv2 + Transport Selection (WS 4.4 + 4.5)

**Goal**: RoCEv2 as verbs delta. Wire transport selection to real implementations.

### C1: RoCEv2 transport

File: `kiseki-transport/src/verbs.rs` (extend, not new file)

RoCEv2 shares the ibverbs layer from B1. Differences:
- GRH (Global Routing Header) required: set `ibv_ah_attr.is_global = 1`
- GID type: RoCEv2 uses IPv4/IPv6 mapped GIDs (not IB port GIDs)
- `ibv_modify_qp()` RTR: set `ah_attr.grh.dgid` from peer's GID
- MTU negotiation: `ibv_query_port()` → `active_mtu`, typically 4096 for RoCE
- ECN (Explicit Congestion Notification): kernel-level, not in userspace API
  but document tuning params: `KISEKI_ROCE_DSCP`, `KISEKI_ROCE_ECN`

Detection at boot:
- `ibv_query_port()` → `link_layer` field
  - `IBV_LINK_LAYER_INFINIBAND` → IB mode
  - `IBV_LINK_LAYER_ETHERNET` → RoCEv2 mode
- Auto-configure GRH based on link layer

New enum:
```rust
pub enum VerbsMode {
    InfiniBand,
    RoCEv2 { dscp: u8 },
}
```

### C2: Transport selection (production)

File: `kiseki-transport/src/selector.rs` (new, replaces client-side TransportSelector)

Move transport selection to `kiseki-transport` (shared by client + server):
```rust
pub struct FabricSelector {
    transports: Vec<Box<dyn DynTransport>>,   // priority-ordered
    health: TransportHealthTracker,            // from Phase A
    pool: ConnectionPool<DynConn>,             // from Phase A
}
```

Boot-time probing:
1. Check `/sys/class/cxi/` — CXI devices present? → add CxiTransport
2. Check `/sys/class/infiniband/` — IB/RoCE devices present?
   - Query `link_layer` → add VerbsTransport(IB) or VerbsTransport(RoCEv2)
3. TCP+TLS always available as fallback

Runtime failover:
- On `connect()` failure: `health.record_failure()`, try next transport
- On circuit breaker trip: skip transport until reprobe
- On recovery: `health.record_success()`, re-promote

Wire into:
- `kiseki-server/src/runtime.rs` — construct `FabricSelector` at boot
- `kiseki-raft/src/tcp_transport.rs` — `RaftNetworkFactory` uses selector
  instead of raw TCP (rename to `RaftNetworkFactory`)
- `kiseki-client/src/transport_select.rs` — replace with `FabricSelector`

### Validation C

| Check | Method | CI? |
|-------|--------|-----|
| RoCEv2 GRH configuration correct | Unit test: mock ibv_query_port returns Ethernet | Yes |
| Auto-detect IB vs RoCE | Unit test: mock sysfs, assert correct VerbsMode | Yes |
| Selector picks highest-priority available | Unit test: register 3 transports, assert order | Yes |
| Failover on connect failure | Unit test: first transport returns error, second succeeds | Yes |
| Circuit breaker skips failed transport | Unit test: trip breaker, assert skipped | Yes |
| Recovery after breaker reset | Unit test: trip → wait → reprobe → available | Yes |
| Raft uses selector for peer connections | Integration test: mock selector, verify called | Yes |
| SoftROCE round-trip (RoCEv2 mode) | Integration test (if SoftROCE avail) | Maybe |
| No regression | `cargo test --workspace` | Yes |

**Estimated effort**: 3–5 sessions

---

## Phase D: NUMA-Aware Thread Pinning (WS 4.6)

**Goal**: Pin I/O threads to NUMA node of associated NIC/NVMe controller.

### D1: NUMA topology detection

New file: `kiseki-transport/src/numa.rs`

```rust
pub struct NumaTopology {
    nodes: Vec<NumaNode>,
}

pub struct NumaNode {
    id: u32,
    cpus: Vec<u32>,              // online CPUs on this node
    devices: Vec<String>,        // PCI devices (NICs, NVMe)
    memory_mb: u64,
}
```

Detection (Linux):
- `/sys/devices/system/node/node*/cpulist` → CPU affinity
- `/sys/class/infiniband/*/device/numa_node` → IB NIC NUMA node
- `/sys/class/cxi/*/device/numa_node` → CXI NIC NUMA node
- `/sys/class/nvme/*/device/numa_node` → NVMe controller NUMA node
- Non-Linux: return single-node topology (no pinning)

### D2: Thread affinity

New file: `kiseki-transport/src/affinity.rs`

```rust
pub fn pin_current_thread(cpus: &[u32]) -> io::Result<()>
```

Linux: `sched_setaffinity()` via libc.
macOS/other: no-op with log warning.

### D3: Runtime wiring

File: `kiseki-server/src/runtime.rs`
- Detect NUMA topology at boot
- Find NIC NUMA node → pin transport I/O threads
- Find NVMe NUMA node → pin chunk I/O threads
- Log: "Pinned transport threads to NUMA node X (CPUs: ...)"

### Validation D

| Check | Method | CI? |
|-------|--------|-----|
| Topology detection parses sysfs | Unit test: mock sysfs tree | Yes |
| Single-node fallback on non-Linux | Unit test: non-Linux returns 1 node | Yes |
| pin_current_thread sets affinity (Linux) | Integration test on Linux CI runner | Yes |
| pin_current_thread is no-op on macOS | Unit test: returns Ok, no panic | Yes |
| No regression | `cargo test --workspace` | Yes |
| Before/after latency comparison | Manual: benchmark on NUMA hardware | No |
| Thread affinity verified via /proc | Manual: check Cpus_allowed on HW | No |

**Estimated effort**: 1–2 sessions

---

## Phase E: Network Failure Validation (WS 7.3)

**Goal**: Validate all three F-N failure modes with deterministic tests.

### E1: F-N2 — Client disconnect during write

Test: start a write (AppendDelta), kill the client mid-stream.
Verify: no orphan chunks, no partial deltas in shard state.

Implementation: integration test in `kiseki-transport/tests/`:
- Spin up in-process server
- Start write, drop connection after sending request but before reading response
- Read shard state: either fully committed or fully absent

### E2: F-N3 — Fabric transport failure → TCP fallback

Test: `FabricSelector` with mock transports.
1. Primary (mock "verbs") available → selected
2. Inject failure on primary → selector falls back to TCP
3. Verify: RPC succeeds on TCP, metrics show failover
4. Restore primary → selector re-promotes after reprobe
5. Verify: next RPC uses primary again

### E3: Raft survives transport failover

Test: 3-node Raft cluster using `FabricSelector`.
1. All nodes on mock "fast" transport
2. Kill "fast" transport on leader → follower becomes leader via TCP
3. Commit still succeeds
4. Restore "fast" → new connections use fast transport

### Validation E

| Check | Method | CI? |
|-------|--------|-----|
| No orphan chunks after client disconnect | Integration test | Yes |
| No partial deltas after client disconnect | Integration test | Yes |
| Failover from fabric to TCP | Unit test with mock transports | Yes |
| Recovery from TCP back to fabric | Unit test with mock transports | Yes |
| Raft leader election survives transport failover | Integration test (in-process) | Yes |
| Raft commit works after failover | Integration test (in-process) | Yes |

**Estimated effort**: 1–2 sessions

---

## Phase F: Benchmark + Hardware Validation (WS 6.6 + 9.1)

**Goal**: Measure transport performance. Validate assumptions from specs.

### F1: Benchmark harness

New file: `benches/transport_bench.rs` (or `tests/bench/`)

Measurements per transport (TCP-TLS, IB, RoCEv2, CXI):
- **Latency**: 10,000 × 64-byte messages, report p50/p99/p999
- **Throughput**: streaming 1GB, report MB/s
- **Concurrent**: 8 parallel streams × 1GB each, report aggregate MB/s
- **Small message rate**: 100,000 × 64-byte messages/sec
- **Raft commit latency**: 1000 commits with 4KB payloads, report p50/p99

### F2: Assumption validation

From `specs/assumptions.md`:
- CXI fabric latency: assumed < 2µs for small messages → **measure**
- NVMe write latency: assumed < 20µs for 4KB aligned writes → **measure**
- EC encode overhead: assumed < 5% CPU for 4+2 RS coding → **measure**
- HDD sequential throughput: assumed > 200 MB/s per drive → **measure**

### F3: Results documentation

Output: `specs/validation/transport-benchmarks.md`
- Table per transport: latency (p50/p99/p999), throughput, concurrency
- Comparison chart: TCP vs IB vs RoCE vs CXI
- Assumption validation: measured vs assumed, PASS/FAIL per assumption
- Any violated assumption → escalation to architect

### Validation F

| Check | Method | CI? |
|-------|--------|-----|
| Bench harness compiles and runs (TCP) | CI: run TCP-only bench | Yes |
| IB bench on real hardware | Manual: run on IB-equipped machine | No |
| RoCEv2 bench on real hardware | Manual: run on RoCE-equipped machine | No |
| CXI bench on Slingshot hardware | Manual: run on Slingshot-equipped machine | No |
| Assumption violations documented | Manual: review results doc | No |
| Results committed to specs/validation/ | Post-bench commit | No |

**Estimated effort**: 5–8 sessions

---

## Phase Dependency Graph

```
Phase A (foundation)
  ├── A1: transport extensions
  └── A2: Raft mTLS activation
         │
         ▼
Phase B (parallel)
  ├── B1: RDMA verbs ────────┐
  └── B2: CXI/libfabric      │
         │                    │
         ▼                    ▼
Phase C (RoCEv2 + selection)
  ├── C1: RoCEv2 (extends B1)
  └── C2: FabricSelector
         │
         ▼
Phase D (NUMA pinning)
         │
         ▼
Phase E (failure validation)
         │
         ▼
Phase F (benchmark + HW validation)
```

## CI Integration

### New CI jobs (`.github/workflows/ci.yml`)

```yaml
# Stage 2 additions:
- name: transport-verbs
  cmd: cargo check -p kiseki-transport --features verbs --locked
- name: transport-cxi
  cmd: cargo check -p kiseki-transport --features cxi --locked
```

### Hardware test runner (future, not CI)

Separate script: `tests/hw/run_transport_bench.sh`
- Detects available hardware (IB, RoCE, CXI)
- Runs appropriate bench suite
- Outputs results to `specs/validation/transport-benchmarks.md`
- Not in GH Actions — run manually on lab machines

## Total Estimated Effort

| Phase | Sessions | Hardware needed |
|-------|----------|-----------------|
| A: Foundation | 3–4 | No |
| B: Fabric impl | 10–16 | Build only (SoftROCE optional) |
| C: RoCEv2 + selection | 3–5 | No (SoftROCE optional) |
| D: NUMA pinning | 1–2 | No (mock sysfs) |
| E: Failure validation | 1–2 | No |
| F: Benchmark + HW | 5–8 | Yes (IB/RoCE/CXI hardware) |
| **Total** | **23–37** | Phases A–E: CI only. Phase F: lab. |

## Cross-cutting

- Every phase: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
- Every phase: BDD scenario count does not decrease
- Adversary review of all new `unsafe` blocks (Phases B, D)
- `#![allow(unsafe_code)]` only on `verbs.rs`, `cxi.rs`, `affinity.rs`
