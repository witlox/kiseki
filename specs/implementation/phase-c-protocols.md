# Phase C: Protocol Interface Delivery (S3 + NFS + FUSE)

## Context

Phases B (Python e2e) and A (BDD harness real) are complete. The
system is validated across process boundaries via Docker. The core
data path works (gateway → encrypt → chunk → composition → log → view).

Phase C delivers all three protocol interfaces as an integrated
package — S3, NFS, and FUSE — making the system accessible to real
workloads for the first time.

## Architecture

All three protocols share `GatewayOps` (read/write with encrypt/decrypt):

```
S3 HTTP (:9000)  ──┐
NFS TCP (:2049)  ──┤──→ GatewayOps ──→ Crypto + Chunk + Composition + Log
FUSE (client)    ──┘
```

S3 and NFS run **in-process** with `kiseki-server` (ADR-019).
FUSE runs **client-side** in `kiseki-client` on compute nodes.

---

## C.1: S3 HTTP Gateway (Low risk)

axum 0.7 is already in Cargo.lock. Domain types exist in `s3.rs`.

### Files

| File | Action |
|------|--------|
| `crates/kiseki-gateway/Cargo.toml` | Add `axum`, `tokio` (features: net, macros) |
| `crates/kiseki-gateway/src/s3_server.rs` | New: axum router + handlers |
| `crates/kiseki-server/src/config.rs` | Add `s3_addr: SocketAddr` (default `:9000`) |
| `crates/kiseki-server/src/runtime.rs` | Build `InMemoryGateway`, spawn S3 HTTP server |
| `Dockerfile.server` | Expose port 9000 |
| `docker-compose.yml` | Map port 9000 |

### S3 API subset (MVP)

| Method | Path | Maps to |
|--------|------|---------|
| `PUT` | `/:bucket/:key` | `S3Gateway::put_object` |
| `GET` | `/:bucket/:key` | `S3Gateway::get_object` |
| `HEAD` | `/:bucket/:key` | metadata (size, etag) |
| `DELETE` | `/:bucket/:key` | `CompositionOps::delete` |

No SigV4 auth initially (dev mode). Bucket = namespace, key = composition path.
Tenant derived from well-known bootstrap tenant (e2e) or future mTLS cert.

### E2E tests

| File | Tests |
|------|-------|
| `tests/e2e/test_s3_gateway.py` | PUT → GET roundtrip, HEAD, DELETE, not-found |
| `tests/e2e/helpers/s3.py` | `requests`-based S3 helper (no SigV4 needed) |

---

## C.2: NFS Gateway — NFSv3 + NFSv4.2 (Medium risk)

Both NFSv3 (stateless, HPC ubiquity) and NFSv4.2 (sessions, IO_ADVISE,
server-side copy). Core ops shared, wire format differs.

### Architecture

```
NFSv3 ONC RPC (:2049)  ──┐
                          ├──→ nfs_ops.rs (shared) ──→ NfsGateway<GatewayOps>
NFSv4.2 COMPOUND (:2049) ──┘
         ↕
    nfs_xdr.rs (XDR codec, shared)
```

Version negotiated at mount time. Single port 2049, TCP only.

### Files

| File | Purpose |
|------|---------|
| `crates/kiseki-gateway/src/nfs_xdr.rs` | XDR encode/decode (shared by v3+v4) |
| `crates/kiseki-gateway/src/nfs_ops.rs` | Shared ops: lookup, read, write, getattr, readdir, create |
| `crates/kiseki-gateway/src/nfs3_server.rs` | NFSv3 ONC RPC dispatcher (program 100003, version 3) |
| `crates/kiseki-gateway/src/nfs4_server.rs` | NFSv4.2 COMPOUND dispatcher + session/lease state |
| `crates/kiseki-gateway/src/nfs_server.rs` | TCP listener, version routing |
| `crates/kiseki-server/src/config.rs` | Add `nfs_addr: SocketAddr` (default `:2049`) |
| `crates/kiseki-server/src/runtime.rs` | Spawn NFS TCP server |
| `docker-compose.yml` | Map port 2049 |

### Shared ops (both versions)

| Operation | NFSv3 proc | NFSv4.2 op | Maps to |
|-----------|-----------|------------|---------|
| Ping | NULL | — | no-op |
| Read | READ | READ | `NfsGateway::read` |
| Write | WRITE | WRITE | `NfsGateway::write` |
| Lookup | LOOKUP | LOOKUP | namespace + path → filehandle |
| Getattr | GETATTR | GETATTR | composition metadata |
| Readdir | READDIR | READDIR | list compositions |
| Create | CREATE | OPEN(create) | `GatewayOps::write` |
| Remove | REMOVE | REMOVE | `CompositionOps::delete` |

### NFSv4.2-specific ops (after NFSv3 works)

| Op | Purpose | Kiseki mapping |
|----|---------|----------------|
| EXCHANGE_ID | Session setup | Tenant auth (mTLS cert) |
| CREATE_SESSION | Session state | Per-client lease |
| SEQUENCE | Request ordering | Slot/sequence validation |
| IO_ADVISE | I/O hints | → Advisory subsystem (ADR-020) |
| COPY | Server-side copy | Composition clone (no data movement) |
| SEEK | Sparse file holes | Beyond MVP |

### Implementation order

1. **XDR codec** — shared encoder/decoder for basic XDR types
2. **Shared ops** — file handle management, path resolution, stat
3. **NFSv3 dispatcher** — ONC RPC framing + procedure dispatch
4. **E2E test** — mount via NFSv3, write, read, ls
5. **NFSv4.2 COMPOUND** — op-by-op dispatch, session management
6. **NFSv4.2 extras** — IO_ADVISE → advisory, COPY → clone

### E2E tests

| File | Tests |
|------|-------|
| `tests/e2e/test_nfs_gateway.py` | NFSv3: mount + write + read + ls |
| `tests/e2e/test_nfs4_gateway.py` | NFSv4.2: mount + write + read (after sessions) |

### Risk mitigation

NFSv3 ships first (simpler, covers HPC). NFSv4.2 COMPOUND follows.
Domain logic (NfsGateway) is already tested independently.

---

## C.3: FUSE Client Mount (Low risk, client-side)

FUSE runs on compute nodes, not in the server. Uses the `fuser` crate
(mature, actively maintained, supports macOS via macFUSE/FUSE-T).

### Files

| File | Action |
|------|--------|
| `crates/kiseki-client/Cargo.toml` | Add `fuser` (feature-gated: `fuse`) |
| `crates/kiseki-client/src/fuse_fs.rs` | New: `KisekiFuse` implementing `fuser::Filesystem` |
| `crates/kiseki-client/src/lib.rs` | Expose `fuse_fs` under `#[cfg(feature = "fuse")]` |

### FUSE ops (MVP)

| Syscall | Maps to |
|---------|---------|
| `open` | track filehandle |
| `read` | `GatewayOps::read` via local gateway |
| `write` | `GatewayOps::write` via local gateway |
| `lookup` | namespace path resolution |
| `getattr` | composition metadata |
| `readdir` | list compositions |
| `create` | `GatewayOps::write` |
| `unlink` | `CompositionOps::delete` |

### Testing

FUSE can't run in Docker easily. Test on the host:
- Unit test: `KisekiFuse` with mock gateway (no mount)
- Integration: mount tmpdir, write file, read back, unmount

### Platform notes

| Platform | FUSE support |
|----------|-------------|
| Linux | Native (kernel module, no extra install) |
| macOS | macFUSE or FUSE-T (user install required) |
| Docker | Needs `--device /dev/fuse --cap-add SYS_ADMIN` (Linux only) |

---

## C.4: Cross-Protocol E2E Tests

The crown jewel — proves data flows across all protocols.

| File | Test |
|------|------|
| `tests/e2e/test_cross_protocol.py` | Write via S3 PUT → read via gRPC `ReadDeltas` (verify delta in log) |
| `tests/e2e/test_cross_protocol.py` | Write via gRPC `AppendDelta` → read via S3 GET (requires namespace/shard wiring) |

---

## Execution Order

```
C.1 S3 HTTP ──→ C.1 e2e tests ──→ C.2 NFS TCP ──→ C.2 e2e tests ──→ C.3 FUSE ──→ C.4 cross-protocol
     │                                  │                                  │
     │ axum, low risk                   │ XDR codec, medium risk           │ fuser, client-side
     ▼                                  ▼                                  ▼
   Docker port 9000              Docker port 2049                    Host-only (no Docker)
```

Each sub-phase gets adversarial review. C.1 first because it's
lowest risk and validates the server wiring pattern. C.2 follows
the same pattern. C.3 is independent (client-side).

## Test Count Projections

| Sub-phase | New Tests | Notes |
|-----------|-----------|-------|
| C.1 S3 | +4 Python e2e | PUT/GET/HEAD/DELETE |
| C.2 NFS | +2 Python e2e | mount+write+read, readdir |
| C.3 FUSE | +3 Rust integration | open/read/write/unlink (host only) |
| C.4 Cross | +2 Python e2e | S3→gRPC, gRPC→S3 |
| **Total** | **+11** | |

## Key Dependencies

| Crate | Purpose | Risk |
|-------|---------|------|
| `axum` 0.7 | S3 HTTP server | Already in Cargo.lock |
| XDR/RPC codec | NFS wire format | May need hand-rolled or `onc-rpc` |
| `fuser` | FUSE filesystem | Mature, well-maintained |

## Verification

After all sub-phases:
1. `make e2e` — Docker compose up, all Python e2e pass (8+ tests)
2. `cargo test` — all Rust tests pass (171+)
3. Manual: `curl -X PUT localhost:9000/bucket/key -d "data"` works
4. Manual: FUSE mount on host, `echo data > /mnt/kiseki/file`, `cat /mnt/kiseki/file` works
