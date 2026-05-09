# ADR-043: System Library FFI Policy

**Status**: Proposed (rev 4 — gate-1 round-3 plan-specific findings closed; acceptance pending only on Open item B)
**Date**: 2026-05-09 (rev 1); 2026-05-09 (rev 2 same day, post gate-1); 2026-05-09 (rev 3 same day, post gate-1 round 2); 2026-05-09 (rev 4 same day, post gate-1 round 3 / plan-specific)
**Deciders**: Architect (this draft); Adversary gate-1 rounds 1, 2, and 3 all closed.

## Revision history

- **rev 1 (2026-05-09)**: Original. Covered both system-library FFI (libfuse) and external-process-daemon adoption (nfs-ganesha) as a single combined policy. Adversary gate-1 (`specs/findings/2026-05-09-adv-gate1-adr043-findings.md`) returned **CHANGES REQUESTED** with 3 CRITICAL, 6 HIGH, 9 MEDIUM, 4 LOW + 3 cross-cutting findings. All three CRITICALs and most HIGHs targeted the ganesha-as-external-daemon scope:
  - F-C1: plaintext exposure across a new process boundary, unbounded by ADR-011's TTL.
  - F-C2: tenant identity propagation through ganesha's multi-export deployment.
  - F-C3: ganesha's default krb5 build silently importing MIT krb5 into the FIPS scope.
  - F-H1: ganesha-as-process reversibility ratchet once in-tree NFS code is deleted.
  - F-H5: ADR-038 pNFS interop with ganesha unverified.
  - F-H6: gRPC-boundary perf regression on every NFS op.
  Combined with the architectural realization that **the redundancy in our protocol surface is on the NFS axis specifically** (we hand-roll BOTH the NFS server and a userspace NFS client used only by tests + perf driver — see `specs/implementation/libfuse-swap.md`), the rev-1 ganesha proposal was the wrong cut. The hand-rolled NFS server stays in tree; the redundant userspace NFS client is the thing to remove (separate work item, not this ADR).
- **rev 2 (2026-05-09)**: Scope reduced to FFI policy only. §D2 (process-daemon adoption) and §D4 (ganesha row) removed. §D7 (SELinux confinement of an external daemon) removed. The libfuse FFI direction survives because:
  - F-C1 doesn't apply: libfuse runs in the same process as the existing FUSE daemon; plaintext exposure shape is unchanged.
  - F-C2 doesn't apply: FUSE is single-tenant per mount.
  - F-C3 doesn't apply: libfuse performs no crypto; no krb5 dependency.
  - F-H1, F-H5, F-H6 are ganesha-specific.

  The surviving findings (F-H2 security-posture rule, F-H4 FIPS evaluator written reference, F-M1 license, F-M5 cross-platform, F-M8 fuser-PR alternative, F-M9 *-sys enforceability) are addressed in this rev-2.
- **rev 3 (2026-05-09)**: Adversary gate-1 round 2 (`specs/findings/2026-05-09-adv-gate1-round2-adr043-findings.md`) reviewed rev-2 itself plus the §D6 plan-gating amendment (commit `6fb88aa`) plus the libfuse-swap implementation plan, returning **CHANGES REQUESTED — small scope** (0 CRITICAL, 3 HIGH, 6 MEDIUM, 3 LOW). Rev-3 closes:
  - F2-H1: §D6 illustrative "e.g." criteria → exhaustive 7-row checklist with "if any answer is yes" rule.
  - F2-M1: §D2 grandfathered libfabric / libibverbs rows backfilled with D1.1 security-posture data.
  - F2-M2: §D6 review-discipline gains "amendments trigger re-review" rule.
  - F2-M6: §D2 libfuse row clarifies kernel-side (≥ 5.4) vs userspace-side (libfuse 3.10) version dependency.
  - F2-L1: New §"Review schedule" section names architect as owner and lists per-binding review dates.
  - F2-L2: §"Why libfuse..." adds deployment-scale grounding for the production-use citation.
  - CC2-1: §D6 findings-filename convention adopts the existing `YYYY-MM-DD-adv-gate1-<artifact>-findings.md` shape.

  The remaining round-2 findings (F2-H2 D1.1 acceptance check; F2-H3 FFI safety contract; F2-M3 GCP build-path audit; F2-M4 go/no-go criteria; F2-M5 rollback procedure; F2-L3 macOS-retirement upgrade) are amendments to `specs/implementation/libfuse-swap.md`, applied in the same change-set.
- **rev 4 (2026-05-09)**: Adversary gate-1 round 3 (`specs/findings/2026-05-09-adv-gate1-libfuse-swap-findings.md`) was the first plan-specific gate-1 — attacked the libfuse-swap implementation plan directly per the §D6 review-discipline. Returned **CHANGES REQUESTED — small-to-medium scope** (0 CRITICAL, 5 HIGH, 8 MEDIUM, 4 LOW + 2 cross-cutting). Rev-4 closes the policy-side findings:
  - F3-H1: §D6 checklist criterion 4 (license change) — surfaced as plausibly **yes**. Resolution: in-plan justification of the **no** answer (LGPL-2.1's dynamic-linking exception keeps kiseki-client itself permissive; libfabric precedent established the same shape; downstream wrapper LGPL exposure is operational disclosure, not architectural change). Plan's §D6 checklist table now documents this.
  - F3-H2: §D6 checklist criterion 6 (new invariants/failure modes) — was plausibly **yes**. Resolution: catalogue promotion (path b). The libfuse-swap plan's §"Safety contract" rules promoted to `specs/invariants.md` as I-FUSE-1..I-FUSE-8; failure modes promoted to `specs/failure-modes.md` as F-FUSE-1..F-FUSE-3. With the catalogues updated, the plan introduces no *new* invariants beyond the catalogues — answer is **no**.
  - CC3-1: process note (this rev): the plan's first heading section MUST contain a §D6 checklist table with one row per criterion and a yes/no/explain answer. libfuse-swap.md adds the table at the top.

  All other round-3 findings (F3-H3 ADR-013 parity, F3-H4 async-bridge mechanics, F3-H5 session-crash handling, all 8 MEDIUMs, all 4 LOWs) are plan-side and are addressed in the same change-set's amendments to `specs/implementation/libfuse-swap.md`. With the round-3 closure, the plan is ready for implementer phase 0.

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
| `libfabric` | Transport (Slingshot/Cassini, EFA, generic OFI) | `libfabric-sys` Rust crate | No | per ADR-042 | Existing — pre-permitted by ADR-001 Consequences. **D1.1 data**: upstream `ofiwg/libfabric` on GitHub; security advisories tracked at GitHub Security Advisories on the upstream repo; no kiseki-blocking CVE in the 24 months preceding 2026-05-09; kiseki triage SLA = D1.1 default (CRITICAL ≤ 7d, HIGH ≤ 30d, MEDIUM at next release). |
| `librdmacm` / `libibverbs` | Transport (InfiniBand, RoCEv2) | Rust bindings via `rdma-core` ecosystem | No | per ADR-042 | Existing — pre-permitted by extension of ADR-001. **D1.1 data**: upstream `linux-rdma/rdma-core`; security advisories tracked via GitHub Security Advisories; CVE history reviewed at this rev — no kiseki-blocking advisory open at 2026-05-09; kiseki triage SLA = D1.1 default. |
| `libfuse` 3.x | FUSE protocol dispatch | `kiseki-fuse-sys` (bindgen) + `kiseki-fuse` safe wrapper | No | userspace ≥ 3.10 + kernel ≥ 5.4 | **New rev-2 addition.** Replaces `fuser` 0.17. The userspace floor (libfuse 3.10) is the Debian/Ubuntu LTS shipped version. The kernel floor (≥ 5.4) covers FUSE_SYNCFS opcode 50 dispatch (kernel-side; libfuse exposes the userspace callback from 3.0+, but the kernel only sends the opcode on ≥ 5.1 — kiseki requires ≥ 5.4 for stability). Operator docs document the kernel floor at acceptance of the libfuse swap. Implementation plan: `specs/implementation/libfuse-swap.md`. No per-binding ADR per §D6. **D1.1 data**: upstream `libfuse/libfuse`; security advisories tracked at the upstream's GitHub Security Advisories; CVE history filled in by libfuse-swap.md acceptance criterion (7); kiseki triage SLA = D1.1 default. |

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

**Enforcement (added rev 2 per gate-1 F-M9):** a workspace lint rejects crates that declare `links = "..."` in Cargo.toml unless the crate name ends with `-sys`. Implemented either as a `cargo-deny` `bans` rule or a custom workspace lint; the libfuse-swap implementation plan owns the concrete check.

Distribution implications:
- Build-time: `libfuse3-dev` (Debian/Ubuntu) / `fuse3-devel` (RHEL/Fedora). FFI/cdylib for Python and C++ wrappers picks up `libfuse3.so` transitively when the `fuse` feature is enabled. Wrappers built without `fuse` are unaffected.
- Cross-platform: `libfuse3` is Linux-only (via `/dev/fuse`). macOS uses macFUSE with a different ABI; Windows has no equivalent. **macOS and Windows FUSE are out of scope under this ADR** (rev-2 closure of gate-1 F-M5 / open item F). If a future requirement re-introduces them, that ADR adds rows to D2 (`macfuse-sys`, WinFsp-sys) under the same FIPS-isolation rule.

### D5. Reversibility

Each FFI binding lives in a dedicated `*-sys` + safe-wrapper crate pair (D4) that can be replaced wholesale. Reversal cost = swap the crate pair, refactor the call sites, bound by the trait surface the safe wrapper exposes. Pre-existing pure-Rust paths remain in tree until the migration plan delivers a replacement at parity (e.g., `fuser` stays as a dependency until `kiseki-fuse` ships at parity).

For each addition to D2: the migration plan commits to a "go / no-go" review at a specific milestone (e.g., 6 months of cluster operation post-merge). Triggers for marking a row Rejected: a kiseki-internal incident tracing to a binding's bug whose CVSS ≥ 7.0 and which upstream cannot patch within the D1.1 SLA; OR a perf regression > 20% on the Tier_1 reference perf-cluster matrix; OR a security advisory with no upstream patch path. Decision-maker: architect in consultation with kiseki-security.

### D6. Migration is per-binding and plan-gated

This ADR codifies the policy. Each binding's adoption is governed by an implementation plan in `specs/implementation/`. A separate per-binding ADR is required if **any** of the following criteria apply (the architect documents the answer to each before merging the plan; the gate-1 review verifies the answers):

1. The binding introduces a new bounded-context boundary (a new OS process kiseki maintains, a new RPC service in `kiseki-proto`, or a new cross-language wire format).
2. The binding's auth/authz model differs from kiseki's existing tenant identity propagation (`OrgId` / `NamespaceId` end-to-end through the call chain).
3. The binding introduces a new ubiquitous-language term (`specs/ubiquitous-language.md` gains a new entry).
4. The binding's license materially changes downstream distribution shape (transitions across permissive ↔ LGPL ↔ copyleft; or wrapper LGPL exposure that didn't exist before).
5. The binding's distribution shape requires new packaging steps on the GCP perf cluster, the dev environment, or downstream wrapper builds beyond a single distro-package install.
6. The binding adds a new failure mode (per `specs/failure-modes.md`) or a new invariant (per `specs/invariants.md`).
7. The binding's adoption changes any existing ADR's decision (i.e., requires another ADR to be revised).

If every answer is **no**, the policy ADR plus implementation plan are sufficient; no per-binding ADR ceremony. A **yes** to any criterion triggers a per-binding ADR.

Active plans:

- **`specs/implementation/libfuse-swap.md`**: libfuse 3.x via `kiseki-fuse-sys` + `kiseki-fuse` — replaces `kiseki-client`'s `fuser`-based FUSE adapter. Decides binding-crate version pin, trait-surface details, testing strategy, performance targets at parity, perf-cluster validation order. Inherits §D1, §D1.1, §D2, §D5. Architect's checklist answers documented in the plan's §"§D6 checklist" table (rev-4): every criterion answered **no**, with explicit justifications for criteria 4 (license — dynamic-linking exception) and 6 (invariants — promoted to catalogues so the plan introduces no *new* invariants beyond `specs/invariants.md` and `specs/failure-modes.md`). Therefore plan-only adoption is appropriate.

Pre-existing pure-Rust paths (`fuser` 0.17 in `kiseki-client/src/fuse_daemon.rs`) remain in tree and CI-tested until the plan delivers a replacement that passes the existing `@integration` suite at parity. No big-bang removal.

**Review discipline**: even without a per-binding ADR, the implementation plan IS reviewed by adversary gate-1 (per `.claude/CLAUDE.md` Diamond workflow) BEFORE implementer phase 0. Skipping the per-binding ADR does not skip the adversary review — it relocates the review from "attack the ADR" to "attack the plan." Findings live in `specs/findings/` keyed on the plan basename and date: `YYYY-MM-DD-adv-gate1-<plan-base-name>-findings.md` (per the existing convention used for ADR findings). The review verifies the architect's answers to the checklist above; if any answer is plausibly **yes** but was answered **no**, the adversary requires a per-binding ADR before implementer phase 0.

**Amendment-trigger rule**: material amendments to a reviewed plan (new phases, new dependencies, scope expansion, removal of acceptance criteria) trigger a gate-1 round-N+1 on the amended sections only. Cosmetic edits (typos, formatting, citation fixes) do not. The architect tags the commit message with `gate-1-amendment` so the review trigger is visible in `git log`.

**Plan-frontmatter-checklist rule (added rev 4 per gate-1 round-3 CC3-1)**: every implementation plan in `specs/implementation/` covered by §D6's plan-only adoption path MUST include, in its first heading section, a §D6 checklist table with one row per criterion and a yes/no/explain answer. This makes both the architect's reasoning and the adversary's verification machine-traceable. The plan-specific gate-1 review verifies the table directly. The libfuse-swap plan's table at `specs/implementation/libfuse-swap.md` §"§D6 checklist" is the reference shape for future plans.

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
- libfuse 3.x is the reference implementation maintained by FUSE upstream itself. Production deployment scale: sshfs (millions of installs as the de-facto remote-mount tool); juicefs (thousands of production clusters); gcsfuse (Google Cloud Storage's FUSE adapter, used at GCP scale); dfuse (DAOS POSIX gateway, deployed on US national-lab HPC clusters); ceph-fuse (Ceph's userspace POSIX path, alternative to the kernel client at site-by-site choice); gocryptfs (encrypted-filesystem tooling). The pattern is: libfuse is what production FUSE filesystems sit on; pure-Rust reimplementations like fuser-rs are smaller-scope alternatives without that deployment base.
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
- LGPL-2.1 dynamic-linking obligations propagate to the kiseki-client cdylib's `fuse` feature builds. Compatible with permissive overall licenses via the dynamic-linking exception, but downstream wrappers (Python via PyO3, C++) must honor LGPL-2.1's source-availability requirement for the kiseki-client object code on the FUSE feature path. The libfuse-swap plan owns the explicit license decision.
- Reversal cost for the libfuse swap: once `fuser` is removed from `kiseki-client/Cargo.toml` (final step of the plan), reverting requires re-adding it and re-porting `fuse_daemon.rs`. Mitigated by the parity-with-existing requirement in D6 (no removal until parity in `@integration`).
- ADR-027's "single language" cognitive-load benefit is partially given up for contributors touching the FUSE adapter (they learn libfuse's C API via the safe wrapper). The Rust-only invariant for kiseki's *own* code stays.

## Open items (carry into the libfuse-swap implementation plan)

- **A**: Confirm `libfuse3` LGPL-2.1 license is compatible with kiseki's overall license and downstream wrapper distribution. The libfuse-swap plan owns the explicit decision and CONTRIBUTING.md note.
- **B**: Verify the FIPS argument concretely under FIPS 140-3 IG §A.5 (cryptographic boundary scope) and §C.G (loadable libraries). Existing precedent (kernel TCP, libfabric, network switches) says transport-only C libraries don't enter the boundary, but get a written reference from a FIPS evaluator before the certification effort starts. Until the reference is filed, ADR-043 stays Proposed.
- **C**: Cross-platform FUSE policy is silently retired here. If macOS / Windows FUSE later becomes a requirement, that ADR adds D2 rows under the same rules.

## Review schedule

The architect (currently the workflow owner per `.claude/CLAUDE.md`) tracks per-binding go/no-go review dates in this section. Each row is set when the binding's final D6 step lands (e.g., for libfuse: when `fuser` is removed from `Cargo.lock`).

| Binding | Final D6 step merged | Go/no-go review due (+ 6 months) | Owner | Status |
|---|---|---|---|---|
| `libfabric` | n/a (pre-ADR-043; ADR-001 era) | n/a (grandfathered into D2 at this ADR) | Architect | Active |
| `librdmacm` / `libibverbs` | n/a (pre-ADR-043 via ADR-042) | n/a (grandfathered into D2 at this ADR) | Architect | Active |
| `libfuse` 3.x | TBD (set at libfuse-swap.md final phase merge) | TBD + 6 months | Architect | Pending swap |

Triggers for the review (per §D5): perf regression > 20% on Tier_1 reference matrix, OR an unpatched CRITICAL/HIGH CVE older than the D1.1 SLA, OR a kiseki-internal incident with CVSS ≥ 7.0 traced to the binding. Decision-maker: architect in consultation with kiseki-security (a designated reviewer per the project's role definitions).

## References

- ADR-001: Pure Rust, No Mochi — the libfabric precedent this ADR codifies and generalizes.
- ADR-013: POSIX Semantics Scope — defines the FUSE-supported operation matrix; unaffected by this ADR.
- ADR-019: Gateway Deployment Model — defines the gateway-as-binary pattern that the FUSE daemon already follows.
- ADR-027: Single-Language Rust Only — bounded by this ADR's reading: about kiseki's own code, not about system libraries we depend on.
- ADR-042: Native Gateway Data Service — independent; native clients don't go through libfuse. ADR-042's libfabric/ibverbs bindings are codified by this ADR's D2 retroactively.
- `specs/findings/2026-05-09-adv-gate1-adr043-findings.md` — round-1 findings that drove the rev-1 → rev-2 scope reduction.
- `specs/findings/2026-05-09-adv-gate1-round2-adr043-findings.md` — round-2 findings that drove the rev-2 → rev-3 amendments documented in the Revision history.
- `specs/findings/2026-05-09-adv-gate1-libfuse-swap-findings.md` — round-3 plan-specific findings that drove the rev-3 → rev-4 amendments + the catalogue promotions (I-FUSE-1..8, F-FUSE-1..3).
- `specs/implementation/libfuse-swap.md` — implementation plan for the libfuse swap; per §D6, no per-binding ADR is required. The plan's §"§D6 checklist" table documents the architect's answers per the rule in §D6 above.
- `specs/invariants.md` §"FUSE wrapper invariants (libfuse-swap, ADR-043 §D2)" — I-FUSE-1..I-FUSE-8 promoted from the libfuse-swap plan's §"Safety contract" per gate-1 round-3 F3-H2.
- `specs/failure-modes.md` §"FUSE wrapper failures (libfuse-swap, ADR-043 §D2)" — F-FUSE-1..F-FUSE-3 promoted from the libfuse-swap plan per gate-1 round-3 F3-H2.
- `specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md` — the original FUSE_SYNCFS opcode 50 finding; the libfuse swap closes it.
