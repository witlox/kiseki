# Transport Layer

The `kiseki-transport` crate provides a pluggable transport abstraction
for bidirectional byte-stream connections. It ships with a TCP+TLS
reference implementation and feature-flagged support for HPC fabric
transports.

---

## Transport trait

The `Transport` trait is the core abstraction:

```rust
pub trait Transport: Send + Sync + 'static {
    type Connection: Connection;
    async fn connect(&self, addr: SocketAddr) -> Result<Self::Connection>;
    async fn listen(&self, addr: SocketAddr) -> Result<Listener>;
}

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {
    fn peer_identity(&self) -> Option<&PeerIdentity>;
}
```

All components (client, server, Raft) use this trait, enabling transport
selection without code changes.

---

## TCP+TLS (reference implementation)

The `TcpTlsTransport` is always available and serves as the universal
fallback:

- **mTLS**: Cluster CA validation with per-tenant certificates (I-Auth1,
  I-K13)
- **SPIFFE**: SAN-based SVID validation for workload identity (I-Auth3)
- **CRL**: Optional certificate revocation list support via
  `KISEKI_CRL_PATH`
- **Connection pooling**: Configurable pool size per peer
- **Keepalive**: TCP keepalive for connection health
- **Timeouts**: Configurable connect, read, and write timeouts

Configuration: `TlsConfig` with CA cert, node cert, node key, and
optional CRL path.

---

## RDMA verbs (feature: `verbs`)

Native InfiniBand and RoCEv2 support for low-latency HPC fabrics:

- **InfiniBand**: Direct RDMA over InfiniBand fabric (`VerbsIb`)
- **RoCEv2**: RDMA over Converged Ethernet (`VerbsRoce`)
- **Device selection**: Auto-detects the first available IB device, or
  uses the device named in `KISEKI_IB_DEVICE`
- **Zero-copy**: RDMA read/write for chunk data transfer

The verbs module uses `unsafe` code for FFI calls to `libibverbs`.
Each unsafe block has a per-block SAFETY comment.

---

## CXI/libfabric (feature: `cxi`)

HPE Slingshot fabric support via libfabric:

- **CXI provider**: Lowest-latency transport on Slingshot-equipped systems
- **libfabric**: Uses the libfabric API (`fi_*` calls) for fabric
  operations
- **Feature-flagged**: Only compiled when `cxi` feature is enabled

The CXI module uses `unsafe` code for FFI calls to libfabric.

---

## FabricSelector

The `FabricSelector` provides priority-based transport selection with
automatic failover:

```
Priority 0: CXI        (Slingshot, lowest latency)
Priority 1: VerbsIb    (InfiniBand)
Priority 2: VerbsRoce  (RoCEv2)
Priority 3: TcpTls     (always available, universal fallback)
```

At boot, the selector probes for available transports (hardware presence
check). On connection, it selects the highest-priority available transport.
On failure, it falls back to the next-best transport.

The `TransportHealthTracker` monitors transport health and marks transports
as unhealthy after repeated failures, temporarily removing them from
selection until they recover.

---

## GPU-direct (planned)

Future support for direct GPU memory access:

- **NVIDIA cuFile** (feature: `gpu-cuda`): GPUDirect Storage for direct
  NVMe-to-GPU data transfer
- **AMD ROCm** (feature: `gpu-rocm`): ROCm-based GPU direct access

These features bypass CPU memory for chunk data, reducing latency for
AI training workloads.

---

## NUMA-aware thread pinning

The `NumaTopology` module provides NUMA-aware thread pinning for optimal
memory locality:

- Auto-detects NUMA topology on Linux via `sched_setaffinity`
- Pins I/O threads to the NUMA node closest to the network device
- Reduces cross-NUMA memory access latency for high-throughput workloads

---

## Metrics and health

The transport layer exports Prometheus metrics via `TransportMetrics`:

- Connection count per transport type
- Bytes sent/received per transport
- Connection errors and failover events
- Latency histograms per transport

Health tracking (`TransportHealthTracker`) provides per-transport health
status for the selector's failover decisions.

---

## Invariant mapping

| Invariant | How the transport layer enforces it |
|---|---|
| I-K2 | All data on the wire is TLS-encrypted (or pre-encrypted chunks over CXI) |
| I-K13 | mTLS with Cluster CA validation on every data-fabric connection |
| I-Auth1 | Client certificate required on data fabric |
| I-Auth3 | SPIFFE SVID validation via SAN matching |
