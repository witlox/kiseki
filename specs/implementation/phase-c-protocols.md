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

## C.2: NFS Gateway (Medium risk)

No Rust NFSv4 server crate found in the ecosystem. Two options:

**Option A**: Minimal NFSv3 over TCP using the `nfs3` wire format.
NFSv3 is simpler than v4 (stateless, no COMPOUND), and Rust has
basic XDR/RPC encoding via `onc-rpc` or hand-rolled.

**Option B**: Implement a minimal custom NFSv4.1 TCP handler with
just READ/WRITE/LOOKUP/GETATTR/READDIR operations.

**Recommended**: Option A — NFSv3 over TCP. Simpler protocol, maps
cleanly to `NfsGateway::read`/`write`, clients can mount via
`mount -t nfs -o nfsvers=3,tcp host:/export /mnt`.

### Files

| File | Action |
|------|--------|
| `crates/kiseki-gateway/Cargo.toml` | Add NFS wire format deps (or hand-roll XDR) |
| `crates/kiseki-gateway/src/nfs_server.rs` | New: TCP listener + NFS RPC dispatcher |
| `crates/kiseki-gateway/src/nfs_xdr.rs` | New: XDR encode/decode for NFS3 ops |
| `crates/kiseki-server/src/config.rs` | Add `nfs_addr: SocketAddr` (default `:2049`) |
| `crates/kiseki-server/src/runtime.rs` | Spawn NFS TCP server |
| `docker-compose.yml` | Map port 2049 |

### NFS ops (MVP)

| NFS3 Procedure | Maps to |
|----------------|---------|
| NULL | ping (no-op) |
| GETATTR | composition metadata |
| LOOKUP | namespace + path → filehandle |
| READ | `NfsGateway::read` |
| WRITE | `NfsGateway::write` |
| READDIR | list compositions in namespace |
| CREATE | `GatewayOps::write` (new file) |

### E2E tests

| File | Tests |
|------|-------|
| `tests/e2e/test_nfs_gateway.py` | Mount + write + read + ls (via subprocess `mount` or NFS client lib) |

**Risk mitigation**: If NFS wire format proves too complex, fall back
to a gRPC-based "NFS-like" service with the same semantics, and
add real NFS wire format later. The domain logic (NfsGateway) is
already tested.

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
