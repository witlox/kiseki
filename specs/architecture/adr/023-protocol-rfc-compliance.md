# ADR-023: Protocol RFC Compliance Scope and Test Discipline

**Status**: Accepted (rev 3 — adds D7 client-side protocol library).
**Date**: 2026-04-20 (rev 1) / **Revised**: 2026-04-27 (rev 2) / 2026-05-01 (rev 3)
**Deciders**: Architect + implementer (rev 1); Architect → adversary
gate 1 → implementer (rev 2).

## Revision history

- **rev 1 (2026-04-20)**: Original. Defined NFSv3 / NFSv4.2 / S3
  implementation scope. Stated "compliance testing approach" as:
  BDD feature files, raw e2e wire tests, real client interop
  (future).
- **rev 2 (2026-04-27)**: Triggered by two production wire-protocol
  bugs (NFSv4 NULL ping rejected, EXCHANGE_ID `eir_flags` wrong)
  that landed despite passing rev-1 BDD compliance tests. Rev 2:
  - Implementation-scope tables migrated to the new
    [`specs/architecture/protocol-compliance.md`](../protocol-compliance.md)
    catalog (the catalog is the living index).
  - Folds in the former draft ADR-039 ("Layer 1 RFC compliance
    test discipline") to strengthen the testing discipline.
  - Adds Layer 1 reference-decoder + per-spec-section unit-test
    requirement; rev 1's "Compliance testing approach" §
    superseded.

## Context

Kiseki exposes external wire protocols (S3 HTTP, NFSv3, NFSv4.2,
pNFS, FUSE) and carries production traffic over internal protocols
(gRPC, openraft RPC, FIPS crypto primitives). ADR-013 (POSIX
semantics) and ADR-014 (S3 API scope) define the functional subsets
but don't reference specific RFC sections or define wire-format
compliance testing.

Rev 1 of this ADR made the scope explicit. Rev 2 caught a fidelity
problem: the rev-1 testing approach was happy-path response-shape
inspection — tests decoded our handler's output and asserted field
presence; they did not validate against the spec. When the
developer's mental model of the spec was wrong (e.g. the
"CONFIRMED" flag was `0x01` instead of `0x80000000`), the test
mirrored the wrong model and confirmed the bug.

Concrete evidence (2026-04-27, Phase 15 e2e):

1. **NULL procedure (RFC 7530/8881 §15.1)**: NFSv4 NULL must
   succeed with an empty ACCEPT_OK reply. Kiseki returned
   PROC_UNAVAIL via the COMPOUND fall-through path. Unit suite
   never exercised proc=0; the `@integration` BDD never noticed
   because no scenario tried mounting from a real client.
2. **EXCHANGE_ID flags (RFC 8881 §18.35.4)**: `eir_flags` MUST
   include at least one of `USE_NON_PNFS | USE_PNFS_MDS |
   USE_PNFS_DS`. Kiseki emitted `0x01` (`SUPP_MOVED_REFER`,
   mislabeled "CONFIRMED" in the source) and the unit test
   `exchange_id_returns_ok_with_client_id` asserted `flags & 0x01
   == 0x01` — confirming the wrong constant.

Both bugs cleared `cargo test`, `cargo clippy`, the BDD harness,
and Phase 15 review. They blocked a real Linux client at
`mount.nfs4` time with EIO. The existing `@integration` claim that
"kiseki implements RFC 7862" was stronger than the actual coverage
warranted. That fidelity gap is what rev 2 addresses.

## Decision

### D1. Implementation scope (rev 1, retained)

The set of NFSv3 procedures, NFSv4.2 COMPOUND operations, and S3
operations kiseki supports lives in the
[`protocol-compliance.md`](../protocol-compliance.md) catalog under
the per-spec rows. The catalog is the live index — it is updated
in the same change-set as any code that adds or removes a wire
operation.

ADR rev 1's tables are now machine-checkable rows in the catalog,
keyed by spec ID, with implementation status, owner crate, and
critical-path Y/N. Adding or removing an operation is a catalog
edit + ADR amendment if the operation introduces a new bounded
context (e.g. ADR-038 for pNFS).

### D2. Test discipline — Layer 1 RFC compliance (rev 2)

Every protocol surface kiseki exposes gets a **Layer 1 reference
decoder + per-spec-section unit tests** before any feature can
claim `@integration` compliance.

#### D2.1 Per-RFC reference decoder

For each spec listed in the catalog, the owning crate hosts a
pure-function module:

```
crates/<crate>/src/rfc/<rfc>.rs       # for runtime use (rare)
crates/<crate>/tests/rfc_<rfc>.rs     # for tests only
```

The decoder follows the RFC's wire format byte-for-byte, names
its types after the spec types (`exchange_id4resok`, `ff_layout4`,
…), and each function has a doc comment citing the spec section
it implements (`/// RFC 8881 §18.35.4`).

#### D2.2 Per-section coverage

For each section that defines a wire structure:

- **Positive test** — decoder accepts a valid example, every
  field-level assertion ties to a spec line.
- **Negative test** — decoder rejects a malformed example with the
  spec's error code (e.g. `NFS4ERR_BADXDR` for short input).

For each error code the spec defines (e.g. `NFS4ERR_*`), at least
one test that triggers it from the wire side, not from internal
state.

#### D2.3 Round-trip and cross-implementation seed

When a section defines an *encoder* (response shape), the test
suite includes:

- **Round-trip** — `encode → decode → encode` is byte-identical.
- **Cross-implementation seed** — at least one captured wire
  sample from a known-good independent implementation seeds the
  tests.

##### D2.3.1 Wire-sample provenance (addresses ADV-023-5)

Source priority for cross-implementation seeds:

1. **Spec-embedded examples** — RFC text, AWS published test
   vectors. Pure text, no chicken-and-egg with our own server.
   Preferred.
2. **Public test suites** — e.g. AWS SigV4 official test vectors,
   Linux kernel `nfs-utils` test fixtures (BSD-3 licensed). Pure
   text, vendored as bytes.
3. **Captured `.pcap`** from a known-good independent
   implementation, AFTER we have a baseline that lets us compare
   against. Used only for full-flow sanity checks.
4. **Hand-crafted from spec** — only for paths no real client
   exercises (rare error codes like `NFS4ERR_RESOURCE`).

##### D2.3.2 Storage policy

- **Text fixtures** (RFC examples, test vectors, hand-crafted
  bytes) live under `tests/wire-samples/<rfc>/` directly in git.
  Each fixture has a sibling `<name>.txt` with provenance: which
  RFC section, which paragraph or test-vector ID, what kernel
  version (for captures), how to reproduce.
- **Binary `.pcap` captures** (when used) go in the same path
  with `.gitattributes` declaring them as Git LFS pointers, AND
  the source file embeds the SHA-256 of the expected blob so a
  missing LFS object fails loudly.
- **Maximum size**: 200 KiB per fixture before LFS is required
  (typical RFC example fits in a few hundred bytes).
- **Reproduction script**: each capture has a sibling shell
  script that re-captures from a documented reference setup
  (kernel version, mount.nfs version, server config). When the
  catalog row goes ✅ this script is a contract: anyone can
  re-run and compare.

### D3. Catalog drives priority

[`protocol-compliance.md`](../protocol-compliance.md) lists every
spec with **owner crate, current coverage tag, critical-path
Y/N**. Layer 1 work proceeds in this order (visual):

```
        ┌───────────────────────────────────┐
Phase A:│  Foundation                       │  unblocks every NFS spec
        │  RFC 4506 (XDR)                   │
        │  RFC 5531 (ONC RPC v2)            │
        │  RFC 1057 (AUTH_NONE / AUTH_SYS)  │
        └─────────────────┬─────────────────┘
                          ▼
        ┌───────────────────────────────────┐
Phase B:│  Critical-path failing today      │  unblocks Phase-15 e2e
        │  RFC 8881 (NFSv4.1)               │
        └─────────────────┬─────────────────┘
                          ▼
        ┌───────────────────────────────────┐
Phase C:│  Critical-path data plane         │
        │  RFC 7862 (NFSv4.2)               │
        │  RFC 8435 (pNFS Flexible Files)   │
        │  RFC 5665 (uaddr)                 │
        │  RFC 9289 (NFS-over-TLS)          │
        └─────────────────┬─────────────────┘
                          ▼
        ┌───────────────────────────────────┐
Phase D:│  NFSv3 path                       │
        │  RFC 1813                         │
        │  RFC 7530 (4.0 fallback)          │
        └─────────────────┬─────────────────┘
                          ▼
        ┌───────────────────────────────────┐
Phase E:│  S3 stack                         │  parallelizable with B-D
        │  RFC 9110/9111/9112 (HTTP)        │
        │  RFC 3986 (URI)                   │
        │  RFC 8446 (TLS 1.3)               │
        │  AWS SigV4                        │
        │  AWS S3 REST API                  │
        └─────────────────┬─────────────────┘
                          ▼
        ┌───────────────────────────────────┐
Phase F:│  FUSE / native client             │  parallelizable with E
        │  POSIX.1-2024                     │
        │  Linux FUSE protocol              │
        │  macOS osxfuse divergence         │
        └─────────────────┬─────────────────┘
                          ▼
        ┌───────────────────────────────────┐
Phase G:│  Internal protocols               │  cleanup tail
        │  gRPC + Protobuf                  │
        │  openraft RPC                     │
        │  FIPS crypto usage                │
        └───────────────────────────────────┘
```

Phases A→D are sequential (each unlocks the next critical-path
need). E and F parallelize with B-D when ownership doesn't overlap
(S3 stack is `kiseki-gateway/s3*` only; FUSE is `kiseki-client`).
G is the cleanup tail.

### D4. BDD `@integration` redefinition (addresses ADV-023-10)

A BDD scenario tagged `@integration` MAY claim spec conformance
**only** when the spec it cites is ✅ in the catalog. Until then,
those scenarios are tagged `@happy-path` and the BDD's RFC
references are documentation, not assertions.

#### D4.1 Three-phase transition plan

Renaming every existing `@integration` scenario to `@happy-path`
in one sweep would touch dozens of feature files before any
Layer-1 work landed. Instead:

- **Phase A (this ADR rev 2 — landing now)**: introduce
  `@happy-path` as a *superset* of `@integration` (cucumber treats
  them the same, no semantic change yet). New BDD scenarios that
  cite an RFC use both tags side-by-side.
- **Phase B (per-RFC)**: when an RFC's row goes ✅, the
  corresponding feature file is allowed to keep `@integration`
  alone. Until then, the dual tag stays. Existing CI behavior
  unchanged throughout.
- **Phase C (catalog all ✅)**: drop the dual-tag scaffold.
  Auditor gate-2 enforces: every `@integration` scenario maps to
  a ✅ row in the catalog.

This unblocks Layer-1 work without an organization-wide rename.

### D5. Auditor enforcement (depth gate 2 — extends D2)

`roles/auditor.md` already classifies BDD step depth (STUB →
SHALLOW → MOCK → THOROUGH). This ADR extends gate 2 with a fourth
axis: **spec fidelity**. For each `@integration` scenario that
cites an RFC, the auditor verifies the cited RFC is ✅ in the
catalog. If not, the scenario gets downgraded to `@happy-path`
until Layer 1 lands.

### D6. New protocol → catalog row first

Adding a new protocol surface:

1. Add a row to the catalog (status ❌).
2. Open ADR if the protocol introduces a new bounded context
   (e.g. ADR-038 for pNFS).
3. Build Layer 1 (decoder + section tests) BEFORE writing any
   BDD scenario that claims spec conformance.
4. When ✅, the BDD `@integration` tier may rely on the protocol
   without re-asserting wire-format details.

## Consequences

### Positive

- Latent wire-protocol bugs surface in `cargo test`, not at
  customer mount time.
- The Phase 15 e2e perf cluster starts producing meaningful
  numbers because the protocol layer is verified, not just
  observed.
- Rich, RFC-aligned negative-test coverage. Adversary review can
  reason about `NFS4ERR_*` paths from the test names alone.
- Documentation of the catalog itself surfaces gaps. Anyone
  reading `protocol-compliance.md` sees what is and isn't tested.
- Single ADR (this one) covers scope + discipline. No orphan
  ADR-039 with a "supersedes" arrow.

### Negative

- **Substantial up-front work** — ~18 specs in the catalog, weeks
  of cumulative effort. Layer 1 work blocks the Phase 15 perf
  cluster as long as the critical-path RFCs (8881, 7862, 8435)
  are ❌.
- **Maintenance burden** — RFC errata and updates must be tracked.
  Mitigated by per-section doc comments that say what spec rev a
  test was written against.
- **Reference-decoder duplication** — by construction we now have
  a decoder for every spec we encode/decode. That's the point;
  the decoder is the spec's executable form. But it's still 2× the
  XDR work.
- **Modifying an Accepted ADR** (rev 1 → rev 2) violates the
  immutability convention some teams use. Mitigated by a
  prominent revision-history block at the top.

### Mitigated risks

- **Slow start of Layer 1** — start with RFC 4506 (XDR) since
  every NFS spec depends on it. Once foundation is ✅, downstream
  specs go faster.
- **Premature ✅ tagging** — auditor gate-2 verifies the catalog
  status before allowing release.
- **Wire-sample provenance ambiguity** — D2.3.1 lists explicit
  source priority + storage policy.
- **`@happy-path` tag rollout chicken-and-egg** — D4.1 transition
  plan keeps existing CI green throughout.

### D7. Client-side protocol library (rev 3 — 2026-05-01)

`kiseki-client` provides client-side implementations for all three
protocol paths the cluster exposes:

| Feature flag | Transport | Target port | Interface |
|---|---|---|---|
| `remote-http` (exists) | HTTP/S3 | 9000 | `GatewayOps` |
| `remote-nfs` (new) | NFSv4 ONC RPC over TCP | 2049 | `GatewayOps` |
| `fuse` (exists) | `/dev/fuse` kernel | — | `KisekiFuse` |

The NFS client reuses `kiseki-gateway::nfs_xdr::{XdrWriter, XdrReader}`
and `kiseki-gateway::nfs4_server::op` constants for COMPOUND
construction. It manages an NFSv4.1 session (EXCHANGE_ID →
CREATE_SESSION → SEQUENCE-bound ops) over a single TCP connection.

Both `remote-http` and `remote-nfs` implement `GatewayOps`, so BDD
@integration steps use the same trait interface regardless of
protocol:

```rust
let s3 = KisekiClient::s3(server.s3_url(""));
s3.write(req).await?;  // HTTP PUT

let nfs = KisekiClient::nfs(server.nfs_addr());
nfs.write(req).await?; // NFSv4 OPEN+WRITE COMPOUND
```

Cross-protocol tests exercise both clients against the same server:
write via S3, read via NFS (and vice versa).

This closes the gap identified by the GCP deployment (2026-05-01):
BDD tests used in-memory domain objects instead of real protocol
clients. With `kiseki-client` as the BDD interface, every
@integration step exercises the full wire protocol stack.

## Open

- **Versioned spec compliance** — RFC 8881 has had errata. Should
  tests pin to "8881 + Errata 6178" or just "8881"? Default to
  "8881 + applicable errata as of test write time", document in
  the test header.
- **Per-section coverage measurement** — there's no automated
  way to detect a spec section that lacks a test. A lint that
  cross-references doc-comment section numbers vs the spec's TOC
  would help. Future work; not blocking.

## References

### Specifications

The catalog at
[`specs/architecture/protocol-compliance.md`](../protocol-compliance.md)
is the authoritative list. Highlights:

- RFC 4506 (XDR), RFC 5531 (ONC RPC v2), RFC 1057 (AUTH flavors)
- RFC 1813 (NFSv3), RFC 7530 (NFSv4.0), RFC 8881 (NFSv4.1,
  obsoletes RFC 5661), RFC 7862 (NFSv4.2)
- RFC 8435 (pNFS FFL), RFC 5665 (uaddr), RFC 9289 (NFS-over-TLS)
- RFC 9110/9111/9112 (HTTP/1.1), RFC 3986 (URI), RFC 8446 (TLS 1.3)
- AWS SigV4, AWS S3 REST API
- POSIX.1-2024, Linux FUSE protocol, macOS osxfuse

### Related ADRs

- [ADR-013 — POSIX semantics scope](013-posix-semantics-scope.md)
- [ADR-014 — S3 API scope](014-s3-api-scope.md)
- [ADR-037 — Test infrastructure (Raft harness + subsystem traits)](037-test-infrastructure.md)
- [ADR-038 — pNFS layout + DS subprotocol — surfaced the gap](038-pnfs-layout-and-ds-subprotocol.md)

### Bugs that motivated rev 2

- `crates/kiseki-gateway/src/nfs4_server.rs:218-249` — NULL ping
  was rejected with PROC_UNAVAIL (commit `5f6fece`).
- `crates/kiseki-gateway/src/nfs4_server.rs:367-394` —
  EXCHANGE_ID `eir_flags` were 0x01 (commit `7b1b4f6`).

### Roles affected

- [`roles/auditor.md`](../../.claude/roles/auditor.md) — gate-2
  extended by D5.
- [`roles/implementer.md`](../../.claude/roles/implementer.md) —
  TDD/BDD protocol extended by D6 (catalog-row-first).
