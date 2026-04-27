# Protocol Compliance Catalog

**Status**: Architect phase, rev 2 (addresses ADV-023-2/3/4/6/7/8).
Living document — every new protocol surface added to kiseki gets
a row.
**Last updated**: 2026-04-27. ADR-023 (rev 2) declares the
discipline; this file is its index.

## Purpose

Kiseki implements multiple wire protocols (NFS family, S3, FUSE),
plus internal cluster protocols (gRPC, Raft RPC), each with their
own underlying transport stack. The diamond workflow asks "every
invariant has an enforcement point" — for **protocol invariants**,
the enforcement point is a Layer 1 reference decoder + per-spec-
section unit tests, NOT the BDD `@integration` tag.

Two production bugs (NFSv4 NULL ping, EXCHANGE_ID flags) were
shipped despite passing `nfs4-rfc7862.feature`'s 19 unit tests
because those tests asserted "the response decodes" rather than
"the response decodes per spec section X.Y.Z". Layer 1 work plugs
that gap.

## Prior art

- **ADR-013** — POSIX semantics scope (filesystem expectations the
  FUSE backend must satisfy).
- **ADR-014** — S3 API scope (which S3 ops we expose).
- **ADR-023** (rev 2) — Protocol RFC Compliance Scope and Test
  Discipline. Original (rev 1) defined NFS/S3 implementation scope;
  rev 2 (2026-04-27) folds in the Layer 1 testing discipline that
  was briefly drafted as ADR-039 before being merged back. The
  implementation-scope tables migrated from ADR-023 rev 1 into
  this catalog.

For each spec below:

- **Owner crate** — where the implementation lives.
- **Reference decoder** — pure-function decoder we test the
  implementation against. Lives in
  `crates/<crate>/src/rfc/<rfc>.rs` or
  `crates/<crate>/tests/rfc_<rfc>.rs`.
- **Coverage** — Layer-1 compliance status (see legend).
- **Critical-path** — Y if a kiseki primary use-case (NFS mount,
  S3 PUT/GET, FUSE mount) is blocked when this spec is wrong.

### Coverage legend

| Tag | Meaning |
|---|---|
| ❌ | No reference decoder; only happy-path response-shape tests. (Today's status for almost everything.) |
| 🟡 | Reference decoder exists, partial section coverage. |
| ✅ | Reference decoder + every spec section has at least one assertion. Negative tests for every error code the spec defines. |
| ⛔ | Explicitly rejected — spec considered and chosen out of scope (with ADR reference). |

## Catalog

### Foundation

| Spec | Owner | Decoder | Coverage | Critical |
|---|---|---|---|---|
| **RFC 4506** — XDR external data representation | `kiseki-gateway` (`nfs_xdr.rs`) | `crates/kiseki-gateway/tests/rfc4506.rs` | ✅ — Group I 2026-04-27: strict bool + opaque pad | Y — every NFS XDR field depends on it |
| **RFC 5531** — ONC RPC v2 (call/reply framing, AUTH discriminant) | `kiseki-gateway` | `crates/kiseki-gateway/tests/rfc5531.rs` | ✅ — Group I 2026-04-27: §8.2 400-byte cap on opaque_auth body | Y — wraps every NFSv2/v3/v4 call |
| **RFC 1057** — ONC RPC v1 AUTH flavors (AUTH_NONE, AUTH_SYS) | `kiseki-gateway` (`nfs_auth.rs`) | `crates/kiseki-gateway/tests/rfc1057.rs` | ✅ — Group I 2026-04-27: typed `AuthSysParams::decode`, `OpaqueAuth::decode_strict` | Y — current AUTH_SYS path |
| **RFC 2203** — RPCSEC_GSS protocol (Kerberos for NFS) | `kiseki-gateway` (`nfs_auth.rs` future) | `crates/kiseki-gateway/tests/rpcsec_gss.rs` | ❌ — not implemented today (canonical reject path tested) | N (until enterprise tenants need Kerberos) |
| **RFC 5403** — RPCSEC_GSS Version 2 | `kiseki-gateway` (`nfs_auth.rs` future) | `crates/kiseki-gateway/tests/rpcsec_gss.rs` | ❌ — not implemented today | N |
| **RFC 7204** — RPCSEC_GSS contextual definitions | `kiseki-gateway` (`nfs_auth.rs` future) | (folded into rpcsec_gss.rs) | ❌ — not implemented today | N |

### NFS data path

| Spec | Owner | Decoder | Coverage | Critical |
|---|---|---|---|---|
| **RFC 1813** — NFSv3 protocol (procedure-based) | `kiseki-gateway` (`nfs3_server.rs`) | `crates/kiseki-gateway/tests/rfc1813.rs` | ❌ — happy-path only | Y for NFSv3 mounts |
| **RFC 7530** — NFSv4.0 (substrate for 4.1/4.2) | `kiseki-gateway` (`nfs4_server.rs`) | `crates/kiseki-gateway/tests/rfc7530.rs` | ✅ — Group II 2026-04-27: minor=0 → MINOR_VERS_MISMATCH | N — kiseki advertises 4.1+, but a 4.0-only client probe must fall back cleanly |
| **RFC 8881** — NFSv4.1 (sessions, EXCHANGE_ID, pNFS hooks). **Obsoletes RFC 5661.** Companion XDR: RFC 5662 + applicable errata. | `kiseki-gateway` (`nfs4_server.rs`) | `crates/kiseki-gateway/tests/rfc8881.rs` | ✅ — Group II 2026-04-27: NOFILEHANDLE vs BADHANDLE; OP_ILLEGAL vs NOTSUPP; BADXDR on truncation; minor-vers validation; bitmap word0 = TYPE\|SIZE | Y — the protocol Linux mount.nfs4 uses |
| **RFC 7862** — NFSv4.2 (extends 5661/8881: ALLOCATE, DEALLOCATE, COPY, READ_PLUS, IO_ADVISE). Companion XDR: RFC 7863. | `kiseki-gateway` (`nfs4_server.rs`) | `crates/kiseki-gateway/tests/rfc7862.rs` | ✅ — Group II 2026-04-27: SEEK→UNION_NOTSUPP; LAYOUTERROR→BADIOMODE; v4.2 op-table coverage | Y for NFSv4.2 mounts |
| **RFC 8435** — pNFS Flexible Files Layout | `kiseki-gateway` (`pnfs.rs`, `nfs4_server.rs`) | `crates/kiseki-gateway/tests/rfc8435.rs` | ✅ — Group III 2026-04-27: `ffl_flags` advertises `FF_FLAGS_NO_LAYOUTCOMMIT` (tightly_coupled per ADR-038 §D3) | Y for pNFS perf |
| **RFC 5663** — pNFS Block Layout | n/a | n/a | ⛔ Rejected (ADR-038 §D1) | N |
| **RFC 8154** — pNFS SCSI Layout | n/a | n/a | ⛔ Rejected (ADR-038 §D1) | N |
| **RFC 5665** — Universal Address Format (`netaddr4`, `uaddr`) | `kiseki-gateway` (`pnfs.rs::host_port_to_uaddr`) | `crates/kiseki-gateway/tests/rfc5665.rs` | ✅ — Group III 2026-04-27: bracketed IPv6 form `[ipv6]:port` parsed correctly; tcp/tcp6 netid pinned | Y for pNFS GETDEVICEINFO |
| **RFC 9289** — NFS-over-TLS (`xprtsec=mtls` handshake, keep-alives) | `kiseki-gateway` (`nfs_server.rs`, `pnfs_ds_server.rs`) | `crates/kiseki-gateway/tests/rfc9289.rs` | ❌ — Phase 15a default, no compliance tests | Y for production NFS |

### S3 stack

| Spec | Owner | Decoder | Coverage | Critical |
|---|---|---|---|---|
| **RFC 9110** — HTTP semantics (methods, headers, status codes, ETag §8.8.3, Range §14, conditional requests §13) | `kiseki-gateway` (`s3_server.rs`) | `crates/kiseki-gateway/tests/rfc9110.rs` | ❌ | Y for S3 PUT/GET/HEAD/conditional ops |
| **RFC 9111** — HTTP caching (Cache-Control on responses) | `kiseki-gateway` | (folded into 9110) | ❌ | N — server-side; caches are tenant's concern |
| **RFC 9112** — HTTP/1.1 syntax (chunked encoding, header line folding) | `kiseki-gateway` | (folded into 9110) | ❌ | Y — chunked uploads |
| **RFC 3986** — URI generic syntax (percent-encoding) | `kiseki-gateway` (`s3_server.rs::path` parsing) | `crates/kiseki-gateway/tests/rfc3986.rs` | ❌ | Y — S3 keys with arbitrary bytes need correct encoding in path AND in SigV4 canonical request |
| **RFC 6838** — Media Type Specifications | `kiseki-gateway` | (folded into 9110) | ❌ | N — Content-Type is opaque to us; just round-trip it |
| **RFC 7578** — multipart/form-data (browser-based POST) | not implemented today | `crates/kiseki-gateway/tests/rfc7578.rs` (skeleton — flag if implementation lands) | ❌ | N for v1 of the perf cluster |
| **RFC 8446** — TLS 1.3 (HTTPS for S3 + NFS-over-TLS) | `kiseki-transport` (delegates to rustls) | `crates/kiseki-transport/tests/rfc8446_contract.rs` | ❌ — we trust rustls, but pin our cipher-suite + ALPN choices | Y |
| **AWS SigV4** — request signing (no IETF RFC; AWS published spec with official test vectors) | `kiseki-gateway` (`s3_auth.rs`) | `crates/kiseki-gateway/tests/aws_sigv4.rs` | ❌ — happy-path only; AWS publishes test vectors we should run | Y for any non-anonymous S3 |
| **AWS S3 REST API** — bucket/object semantics, error codes, XML body shapes | `kiseki-gateway` (`s3_server.rs`) | `crates/kiseki-gateway/tests/aws_s3.rs` | ❌ | Y |

### FUSE / native client

| Spec | Owner | Decoder | Coverage | Critical |
|---|---|---|---|---|
| **POSIX.1-2024 (IEEE Std 1003.1-2024)** — file-system semantics (errno, stat fields, readdir, rename atomicity). Supersedes POSIX.1-2017. ADR-013 is the Kiseki-side scope. | `kiseki-client` (`fuse_fs.rs`) | `crates/kiseki-client/tests/posix_semantics.rs` | ❌ | Y — workloads break silently if our errno mapping is wrong |
| **Linux FUSE protocol** (kernel `Documentation/filesystems/fuse.rst`) | `kiseki-client` (`fuse_daemon.rs`) | `crates/kiseki-client/tests/fuse_linux.rs` | ❌ — fuser library handles wire; we declare INIT capabilities | Y for native FUSE perf |
| **macOS FUSE / osxfuse** (different op codes from Linux FUSE) | `kiseki-client` (`fuse_*.rs`) | `crates/kiseki-client/tests/fuse_macos.rs` | ❌ — gated behind macOS @slow | N for primary GCP perf path |

### Internal protocols

These are not externally consumed but carry production traffic and
have the same bug-shape risk as external wire formats (length
prefixes, version negotiation, error mappings). Listed for
completeness; Layer-1 work here is structurally simpler since we
control both endpoints.

| Spec | Owner | Decoder | Coverage | Critical |
|---|---|---|---|---|
| **gRPC + Protobuf** (gRPC over HTTP/2, schemas in `specs/architecture/proto/kiseki/v1/*.proto`) | `kiseki-proto` (build-script generated) | (schema enforcement at compile time via `tonic`/`prost`) | 🟡 — schema is enforced; semantic validation (e.g. all required fields populated, status code mapping) not pinned | Y — every cross-context call rides this |
| **openraft / Raft RPC** (TCP framing for AppendEntries / Vote / InstallSnapshot) | `kiseki-raft` (`tcp_transport.rs`) | `crates/kiseki-raft/tests/raft_wire.rs` | ❌ — custom framing, no decoder tests | Y — Raft consensus is the consistency core |
| **FIPS 140-2/3 cryptographic primitives** (AES-256-GCM, HKDF-SHA256, HMAC-SHA256 via `aws-lc-rs`) | `kiseki-crypto` | aws-lc-rs upstream FIPS-validated; pin our usage parameters | ✅ at primitive level (FIPS module certified); 🟡 at usage level (our key-derivation invariants need section tests) | Y |

## Layer 1 contract — per spec, what "✅" requires

For a row to be marked ✅:

1. **Reference decoder** — a pure-function module under
   `crates/<crate>/src/rfc/<rfc>.rs` or
   `crates/<crate>/tests/rfc_<rfc>.rs`. Decoder follows the RFC's
   wire format byte-for-byte, named for the RFC types, with
   section-number doc comments.
2. **Section coverage** — each spec section that defines a wire
   structure has at least one positive test (decoder accepts a
   valid example) and at least one negative test (decoder rejects
   a malformed example with the spec's error).
3. **Round-trip** — when the spec defines an encoder shape,
   `encode → decode → encode` is identity.
4. **Cross-implementation seed** — at least one captured wire
   sample from a known-good independent implementation seeds the
   tests. Provenance and storage policy: see ADR-023 §D2.3.

## Update protocol

Adding a new protocol:

1. Add a row to the catalog with status ❌.
2. Open ADR if the protocol introduces a new bounded context (e.g.
   ADR-038 for pNFS).
3. Build Layer 1 (decoder + section tests) BEFORE writing
   `@integration` BDD scenarios that claim spec compliance.
4. When ✅, the BDD `@integration` tier may rely on the protocol
   without re-asserting wire-format details.

## Cross-reference

- [ADR-023 (rev 2) — Protocol RFC Compliance Scope and Test Discipline](adr/023-protocol-rfc-compliance.md) — folds the Layer 1 discipline previously drafted as ADR-039
- [ADR-013 — POSIX semantics scope](adr/013-posix-semantics-scope.md)
- [ADR-014 — S3 API scope](adr/014-s3-api-scope.md)
- [ADR-038 — pNFS layout + DS subprotocol](adr/038-pnfs-layout-and-ds-subprotocol.md)
- [`nfs4-rfc7862.feature`](../features/nfs4-rfc7862.feature) — depends on RFC 8881 + RFC 7862 ✅
- [`nfs3-rfc1813.feature`](../features/nfs3-rfc1813.feature) — depends on RFC 1813 ✅
- [`pnfs-rfc8435.feature`](../features/pnfs-rfc8435.feature) — depends on RFC 8881 + RFC 8435 + RFC 5665 ✅
- [`s3-api.feature`](../features/s3-api.feature) — depends on RFC 9110 + RFC 3986 + AWS SigV4 + AWS S3 REST ✅
