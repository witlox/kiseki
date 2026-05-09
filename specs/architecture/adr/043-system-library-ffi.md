# ADR-043: System Library FFI Policy

**Status**: Proposed (rev 2 — scoped to FFI policy only after gate-1)
**Date**: 2026-05-09 (rev 1); 2026-05-09 (rev 2 — same day, post gate-1)
**Deciders**: Architect (this draft); Adversary gate-1 closed.

## Revision history

- **rev 1 (2026-05-09)**: Original. Covered both system-library FFI (libfuse) and external-process-daemon adoption (nfs-ganesha) as a single combined policy. Adversary gate-1 (`specs/findings/2026-05-09-adv-gate1-adr043-findings.md`) returned **CHANGES REQUESTED** with 3 CRITICAL, 6 HIGH, 9 MEDIUM, 4 LOW + 3 cross-cutting findings. All three CRITICALs and most HIGHs targeted the ganesha-as-external-daemon scope:
  - F-C1: plaintext exposure across a new process boundary, unbounded by ADR-011's TTL.
  - F-C2: tenant identity propagation through ganesha's multi-export deployment.
  - F-C3: ganesha's default krb5 build silently importing MIT krb5 into the FIPS scope.
  - F-H1: ganesha-as-process reversibility ratchet once in-tree NFS code is deleted.
  - F-H5: ADR-038 pNFS interop with ganesha unverified.
  - F-H6: gRPC-boundary perf regression on every NFS op.
  Combined with the architectural realization that **the redundancy in our protocol surface is on the NFS axis specifically** (we hand-roll BOTH the NFS server and a userspace NFS client used only by tests + perf driver — see `specs/implementation/adr-043-libfuse-swap.md`), the rev-1 ganesha proposal was the wrong cut. The hand-rolled NFS server stays in tree; the redundant userspace NFS client is the thing to remove (separate work item, not this ADR).
- **rev 2 (2026-05-09)**: Scope reduced to FFI policy only. §D2 (process-daemon adoption) and §D4 (ganesha row) removed. §D7 (SELinux confinement of an external daemon) removed. The libfuse FFI direction survives because:
  - F-C1 doesn't apply: libfuse runs in the same process as the existing FUSE daemon; plaintext exposure shape is unchanged.
  - F-C2 doesn't apply: FUSE is single-tenant per mount.
  - F-C3 doesn't apply: libfuse performs no crypto; no krb5 dependency.
  - F-H1, F-H5, F-H6 are ganesha-specific.

  The surviving findings (F-H2 security-posture rule, F-H4 FIPS evaluator written reference, F-M1 license, F-M5 cross-platform, F-M8 fuser-PR alternative, F-M9 *-sys enforceability) are addressed in this rev-2.

## Context

ADR-001 (Pure Rust, No Mochi) and ADR-027 (Single-Language Rust Only) are foundational decisions about kiseki's implementation language. ADR-001 explicitly anticipates FFI to libfabric for Slingshot/Cassini transport (named in its Consequences), establishing a precedent that FFI to system C libraries is acceptable when those libraries do not enter the FIPS crypto module boundary. ADR-027 extends Rust-only across kiseki's own code (control plane and data plane) but is silent on system libraries we depend on rather than maintain.

Today, neither ADR codifies which additional system C libraries we will permit FFI to. The libfabric exception in ADR-001 is implicit, not policy. Without explicit policy, every new system-library question (libfuse for FUSE protocol dispatch; potentially others) re-litigates ADR-001 from scratch.

The 2026-05-07 / 2026-05-09 GCP `compact` runs surfaced a sustained pattern of bugs in our `fuser` 0.17-based FUSE daemon, including the FUSE_SYNCFS opcode 50 gap (`specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md`), single-thread inline dispatch limits (per user memory), and operational issues fixed in `f4b7b75`, `dbe957f`, `c96cb47`. The `fuser` Rust reimplementation has documented gaps that the upstream libfuse 3.x C library does not. Codifying when FFI is permissible lets us swap to libfuse without re-arguing the policy.

## Decision

### D1. FFI to system C libraries is permitted under the FIPS-isolation rule

FFI to a system C library is permitted iff **the library does not enter the FIPS crypto module boundary**. The FIPS module boundary for kiseki is the set of calls into `aws-lc-rs` from `kiseki-crypto` and direct callers (`kiseki-keymanager`, the data-path encryption sites in `kiseki-gateway`, audit-log signing in `kiseki-audit`). Any C library that performs encryption, decryption, key derivation, MAC computation, or any other primitive listed in FIPS 140-3 §6 is forbidden — that primitive must remain in `aws-lc-rs`-via-Rust.

C libraries that handle **transport mechanics, protocol framing, or filesystem-protocol dispatch without touching ciphertext-as-ciphertext or keys** are permitted. (System libraries that move the *bytes* of an encrypted payload are not entering the FIPS boundary; the FIPS boundary tracks *operations on plaintext or keys*.)

### D1.1. Security-posture rule (added rev 2 per gate-1 F-H2)

FIPS conformance is necessary but not sufficient. Every entry on the D2 positive list MUST, at the time of addition, name (a) the upstream's security-issue handling history, (b) the most recent CVE and time-to-patch, (c) the kiseki team's stated triage SLA for upstream advisories. CRITICAL upstream advisories triaged within 7 days; HIGH within 30 days; MEDIUM at next release. This is operational policy, not just a build-time concern.

### D2. Permitted system libraries (positive list)

| Library | Role | FFI shape | FIPS path? | Min version | Notes |
|---|---|---|---|---|---|
| `libfabric` | Transport (Slingshot/Cassini, EFA, generic OFI) | `libfabric-sys` Rust crate | No | per ADR-042 | Existing — pre-permitted by ADR-001 Consequences |
| `librdmacm` / `libibverbs` | Transport (InfiniBand, RoCEv2) | Rust bindings via `rdma-core` ecosystem | No | per ADR-042 | Existing — pre-permitted by extension of ADR-001 |
| `libfuse` 3.x | FUSE protocol dispatch | `kiseki-fuse-sys` (bindgen) + `kiseki-fuse` safe wrapper | No | 3.10 (FUSE_SYNCFS support) | **New rev-2 addition.** Replaces `fuser` 0.17. Implementation plan: `specs/implementation/adr-043-libfuse-swap.md`. Migration ADR: ADR-044 (planned). |

Adding a library to this table requires an ADR amendment plus the D1.1 security-posture data filled in.

### D3. Forbidden system libraries (negative list — illustrative, not exhaustive)

These categories MUST stay in Rust on aws-lc-rs:

- Crypto primitives (AES-GCM, HKDF-SHA256, ChaCha20-Poly1305, RSA, ECDSA, Ed25519, X.509 chain validation, TLS handshake)
- KEM / hybrid post-quantum primitives (ML-KEM, ML-DSA, hybrid X25519+ML-KEM)
- TLS implementation (rustls, not OpenSSL/BoringSSL; aws-lc-rs is rustls's crypto provider)
- Random number generation entering the crypto boundary (`SystemRandom` from aws-lc-rs)
- Kerberos GSS implementations (MIT krb5, Heimdal) — flagged explicitly because nfs-ganesha and similar daemons commonly link them by default; if RPCSEC_GSS becomes required, the implementation lives in `kiseki-keymanager` on aws-lc-rs.

A system library not on the D2 positive list and not obviously in D3's categories requires a new amendment to this ADR before adoption.

### D4. FFI binding crates and packaging convention

System library FFI bindings live in dedicated `*-sys` crates following the `libfabric-sys` precedent (existing) and `kiseki-fuse-sys` (new). The `*-sys` crate contains only `bindgen`-generated C declarations and minimal safety wrappers; the higher-level Rust API lives in a sibling crate (`kiseki-fuse` for the libfuse case, paralleling `kiseki-fabric` for libfabric). This separates the unsafe boundary from the safe Rust surface and matches the workspace's existing `*-sys` / `kiseki-*` split.

**Enforcement (added rev 2 per gate-1 F-M9):** a workspace lint rejects crates that declare `links = "..."` in Cargo.toml unless the crate name ends with `-sys`. Implemented either as a `cargo-deny` `bans` rule or a custom workspace lint; ADR-044 owns the concrete check.

Distribution implications:
- Build-time: `libfuse3-dev` (Debian/Ubuntu) / `fuse3-devel` (RHEL/Fedora). FFI/cdylib for Python and C++ wrappers picks up `libfuse3.so` transitively when the `fuse` feature is enabled. Wrappers built without `fuse` are unaffected.
- Cross-platform: `libfuse3` is Linux-only (via `/dev/fuse`). macOS uses macFUSE with a different ABI; Windows has no equivalent. **macOS and Windows FUSE are out of scope under this ADR** (rev-2 closure of gate-1 F-M5 / open item F). If a future requirement re-introduces them, that ADR adds rows to D2 (`macfuse-sys`, WinFsp-sys) under the same FIPS-isolation rule.

### D5. Reversibility

Each FFI binding lives in a dedicated `*-sys` + safe-wrapper crate pair (D4) that can be replaced wholesale. Reversal cost = swap the crate pair, refactor the call sites, bound by the trait surface the safe wrapper exposes. Pre-existing pure-Rust paths remain in tree until the migration ADR delivers a replacement at parity (e.g., `fuser` stays as a dependency until `kiseki-fuse` ships at parity per ADR-044).

For each addition to D2: the migration ADR commits to a "go / no-go" review at a specific milestone (e.g., 6 months of cluster operation post-merge). Triggers for marking a row Rejected: a kiseki-internal incident tracing to a binding's bug whose CVSS ≥ 7.0 and which upstream cannot patch within the D1.1 SLA; OR a perf regression > 20% on the Tier_1 reference perf-cluster matrix; OR a security advisory with no upstream patch path. Decision-maker: architect in consultation with kiseki-security.

### D6. Migration is per-binding and ADR-gated

This ADR codifies the policy. Each binding's adoption is its own ADR:

- **ADR-044 (planned)**: libfuse 3.x via `kiseki-fuse-sys` + `kiseki-fuse` — replaces `kiseki-client`'s `fuser`-based FUSE adapter. Plan: `specs/implementation/adr-043-libfuse-swap.md`.

The implementation ADR decides concrete shape (binding crate version pins; trait-surface details; testing strategy; performance targets; perf-cluster validation order). It references this ADR for the policy and inherits §D1, §D1.1, §D5.

Pre-existing pure-Rust paths (`fuser` 0.17 in `kiseki-client/src/fuse_daemon.rs`) remain in tree and CI-tested until ADR-044 delivers a replacement that passes the existing `@integration` suite at parity. No big-bang removal.

## Rationale

### The libfabric precedent

ADR-001 explicitly anticipated FFI to libfabric ("libfabric-sys crate needed for Slingshot support"). The argument was implicit: a fabric library at the transport layer doesn't enter the FIPS module boundary; FFI is acceptable when the C code never touches plaintext, keys, or crypto primitives. This ADR generalizes that argument and makes it policy.

### FIPS scope vs language scope

ADR-001's load-bearing rationale is the FIPS 140-3 module boundary. FIPS does not care about *bytes moving through transports*; it cares about *operations on cryptographic material*. A C library that dispatches FUSE opcodes is no more in the FIPS boundary than the kernel TCP stack is. Conflating "FIPS scope" with "language scope" is what makes ADR-001 read as more restrictive than its decision actually is.

### ADR-027's costs don't apply to system C libraries we depend on but don't maintain

ADR-027 enumerates four concrete costs of multi-language: domain-model drift, error-taxonomy duplication, second FIPS module, second CI lane. Linking `libfuse3.so` adds none of these:

- Domain-model: kiseki domain types live in `kiseki-common` Rust. libfuse defines its own opcode types; we map them at the FFI boundary, just as we already map gRPC types at the tonic boundary. No duplication of kiseki domain logic.
- Error-taxonomy: libfuse returns `errno` integers; we already do the `errno`↔`GatewayError` mapping at the FUSE handler edge in `crates/kiseki-client/src/fuse_daemon.rs`. The taxonomy stays single.
- Second FIPS module: libfuse has no crypto.
- Second CI lane: `libfuse3-dev` is an apt/yum dependency, not a toolchain. CI installs the package; the language we develop in is still Rust.

The argument that ADR-027 has the most teeth on — "kiseki's own code in two languages drifts" — does not apply when the C code is **someone else's project that we depend on but do not maintain**.

### Why libfuse and not "fix fuser-rs upstream"

The smaller-cost alternative (Alternative 6 below) is to file PRs upstream to `fuser` for each gap that bites us. The named gap (FUSE_SYNCFS opcode 50) is PR-able. But:

- The structural concern (single-thread inline dispatch, smaller maintainer pool, slower release cadence) is not a single-PR fix.
- libfuse 3.x is the reference implementation maintained by FUSE upstream itself; every production FUSE filesystem (sshfs, juicefs, gcsfuse, dfuse, ceph-fuse, gocryptfs) uses it.
- Cumulative PR-cycle cost across multiple gaps exceeds the one-time swap cost.

The fuser-PR-only path was considered and rejected for reliability reasons, not for any failure of the upstream maintainer or the crate's quality at smaller scope.

## Alternatives

1. **Status quo — no FFI policy beyond ADR-001's libfabric implicit exception.** Pro: minimum churn. Con: re-litigates the question for every new system library; the libfuse decision needs to happen anyway.
2. **Wholesale revise ADR-001 to "Rust where it matters, C where mature."** Too broad — removes the FIPS-boundary discipline that's worth keeping. The targeted policy here preserves what ADR-001 was actually protecting (FIPS surface, audit clarity) while making the libfabric exception generalizable.
3. **Custom FUSE kernel module** (Weka pattern). Multi-month development cost, distro-pinning maintenance burden, out-of-tree kmod packaging hell. Defer to a future ADR if libfuse perf on Tier_1 doesn't reach our targets.
4. **Permit libfuse via FFI but add ganesha-as-process for NFS** (rev-1 of this ADR). Rejected per gate-1 — see Revision history.
5. **Punt POSIX-via-FUSE entirely; ship kiseki as S3 + native only.** Radical scope cut. Loses the HPC/AI POSIX use case in CLAUDE.md's positioning.
6. **Stay on `fuser` and PR upstream fixes.** Considered; rejected per "Why libfuse and not 'fix fuser-rs upstream'" above.

## Consequences

### Positive

- `kiseki-client`'s FUSE adapter drops the `fuser` 0.17 dependency and its known gaps (FUSE_SYNCFS opcode 50 not implemented; single-thread inline dispatch). Inherits libfuse 3.x's multi-thread session loop and full opcode coverage.
- ADR-001's libfabric exception is now codified rather than implicit. Future system-library questions decide against this policy, not by re-reading ADR-001 each time.
- Aligns kiseki's FUSE path with the dominant production pattern (sshfs, juicefs, gcsfuse, dfuse all on libfuse 3.x).
- The `*-sys` enforcement rule (D4) prevents FFI from leaking into non-`*-sys` crates by accident.

### Negative

- New build-time dep: `libfuse3-dev`. New runtime dep: `libfuse3.so.3`. Cross-distro packaging matrix grows by one package.
- macOS and Windows FUSE are out of scope under this ADR. Downstream wrappers building for those platforms must disable the `fuse` feature or wait for a future macFUSE / WinFsp ADR.
- FFI/cdylib path for Python and C++ wrappers picks up `libfuse3.so` transitively when `fuse` feature is enabled. Wrappers must document this.
- LGPL-2.1 dynamic-linking obligations propagate to the kiseki-client cdylib's `fuse` feature builds. Compatible with permissive overall licenses via the dynamic-linking exception, but downstream wrappers (Python via PyO3, C++) must honor LGPL-2.1's source-availability requirement for the kiseki-client object code on the FUSE feature path. ADR-044 owns the explicit license decision.
- Reversal cost for ADR-044: once `fuser` is removed from `kiseki-client/Cargo.toml` (D6 final step), reverting requires re-adding it and re-porting `fuse_daemon.rs`. Mitigated by the parity-with-existing requirement in D6 (no removal until parity in `@integration`).
- ADR-027's "single language" cognitive-load benefit is partially given up for contributors touching the FUSE adapter (they learn libfuse's C API via the safe wrapper). The Rust-only invariant for kiseki's *own* code stays.

## Open items (carry into ADR-044)

- **A**: Confirm `libfuse3` LGPL-2.1 license is compatible with kiseki's overall license and downstream wrapper distribution. ADR-044 owns the explicit decision and CONTRIBUTING.md note.
- **B**: Verify the FIPS argument concretely under FIPS 140-3 IG §A.5 (cryptographic boundary scope) and §C.G (loadable libraries). Existing precedent (kernel TCP, libfabric, network switches) says transport-only C libraries don't enter the boundary, but get a written reference from a FIPS evaluator before the certification effort starts. Until the reference is filed, ADR-043 stays Proposed.
- **C**: Cross-platform FUSE policy is silently retired here. If macOS / Windows FUSE later becomes a requirement, that ADR adds D2 rows under the same rules.

## References

- ADR-001: Pure Rust, No Mochi — the libfabric precedent this ADR codifies and generalizes.
- ADR-013: POSIX Semantics Scope — defines the FUSE-supported operation matrix; unaffected by this ADR.
- ADR-019: Gateway Deployment Model — defines the gateway-as-binary pattern that the FUSE daemon already follows.
- ADR-027: Single-Language Rust Only — bounded by this ADR's reading: about kiseki's own code, not about system libraries we depend on.
- ADR-042: Native Gateway Data Service — independent; native clients don't go through libfuse. ADR-042's libfabric/ibverbs bindings are codified by this ADR's D2 retroactively.
- `specs/findings/2026-05-09-adv-gate1-adr043-findings.md` — gate-1 findings that drove the rev-1 → rev-2 scope reduction.
- `specs/implementation/adr-043-libfuse-swap.md` — implementation plan for the libfuse swap that ADR-044 will formalize.
- `specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md` — the original FUSE_SYNCFS opcode 50 finding; the libfuse swap closes it.
