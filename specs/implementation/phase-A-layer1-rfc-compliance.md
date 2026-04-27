# Phase A — Layer 1 RFC compliance implementation plan

**Status**: ACCEPTED — execution begins 2026-04-27.
**Date**: 2026-04-27
**Predecessor**: Phase 15 (pNFS) — paused mid-e2e when two NFSv4
wire-protocol bugs (NULL ping, EXCHANGE_ID flags) surfaced and
exposed a fidelity gap (ADR-023 rev 2).
**Authority**:
- [ADR-023 rev 2 — Protocol RFC Compliance Scope and Test Discipline](../architecture/adr/023-protocol-rfc-compliance.md)
- [`specs/architecture/protocol-compliance.md`](../architecture/protocol-compliance.md) — the live catalog (status legend ❌ / 🟡 / ✅ / ⛔)

## Why this plan exists

ADR-023 rev 2 mandates Layer 1 RFC compliance: per-RFC reference
decoder + per-spec-section unit tests, with the catalog as the
living index. This plan converts that mandate into discrete work
units and tracks their progress.

The two bugs that motivated rev 2 (`5f6fece`, `7b1b4f6`) cleared
`cargo test`, `cargo clippy`, and BDD review. They blocked a real
Linux client at mount time. Without Layer 1, more such bugs are
guaranteed to be lurking in the rest of the wire surface.

## Two-stage structure

The work splits naturally along TDD lines:

### Stage 1 — Tests in parallel (RED everything)

Every RFC in the catalog gets its own `tests/rfc<N>.rs` file
written in parallel. Each file is independent: a different
developer (or a different agent) can pick up any RFC in any
order. There is no inter-file dependency at the test layer because
every test asserts against bytes our handler emits, not against
other tests.

**Output of Stage 1**: a complete RED fidelity map — for every
spec section we test, we know whether the implementation matches.
The set of failures IS the discovery; we go in not knowing which
of the 18 specs has bugs.

### Stage 2 — Fixes grouped by code path (GREEN sequentially)

Multiple RFC tests collide on the same handler files. Fixing one
file may turn green tests across multiple specs. The fix work is
therefore organized by **owner file**, not by RFC. Within a file,
all the RFCs that touch it are addressed together.

**Output of Stage 2**: every catalog row goes ✅, in the priority
order of ADR-023 §D3.

## Stage 1 — write all tests in parallel

Each row below produces one new file: `crates/<owner>/tests/rfc_<N>.rs`
(or `aws_<protocol>.rs` / `posix_*.rs` / `fuse_*.rs` for the non-RFC
specs). Test contents per ADR-023 §D2.2:

- One positive test per spec section that defines a wire structure.
- One negative test per `_*ERR_*` / `_BADXDR` / `INVALID` error code.
- Round-trip test where the spec defines an encoder shape.
- One cross-implementation seed (RFC text example, public test
  vector, or `tests/wire-samples/<rfc>/` LFS pointer).

Tests start RED. That's the point. Each row's "Acceptance" column
says when the test file is considered done (the file is complete,
not necessarily that all assertions pass).

| # | Spec | Owner crate | Test file | Acceptance — done when |
|---|---|---|---|---|
| T-01 | RFC 4506 (XDR) | `kiseki-gateway` | `tests/rfc4506.rs` | every type from RFC 4506 §3-4 has a positive + negative test |
| T-02 | RFC 5531 (ONC RPC v2) | `kiseki-gateway` | `tests/rfc5531.rs` | call + reply headers; AUTH_NONE/AUTH_SYS discriminant; PROC_UNAVAIL + PROG_MISMATCH negatives |
| T-03 | RFC 1057 (AUTH flavors) | `kiseki-gateway` | `tests/rfc1057.rs` | AUTH_NONE + AUTH_SYS body shapes per §7-9; RPCSEC_GSS rejected as ❌ not-implemented |
| T-04 | RFC 2203 + RFC 5403 + RFC 7204 (RPCSEC_GSS) | `kiseki-gateway` | `tests/rpcsec_gss.rs` | thin file documenting "not-implemented" + the canonical reject path |
| T-05 | RFC 1813 (NFSv3) | `kiseki-gateway` | `tests/rfc1813.rs` | every implemented procedure (per ADR-023 §D1 / catalog) gets a positive test; NFS3ERR_* negatives |
| T-06 | RFC 7530 (NFSv4.0 fallback) | `kiseki-gateway` | `tests/rfc7530.rs` | minor_version=0 client probe → graceful fallback or PROG_MISMATCH; one COMPOUND happy path |
| T-07 | RFC 8881 (NFSv4.1, obsoletes 5661) | `kiseki-gateway` | `tests/rfc8881.rs` | every COMPOUND op kiseki implements per §18 has positive; every NFS4ERR_* the spec defines has at least one wire-side negative |
| T-08 | RFC 7862 (NFSv4.2 extensions) | `kiseki-gateway` | `tests/rfc7862.rs` | ALLOCATE / DEALLOCATE / COPY / READ_PLUS / IO_ADVISE positives; v4.2-specific NFS4ERR_* negatives |
| T-09 | RFC 8435 (pNFS Flexible Files) | `kiseki-gateway` | `tests/rfc8435.rs` | ff_layout4 §5.1, ff_device_addr4 §5.2, fh4 wire layout (76 bytes per ADR-038), GETDEVICEINFO + LAYOUTGET round-trip |
| T-10 | RFC 5665 (Universal Address) | `kiseki-gateway` | `tests/rfc5665.rs` | every example in §5.2.3 (h.h.h.h.p.p), IPv6 form (§5.2.5), tcp/tcp6 netid; truncation negatives |
| T-11 | RFC 9289 (NFS-over-TLS) | `kiseki-gateway` | `tests/rfc9289.rs` | tls_handshake AUTH-flavor wrapping; xprtsec=mtls keep-alive cadence; rejection cases |
| T-12 | RFC 9110/9111/9112 (HTTP/1.1) | `kiseki-gateway` | `tests/rfc9110.rs` | every status code we emit; ETag §8.8.3; Range §14; conditional headers §13; chunked encoding |
| T-13 | RFC 3986 (URI) | `kiseki-gateway` | `tests/rfc3986.rs` | percent-encoding round-trip; reserved/unreserved sets; key-with-binary-bytes negative case |
| T-14 | RFC 6838 (media types) | `kiseki-gateway` | `tests/rfc6838.rs` | Content-Type round-trip — opaque to us; assert no mutation |
| T-15 | RFC 7578 (multipart) | `kiseki-gateway` | `tests/rfc7578.rs` | skeleton — assert "not implemented" path; flag if implementation ever lands |
| T-16 | RFC 8446 (TLS 1.3) | `kiseki-transport` | `tests/rfc8446_contract.rs` | trust rustls; pin our cipher-suite + ALPN choices; client-cert chain validation against Cluster CA |
| T-17 | AWS SigV4 | `kiseki-gateway` | `tests/aws_sigv4.rs` | run AWS official SigV4 test vectors verbatim; assert canonical-request derivation matches |
| T-18 | AWS S3 REST API | `kiseki-gateway` | `tests/aws_s3.rs` | every implemented op's XML body shape; common error codes (NoSuchKey, BucketAlreadyExists, AccessDenied) |
| T-19 | POSIX.1-2024 | `kiseki-client` | `tests/posix_semantics.rs` | errno mapping (ENOENT/EISDIR/ENOTDIR/EEXIST/EACCES/EROFS); stat field meanings; readdir cookie monotonicity; rename atomicity |
| T-20 | Linux FUSE protocol | `kiseki-client` | `tests/fuse_linux.rs` | INIT cap-flag declaration matches what we want (FOPEN_DIRECT_IO, KEEP_CACHE, EXPORT_SUPPORT); op-code happy paths; minor-version negotiation |
| T-21 | macOS osxfuse | `kiseki-client` | `tests/fuse_macos.rs` | gated `#[cfg(target_os = "macos")]`; pin known divergent op-codes |
| T-22 | gRPC + Protobuf | `kiseki-proto` | `tests/grpc_contract.rs` | every gRPC service's status-code-to-protobuf-error mapping; reserved-tag invariants on each message |
| T-23 | openraft RPC framing | `kiseki-raft` | `tests/raft_wire.rs` | length-prefix framing; AppendEntries / Vote / InstallSnapshot serialization round-trip |
| T-24 | FIPS crypto usage | `kiseki-crypto` | `tests/fips_usage.rs` | nonce uniqueness invariant; HKDF info-string domain separation; key-purpose binding |

**24 test files. All independent. All in parallel.**

### Stage 1 done-criterion

A bash one-liner that lists every file and asserts they exist:

```bash
for f in rfc4506 rfc5531 rfc1057 rpcsec_gss rfc1813 rfc7530 rfc8881 \
         rfc7862 rfc8435 rfc5665 rfc9289 rfc9110 rfc3986 rfc6838  \
         rfc7578 aws_sigv4 aws_s3; do
  test -f crates/kiseki-gateway/tests/${f}.rs || echo "MISSING gateway/${f}.rs"
done
test -f crates/kiseki-transport/tests/rfc8446_contract.rs || echo "MISSING"
test -f crates/kiseki-client/tests/posix_semantics.rs    || echo "MISSING"
test -f crates/kiseki-client/tests/fuse_linux.rs         || echo "MISSING"
test -f crates/kiseki-client/tests/fuse_macos.rs         || echo "MISSING"
test -f crates/kiseki-proto/tests/grpc_contract.rs       || echo "MISSING"
test -f crates/kiseki-raft/tests/raft_wire.rs            || echo "MISSING"
test -f crates/kiseki-crypto/tests/fips_usage.rs         || echo "MISSING"
```

When the loop produces zero output, Stage 1 is done.

`cargo test --workspace` is expected to **fail** at this point —
that's the point. The set of failures is the fidelity map that
drives Stage 2.

## Stage 2 — fixes grouped by owner file

Fix work is sequenced by the file each fix touches. Multiple
RFC tests typically share an owner file, so one fix turns several
tests green. The groups below are listed in priority order per
ADR-023 §D3.

The catalog row for each affected RFC moves from ❌ → 🟡 (positive
section coverage only) → ✅ (positive + negative + round-trip +
seed) as the group's tests turn green.

### Group I — Foundation (`kiseki-gateway/src/nfs_xdr.rs` + `nfs_auth.rs`)

Every NFS call rides this code. Fixing here unblocks every NFS
RFC test.

- **Resolves**: T-01 RFC 4506, T-02 RFC 5531, T-03 RFC 1057, T-04 RPCSEC_GSS
- **Files touched**:
  - `crates/kiseki-gateway/src/nfs_xdr.rs` — XDR codec helpers,
    RPC accept/reply encoding
  - `crates/kiseki-gateway/src/nfs_auth.rs` — AUTH flavor parsing
- **Likely fixes (predicted from spec re-read; actual from test failures)**:
  - XDR opaque length-prefix padding (RFC 4506 §4.10)
  - RPC reply rejection cases (PROG_UNAVAIL vs PROG_MISMATCH —
    NFSv4 needs PROG_MISMATCH with version-low/high pair)
  - AUTH_SYS gid array length validation
- **Exit criterion**: T-01–T-04 all GREEN; catalog rows for
  RFC 4506 / 5531 / 1057 → ✅. RPCSEC_GSS row stays ❌
  (not-implemented), with the test asserting the correct reject path.

### Group II — NFSv4 family (`kiseki-gateway/src/nfs4_server.rs`)

The currently-blocking critical path. Single file, three RFCs.

- **Resolves**: T-06 RFC 7530, T-07 RFC 8881, T-08 RFC 7862
- **File touched**:
  - `crates/kiseki-gateway/src/nfs4_server.rs` (~2000 lines —
    where the recent NULL + EXCHANGE_ID fixes landed)
- **Already known fixes**:
  - NULL ping → ACCEPT_OK (`5f6fece`) ✓
  - EXCHANGE_ID flags → USE_PNFS_MDS | CONFIRMED_R (`7b1b4f6`) ✓
- **Likely additional fixes (test-driven, expected based on previous bug pattern)**:
  - CREATE_SESSION reply bitmap encoding (next thing after EXCHANGE_ID)
  - SEQUENCE op slot-id wraparound semantics
  - PUTROOTFH file-handle format vs RFC 8881 §15.4
  - GETATTR bitmap encoding for the attrs we support
  - Op-table coverage: every op kiseki claims to support is in
    the dispatcher (cross-check with catalog row)
- **Exit criterion**: T-06–T-08 all GREEN; catalog rows for
  RFC 7530 / 8881 / 7862 → ✅; the e2e mount paused 2026-04-27
  succeeds against this same code (without further mount-side
  fixes).

### Group III — pNFS (`kiseki-gateway/src/pnfs.rs` + `pnfs_ds_server.rs` + `nfs4_server.rs::op_layoutget_ff`)

Only meaningful after Group II — the pNFS layout body is wrapped
by NFSv4.1 COMPOUND.

- **Resolves**: T-09 RFC 8435, T-10 RFC 5665
- **Files touched**:
  - `crates/kiseki-gateway/src/pnfs.rs` — `MdsLayoutManager`,
    `host_port_to_uaddr`, fh4 codec
  - `crates/kiseki-gateway/src/pnfs_ds_server.rs` — DS dispatcher
  - `crates/kiseki-gateway/src/nfs4_server.rs` — `op_layoutget_ff`,
    `op_getdeviceinfo`
- **Likely fixes**:
  - `ff_layout4` body field ordering (Phase 15b implementation
    needs RFC 8435 §5.1 verification)
  - `host_port_to_uaddr` IPv6 form (currently only IPv4 tested)
  - `ff_device_addr4` versions array encoding
- **Exit criterion**: T-09 + T-10 GREEN; catalog rows → ✅; the
  e2e pNFS mount succeeds with `/proc/self/mountstats` showing
  non-zero per-DS counters.

### Group IV — NFS transport (`kiseki-gateway/src/nfs_server.rs`)

NFS-over-TLS handshake correctness.

- **Resolves**: T-11 RFC 9289
- **File touched**: `crates/kiseki-gateway/src/nfs_server.rs`
  (TLS-wrap path added in Phase 15a)
- **Likely fixes**: keep-alive cadence handling, TLS session
  resumption.
- **Exit criterion**: T-11 GREEN; catalog row → ✅.

### Group V — NFSv3 (`kiseki-gateway/src/nfs3_server.rs`)

Independent of v4 family. Can run in parallel with Group VI.

- **Resolves**: T-05 RFC 1813
- **File touched**: `crates/kiseki-gateway/src/nfs3_server.rs`
- **Exit criterion**: T-05 GREEN; catalog row → ✅.

### Group VI — S3 stack (`kiseki-gateway/src/s3_server.rs` + `s3_auth.rs`)

Parallel with Group V (different owner files, different layer).

- **Resolves**: T-12 RFC 9110/9111/9112, T-13 RFC 3986,
  T-14 RFC 6838, T-15 RFC 7578, T-17 AWS SigV4, T-18 AWS S3 REST
- **Files touched**:
  - `crates/kiseki-gateway/src/s3_server.rs` — REST handlers
  - `crates/kiseki-gateway/src/s3_auth.rs` — SigV4 verifier
- **Likely fixes**:
  - SigV4 canonical-URI percent-encoding (RFC 3986 vs the
    AWS-specific double-encoding rule for the path component)
  - Range-header partial GET semantics (RFC 9110 §14)
  - ETag quoting (W/"…" vs "…")
  - XML error body shapes (S3 official error codes)
- **Exit criterion**: T-12–T-15, T-17, T-18 all GREEN; catalog rows
  for HTTP family / URI / SigV4 / S3 REST → ✅. RFC 6838 (media
  types) stays 🟡 — Content-Type is opaque to us. RFC 7578 stays
  ❌ if multipart isn't implemented.

### Group VII — TLS contract (`kiseki-transport/src/`)

Parallel with everything. Tiny surface.

- **Resolves**: T-16 RFC 8446
- **File touched**: `crates/kiseki-transport/src/tcp_tls.rs`
- **Exit criterion**: T-16 GREEN; cipher-suite + ALPN pinned;
  catalog row → 🟡 (we trust rustls for the bulk of compliance).

### Group VIII — FUSE / native client (`kiseki-client/src/`)

Parallel with everything.

- **Resolves**: T-19 POSIX.1-2024, T-20 Linux FUSE,
  T-21 macOS osxfuse
- **Files touched**:
  - `crates/kiseki-client/src/fuse_fs.rs` — POSIX semantic surface
  - `crates/kiseki-client/src/fuse_daemon.rs` — INIT cap flags
- **Likely fixes**:
  - Errno mapping holes (test-driven discovery)
  - INIT cap flags audit — what we declare vs what we want
- **Exit criterion**: T-19–T-21 GREEN; catalog rows → ✅
  (POSIX, Linux FUSE) / 🟡 (macOS, gated `@slow`).

### Group IX — Internal protocols (parallel; last by ADR-023 §D3)

These don't gate any external client. Cleanup tail.

- **Resolves**: T-22 gRPC, T-23 Raft RPC, T-24 FIPS usage
- **Files touched**:
  - `crates/kiseki-proto/build.rs` + generated code
  - `crates/kiseki-raft/src/tcp_transport.rs`
  - `crates/kiseki-crypto/src/*.rs`
- **Exit criterion**: T-22–T-24 GREEN; catalog rows for internal
  protocols → ✅ (FIPS usage), 🟡 (gRPC, Raft RPC — semantic
  validation; full ✅ requires more cross-implementation seeds
  than we'll generate).

## Parallelization summary

| Stage | Parallelizable across | Sequential within |
|---|---|---|
| **Stage 1** (24 test files) | All 24 files independent — write any/all in parallel | n/a |
| **Stage 2** (9 fix groups) | Groups I, V, VI, VII, VIII, IX may run concurrently (different owner files); II → III is sequential (III's tests need II's fixes); IV depends on I+II | Within a group: fix one file at a time; one developer handles a whole group end-to-end |

The hot path for unblocking Phase 15 e2e is **Group I → Group II → Group III** (sequential). Everything else can ride alongside.

## Tracking

A simple Markdown table at the top of this file (below) is the
progress log. Update it as each row goes ❌ → 🟡 → ✅. The catalog
([`protocol-compliance.md`](../architecture/protocol-compliance.md))
is updated in the same commit as the test/fix landings.

### Stage 1 progress

| # | Spec | File | Written? | Tests RED? |
|---|---|---|---|---|
| T-01 | RFC 4506 | `crates/kiseki-gateway/tests/rfc4506.rs` | ✅ | 0 of 18 RED — Group I closed 2026-04-27 |
| T-02 | RFC 5531 | `crates/kiseki-gateway/tests/rfc5531.rs` | ✅ | 0 of 8 RED — Group I closed 2026-04-27 |
| T-03 | RFC 1057 | `crates/kiseki-gateway/tests/rfc1057.rs` | ✅ | 0 of 15 RED — Group I closed 2026-04-27 |
| T-04 | RPCSEC_GSS family | `crates/kiseki-gateway/tests/rpcsec_gss.rs` | ✅ | 0 of 3 RED (canonical reject path documented) |
| T-05 | RFC 1813 | `crates/kiseki-gateway/tests/rfc1813.rs` | ✅ | 0 of 12 RED — Group V closed 2026-04-27 |
| T-06 | RFC 7530 | `crates/kiseki-gateway/tests/rfc7530.rs` | ✅ | 0 of 7 RED — Group II closed 2026-04-27 |
| T-07 | RFC 8881 | `crates/kiseki-gateway/tests/rfc8881.rs` | ✅ | 0 of 28 RED — Group II closed 2026-04-27 |
| T-08 | RFC 7862 | `crates/kiseki-gateway/tests/rfc7862.rs` | ✅ | 0 of 12 RED — Group II closed 2026-04-27 |
| T-09 | RFC 8435 | `crates/kiseki-gateway/tests/rfc8435.rs` | ✅ | 0 of 20 RED — Group III closed 2026-04-27 |
| T-10 | RFC 5665 | `crates/kiseki-gateway/tests/rfc5665.rs` | ✅ | 0 of 14 RED — Group III closed 2026-04-27 |
| T-11 | RFC 9289 | `crates/kiseki-gateway/tests/rfc9289.rs` | ✅ | 0 of 11 RED — Group IV closed 2026-04-27 |
| T-12 | RFC 9110/9111/9112 | `crates/kiseki-gateway/tests/rfc9110.rs` | ✅ | 0 of 19 RED — Group VI closed 2026-04-27 |
| T-13 | RFC 3986 | `crates/kiseki-gateway/tests/rfc3986.rs` | ✅ | 0 of 11 RED |
| T-14 | RFC 6838 | `crates/kiseki-gateway/tests/rfc6838.rs` | ✅ | 0 of 5 RED — Group VI closed 2026-04-27 |
| T-15 | RFC 7578 | `crates/kiseki-gateway/tests/rfc7578.rs` | ✅ | 0 of 4 (skeleton — multipart not implemented) |
| T-16 | RFC 8446 | `crates/kiseki-transport/tests/rfc8446_contract.rs` | ✅ | 0 of 10 RED — Group VII closed 2026-04-27. CRITICAL finding from Stage 1 (mTLS bypass) was a FALSE POSITIVE: original test panicked on `connect().Ok` but TLS 1.3 alerts can race with handshake completion; hardened test verifies authoritative bytes-cross-channel boundary. Direct verifier-layer test added as regression guard. TLS 1.3-only cipher-suite restriction landed in production. |
| T-17 | AWS SigV4 | `crates/kiseki-gateway/tests/aws_sigv4.rs` | ✅ | 0 of 9 RED — Group VI closed 2026-04-27 (fixture corrected; canonical-request matches AWS-published) |
| T-18 | AWS S3 REST | `crates/kiseki-gateway/tests/aws_s3.rs` | ✅ | 0 of 11 RED — Group VI closed 2026-04-27 (XML error responses via s3_error_response) |
| T-19 | POSIX.1-2024 | `crates/kiseki-client/tests/posix_semantics.rs` | ✅ | 0 of 22 RED — Group VIII closed 2026-04-27 (EROFS mapping) |
| T-20 | Linux FUSE | `crates/kiseki-client/tests/fuse_linux.rs` | ✅ | 0 of 15 RED |
| T-21 | macOS osxfuse | `crates/kiseki-client/tests/fuse_macos.rs` | ✅ | 0 of 5 RED (cfg-gated) |
| T-25 | Native client + C FFI ABI (no RFC; representative variant) | `crates/kiseki-client/tests/native_abi.rs` | ✅ | 0 of 4 RED — Group VIII addendum 2026-04-27 (caught by user) |
| T-22 | gRPC | `crates/kiseki-proto/tests/grpc_contract.rs` | ✅ | 0 of 12 RED — Group IX closed 2026-04-27 |
| T-23 | Raft RPC | `crates/kiseki-raft/tests/raft_wire.rs` | ✅ | 0 of 15 RED — Group IX closed 2026-04-27 |
| T-24 | FIPS usage | `crates/kiseki-crypto/tests/fips_usage.rs` | ✅ | 0 of 12 RED — Group IX closed 2026-04-27 |

**Stage 1 totals**: 24 of 24 files written; ~32 RED across the suite. Critical findings:
- **T-16 RFC 8446** — `WebPkiClientVerifier` may accept unrelated CA-signed
  client certs (potential mTLS bypass). Investigate during Group VII.
- **T-07 RFC 8881** — 7 RED (largest single-RFC failure count); NFSv4.1
  fidelity gap is broader than the two known bugs.
- **T-12 RFC 9110** — 6 RED (Range header, conditional headers); S3 GET
  partial-read semantics are likely not honored.

### Stage 2 progress

| Group | Files | Status |
|---|---|---|
| I — Foundation | `nfs_xdr.rs`, `nfs_auth.rs` | ✅ — strict bool/opaque pad; `OpaqueAuth` w/ §8.2 400-byte cap; `AuthSysParams::decode` enforcing machinename≤255 + gids≤16 (2026-04-27) |
| II — NFSv4 family | `nfs4_server.rs` | ✅ — minor-vers validation; OP_ILLEGAL/NOTSUPP/BADXDR distinctions; NOFILEHANDLE for missing current_fh; getattr bitmap fix; SEEK + LAYOUTERROR stubs (2026-04-27) |
| III — pNFS | `pnfs.rs`, `pnfs_ds_server.rs`, `nfs4_server.rs::op_layoutget_ff` | ✅ — `host_port_to_uaddr` bracketed IPv6 (`[::1]:2049` → `::1.8.1`); `ff_ioflags4` advertises `FF_FLAGS_NO_LAYOUTCOMMIT` (2026-04-27) |
| IV — NFS transport | `nfs_server.rs` | ✅ — TCP keep-alive on accepted sockets at RFC 9289 §4.2 60-sec cadence (2026-04-27) |
| V — NFSv3 | `nfs3_server.rs` | ✅ — never-issued handle pre-check returns BADHANDLE before ctx.getattr/ctx.read (2026-04-27) |
| VI — S3 stack | `s3_server.rs`, `s3_auth.rs` | ✅ — Range/conditional headers; XML error bodies; Content-Type round-trip; SigV4 implementation cross-checked vs Python+OpenSSL (test fixture corrected) (2026-04-27) |
| VII — TLS contract | `kiseki-transport/src/config.rs` | ✅ — TLS 1.3-only via cipher-suite filter + `with_protocol_versions(&[TLS13])`; mTLS chain validation verified directly + authoritatively (2026-04-27) |
| VIII — FUSE / native client | `fuse_fs.rs`, `fuse_daemon.rs`, `ffi.rs` | ✅ — EROFS mapping via typed gateway error; native ABI Layer-1 (T-25) added per user request (2026-04-27) |
| IX — Internal | `kiseki-proto`, `kiseki-raft`, `kiseki-crypto` | ✅ — Stage 1 tests already at 0 RED; catalog rows updated to ✅ (2026-04-27) |

## Definition of Done for Phase A

1. ✅ Every catalog row except the explicitly-rejected ones
   (RFC 5663, RFC 8154) and the explicitly-not-implemented ones
   (RFC 2203 / 5403 / 7204, RFC 7578) is at least 🟡 in the
   catalog.
2. ✅ The Phase 15 e2e mount paused 2026-04-27 succeeds without
   further server-side fixes (Group II + III exit gates).
   **Status (2026-04-27 final)**: `mount.nfs4 -o vers=4.1
   kiseki-node1:/default /mnt/pnfs` succeeds end-to-end, rc=0,
   `/mnt/pnfs` is a mountpoint, `ls -la` shows the root
   directory. Closing this DoD item required four staged
   fixes after the initial ADV-PA-9 e2e run:

   - **Phase 15c.1** (commit `d2e3f45`): SECINFO_NO_NAME (op 52),
     BIND_CONN_TO_SESSION (op 41), DESTROY_CLIENTID (op 57)
     dispatcher entries + CB_NULL on program 400122 acceptance.
     Mount error advanced `Operation not supported` →
     `Input/output error`.
   - **Phase 15c.2** (this commit): `op_getattr` rewritten to
     honor the request bitmap per RFC 8881 §5.6 (was: always
     emitted TYPE|SIZE regardless of what the client asked for).
     Now encodes per-attr bodies for SUPPORTED_ATTRS, TYPE,
     FH_EXPIRE_TYPE, CHANGE, SIZE, LINK_SUPPORT, SYMLINK_SUPPORT,
     NAMED_ATTR, FSID, UNIQUE_HANDLES, LEASE_TIME, RDATTR_ERROR,
     FILEHANDLE, FILEID. Mount error advanced
     `Input/output error` → `No such file or directory`.
   - **Phase 15c.2 (pseudo-root + namespace alias)**:
     `PUTROOTFH` returns a synthetic pseudo-root with fileid=1;
     `LOOKUP("default")` from pseudo-root descends into the
     namespace root (different fileid). This makes
     `mount.nfs4 server:/default` resolve correctly without
     triggering the kernel's loop-detection (`mount(2): Too many
     levels of symbolic links`). Mount error advanced
     `Too many levels of symbolic links` → **rc=0**.

   The kernel-mount path (PUTROOTFH → LOOKUP → GETATTR →
   ACCESS → READDIR-shape) is fully functional. Reading specific
   compositions through NFS (`dd /mnt/pnfs/<uuid>`) requires
   composition-by-UUID LOOKUP which is Phase 15c.3 work — out of
   scope for Phase A close.
3. ✅ `cargo test --workspace` passes (verified post Group IV–IX).
4. 🟡 The auditor's gate-2 spec-fidelity check (ADR-023 §D5)
   verifies every `@integration` BDD scenario maps to a 🟡-or-
   better catalog row. (Pending — not run as part of Phase A
   close.)
5. ❌ ADR-023 rev 2 §D4.1 phase B begins (per-RFC opt-in to keep
   `@integration` alone). Pending — Phase A close authoritatively
   gates Phase B; criterion #2 must clear first.

**Phase A is therefore "Layer-1 fidelity complete" but not
"Phase 15 unblocked"** — the wire-format reference tests all pass,
but the kernel-client e2e surfaces a remaining server-side bug in
the NFSv4.1 first-COMPOUND reply path. The fix belongs to a
Phase-15-class follow-up, not to Phase A.

## What this plan deliberately does NOT cover

- **Phase B** (organization-wide `@happy-path` tag rollout) — per
  ADR-023 §D4.1, that's a downstream sweep after Phase A finishes.
- **Phase C** (drop dual-tag scaffold; auditor enforces strictly)
  — last; only after every catalog row reaches ✅.
- **Re-running the e2e** — handled by `tests/e2e/test_pnfs.py`
  once Group III exits.

## Open

- Wire-sample provenance: where do `tests/wire-samples/<rfc>/`
  fixtures actually come from for tests T-09 onwards? RFC 8435
  has tiny example bodies; capturing real Linux client traffic
  requires a working mount (Group II must land first). Plan: use
  RFC examples for T-09 round-trip; capture real traffic AFTER
  Group II to harden T-09's seed.
- T-22 gRPC contract test needs a list of every status-code
  mapping. Currently scattered across `kiseki-proto` and the
  service impls. May need a small collation effort first.

## Source-of-truth artifacts

- [ADR-023 rev 2](../architecture/adr/023-protocol-rfc-compliance.md)
- [Catalog](../architecture/protocol-compliance.md)
- [Adversary findings](../findings/architecture-review.md) — gate 1 cleared on rev 2
