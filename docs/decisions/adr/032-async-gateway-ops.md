# ADR-032: Async GatewayOps

**Status**: Accepted  
**Date**: 2026-04-24  
**Traces**: I-L2, I-L5, I-V3, I-WA2, I-C2, I-C5, I-L8

## Context

`GatewayOps` is a synchronous trait used by all three protocol gateways
(S3, NFS, FUSE) to perform reads and writes through the composition and
chunk stores. When the Raft-backed log store was introduced, the sync
trait required a sync→async bridge (`run_on_raft`) that blocks the
calling thread while waiting for Raft consensus.

Under concurrent load (≥ Raft runtime thread count), this causes thread
starvation: all Raft threads are occupied polling `client_write` futures,
leaving no thread for the Raft core loop to dispatch entries. The current
mitigation (`KISEKI_RAFT_THREADS = cpus/2`) works but wastes resources
and imposes a concurrency ceiling equal to the thread count.

For HPC/ML workloads with hundreds to thousands of concurrent writers,
the thread-per-request model is unsustainable. The gateway must not
block OS threads while waiting for Raft consensus.

## Decision

Make `GatewayOps` an async trait. All protocol gateways call async
methods directly. NFS and FUSE callers bridge async→sync via
`tokio::runtime::Handle::block_on` on a dedicated runtime (the reverse
of the current problem, but on threads they own — OS threads that are
explicitly meant to block).

### Trait change

```rust
// Before (sync)
pub trait GatewayOps: Send + Sync {
    fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError>;
    fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError>;
    fn list(...) -> Result<...>;
    fn delete(...) -> Result<...>;
    // ...
}

// After (async)
pub trait GatewayOps: Send + Sync {
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError>;
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError>;
    async fn list(...) -> Result<...>;
    async fn delete(...) -> Result<...>;
    // ...
}
```

### Mutex strategy

Replace `std::sync::Mutex` with `tokio::sync::Mutex` for
`CompositionStore` and `ChunkStore` in `InMemoryGateway`. Lock guards
must NOT be held across `.await` points that perform disk I/O or Raft
submissions — acquire, do in-memory work, drop, then await I/O.

### Protocol gateway changes

| Protocol | Current | After |
|----------|---------|-------|
| S3 (axum) | `block_in_place(\|\| gateway.write())` | `gateway.write().await` |
| NFS (std::thread) | `gateway.write()` | `rt.block_on(gateway.write())` on NFS thread |
| FUSE (fuser threads) | `gateway.write()` | `rt.block_on(gateway.write())` on fuser thread |

S3 becomes fully non-blocking. NFS and FUSE threads block as before,
but they own their threads (not tokio worker threads) so no starvation.

### LogOps bridge

`LogOps::append_delta` becomes async. The `run_on_raft` bridge is
removed — the Raft runtime's handle is used directly via `.await` from
async gateway methods. No `mpsc::recv` blocking, no thread starvation.

### Invariant preservation

The async conversion preserves all invariants by maintaining the same
happens-before ordering via `.await`:

| Invariant | Guarantee |
|-----------|-----------|
| I-L2 | Gateway awaits Raft commit before returning to client |
| I-L5 | Chunk writes awaited before composition finalize delta |
| I-V3 | Read-your-writes: `last_written_seq` set after awaited write |
| I-C2 | Refcount ops after awaited chunk confirm |
| I-C5 | Capacity check before async write submission |
| I-L8 | Shard membership validated before async rename |
| I-WA2 | Advisory lookups remain sync + bounded (≤500 µs timeout) |

### Concurrency model

With async GatewayOps, the concurrency ceiling becomes the tokio task
limit (effectively unbounded) instead of the thread count. Thousands
of concurrent writes share a fixed thread pool without starvation.

## Migration

Big-bang conversion. All callers updated in one pass:

1. Make `GatewayOps` async (trait + `InMemoryGateway` impl)
2. Replace `std::sync::Mutex` → `tokio::sync::Mutex` in gateway
3. Make `LogOps` async, remove `run_on_raft` bridge
4. Update S3 handlers: remove `block_in_place`, use `.await`
5. Update NFS server: add `rt.block_on()` wrapper on NFS threads
6. Update FUSE daemon: add `rt.block_on()` wrapper on fuser threads
7. Update all tests + BDD step definitions
8. Remove `KISEKI_RAFT_THREADS` (no longer needed)

## Consequences

**Benefits**:
- No thread starvation under any concurrency level
- S3 handler is fully non-blocking (proper async axum)
- Removes `run_on_raft`, `block_in_place`, `KISEKI_RAFT_THREADS`
- Single Raft runtime (no dedicated runtime needed)
- Clean async-all-the-way data path

**Costs**:
- Large refactor touching all protocol gateways and tests
- NFS/FUSE need a tokio runtime handle for `block_on`
- `tokio::sync::Mutex` has slightly higher per-lock overhead than
  `std::sync::Mutex` (but eliminates thread starvation)
- Async trait requires `Send + 'static` bounds on futures

**Risks**:
- `tokio::sync::Mutex` held across `.await` can cause deadlocks if
  not careful. Mitigated by code review rule: never hold gateway
  mutex across Raft submission or disk I/O.
- NFS/FUSE `block_on` on a non-tokio thread: works correctly but
  must not be called from within a tokio context (same issue we
  already solved with `std::thread::spawn` for runtime creation).

## Implementation Notes (2026-04-24)

**CompositionOps reverted to sync.** The initial implementation made
`CompositionOps` async, but holding `tokio::sync::Mutex<CompositionStore>`
across `emit_delta().await` serialized all writes behind a single Raft
round-trip — the same bottleneck as before, just without thread starvation.

**Final architecture:**
- `GatewayOps`: async (S3 handlers await directly)
- `LogOps`: async (Raft consensus)
- `CompositionOps`: **sync** (in-memory HashMap operations only)

**Gateway write pattern (lock-free):**
1. Lock compositions → `create()` (sync, microseconds) → drop lock
2. Emit delta to log (async, Raft consensus, ~8ms) — no lock held
3. If emission fails, re-acquire lock and rollback (PIPE-ADV-1)

**NFS/FUSE bridge:** `block_gateway()` helper uses `block_in_place`
when on a tokio worker thread (tests), or direct `block_on` on OS
threads (production NFS/FUSE daemon).

**Result:** 1MB write throughput: 39.5 → 380.2 MB/s (9.6x improvement).
32 concurrent S3 PUTs complete in 50ms with no deadlock.
