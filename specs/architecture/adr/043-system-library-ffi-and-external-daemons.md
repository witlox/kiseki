# ADR-043: System Library FFI and External Process Daemon Policy

**Status**: Proposed (drafted 2026-05-09; awaiting analyst seed + adversary gate-1)
**Date**: 2026-05-09
**Deciders**: Architect (this draft); Analyst seed and Adversary gate-1 to follow before acceptance.
**Context**: ADR-001 (Pure Rust, No Mochi), ADR-013 (POSIX semantics scope), ADR-019 (gateway deployment model), ADR-023 (protocol RFC compliance + revisions, including the 2026-04-27 wire-fidelity bug surfacing), ADR-027 (Single-Language Rust Only), ADR-038 (pNFS layout), ADR-042 (native gateway data service). Triggered by the 2026-05-07 / 2026-05-09 GCP `compact` runs, which surfaced a sustained pattern of bugs in our hand-rolled NFS server (NFSv4 NULL ping, EXCHANGE_ID flags, mkdir handle registration, getattr handle dispatch, NFSv3 wrapper transient errors masked as NotFound) and FUSE daemon (per-peer cap collision, LAST-ACK pile-up, zombie mountpoint, FUSE_SYNCFS gap on opcode 50). The surface area on which we pay this debt is *protocol mechanics* — XDR / COMPOUND framing, FUSE opcode dispatch, session/handle bookkeeping — none of which is differentiating to kiseki and all of which has decades-old, battle-hardened open-source equivalents.

## Problem

ADR-001 reads, in three places:

> "Build all core components in Rust. Do not depend on Mochi (Mercury/Bake/SDSKV)."
>
> "C/C++ FFI creates a FIPS compliance surface across two languages. Single-language FIPS module boundary is cleaner for certification."
>
> "**Weakest link is libfabric/CXI Rust binding** — bounded scope, solvable" — and in Consequences: "**libfabric-sys crate needed** for Slingshot support (immature, may need contribution)."

ADR-001 is therefore not a blanket prohibition on FFI to system C libraries. It already permits FFI to a system fabric library (libfabric) for transport. The "no FFI" decision was specifically about not adopting **Mochi's full storage stack** (Mercury for transport, Bake for chunks, SDSKV for KV) as the foundation of kiseki's domain logic. The FIPS argument it cites is specifically about **crypto code** crossing the language boundary — which libfabric does not, and which the system protocol libraries we now want to consider also do not.

ADR-027 ("Single-Language Rust Only") is more often misread as a blanket Rust-only rule. Reading its Rationale, the costs it enumerates — drift in the domain model across languages, two error taxonomies, two FIPS modules, two CI lanes — are about **kiseki's own code being implemented in two languages**. Linking against `libfuse3.so` (or running `nfs-ganesha` as a sidecar daemon that talks gRPC) does not duplicate kiseki domain code, does not add a second toolchain we develop in, and does not split the FIPS module: aws-lc-rs remains the single FIPS crypto provider for everything kiseki computes itself.

Today, however, neither ADR codifies *which* system C libraries we will permit FFI to and *which* external C daemons we will permit behind the existing gRPC boundary. The libfabric exception in ADR-001 is implicit, not policy. Without explicit policy, every new system-library question (libfuse for FUSE; nfs-ganesha for NFS; potentially `librdma`, `libdaos`, `libspdk` later) re-litigates ADR-001 from scratch.

The pattern of bugs cited in **Context** is structural: the kiseki team has been hand-rolling protocol mechanics that mature open-source components already implement to RFC fidelity. Industry-comparable HPC/AI storage vendors (VAST, Weka, Ceph, Lustre, DAOS, JuiceFS) do not hand-roll NFS protocol parsing or FUSE protocol dispatch; they either ship a custom kernel module (multi-month, distro-pinning effort) or compose their differentiating logic with mature system components (`nfs-ganesha` + FSAL plugin; `libfuse` 3.x via FFI). See "Industry comparison" in Rationale.

## Decision

### D1. FFI to system C libraries is permitted under the FIPS-isolation rule

FFI to a system C library is permitted iff **the library does not enter the FIPS crypto module boundary**. The FIPS module boundary for kiseki is the set of calls into `aws-lc-rs` from `kiseki-crypto` and direct callers (`kiseki-keymanager`, the data-path encryption sites in `kiseki-gateway`, audit-log signing in `kiseki-audit`). Any C library on the data path that performs encryption, decryption, key derivation, MAC computation, or any other primitive listed in FIPS 140-3 §6 is forbidden — that primitive must remain in `aws-lc-rs`-via-Rust.

C libraries that handle **transport mechanics, protocol framing, or filesystem-protocol dispatch without touching ciphertext-as-ciphertext or keys** are permitted. (System libraries that move the *bytes* of an encrypted payload are not entering the FIPS boundary; the FIPS boundary tracks *operations on plaintext or keys*.)

### D2. Process-isolated C daemons are permitted behind the existing gRPC boundary

A C daemon may run in its own OS process and communicate with `kiseki-server` (or `kiseki-control`, or `kiseki-client`) over an existing or new gRPC service defined in `kiseki-proto`. This requires:

1. The C daemon **never shares an address space** with any kiseki Rust process.
2. The wire boundary between daemon and kiseki uses gRPC over either Unix-domain socket (preferred for co-located deployments) or TCP+mTLS (per ADR-019's deployment model).
3. The daemon receives only data and operations it strictly needs; in particular, it does **not** receive raw chunk envelope keys, master-key material, or unencrypted user-data plaintext unless its role is plaintext-handling (a NFS gateway, by definition, handles plaintext on behalf of the client — that is its purpose; this is no different from the existing kiseki-gateway NFS handler).
4. The daemon's audit posture is documented: which calls it makes are logged where, and how its own logs integrate with `kiseki-audit`.

This is **not** new ground. ADR-027 already accepts that `kiseki-control` runs as its own binary talking to `kiseki-server` over gRPC ("Runtime separation" in §Enforcement). D2 generalizes that pattern to non-Rust daemons that fit the same shape.

### D3. Permitted system libraries (positive list)

| Library | Role | FFI shape | FIPS path? | ADR | Status |
|---|---|---|---|---|---|
| `libfabric` | Transport (Slingshot/Cassini, EFA, generic OFI) | `libfabric-sys` Rust crate | No | ADR-001 (implicit), ADR-042 | Existing — pre-permitted by ADR-001 |
| `librdmacm` / `libibverbs` | Transport (InfiniBand, RoCEv2) | Rust bindings via `rdma-core` ecosystem | No | ADR-042 | Existing — pre-permitted by extension of ADR-001 |
| `libfuse` 3.x | FUSE protocol dispatch | `libfuse-sys` (new) or via the `fuse3` crate's libfuse backend | No | This ADR; implementation ADR pending | **New** under this ADR |

### D4. Permitted external process daemons (positive list)

| Daemon | Role | Boundary | FIPS path? | ADR | Status |
|---|---|---|---|---|---|
| `nfs-ganesha` | NFS protocol mechanics (NFSv3/v4.1/v4.2 parsing, COMPOUND dispatch, session/handle bookkeeping, pNFS MDS surface) — kiseki implements an FSAL plugin that maps ganesha's filesystem callbacks onto kiseki RPCs | gRPC over UDS or TCP+mTLS via a new `kiseki-proto::nfs_fsal` service | No (plaintext only; encryption stays in kiseki-server) | This ADR; implementation ADR pending | **New** under this ADR |

### D5. Forbidden system libraries (negative list — illustrative, not exhaustive)

These categories MUST stay in Rust on aws-lc-rs:

- Crypto primitives (AES-GCM, HKDF-SHA256, ChaCha20-Poly1305, RSA, ECDSA, Ed25519, X.509 chain validation, TLS handshake)
- KEM / hybrid post-quantum primitives (ML-KEM, ML-DSA, hybrid X25519+ML-KEM)
- TLS implementation (rustls, not OpenSSL/BoringSSL; aws-lc-rs is rustls's crypto provider)
- Random number generation entering the crypto boundary (`SystemRandom` from aws-lc-rs)

Operational libraries below the FIPS boundary that are **not** in any positive list above require a new amendment to this ADR before adoption.

### D6. FFI binding crates and packaging convention

System library FFI bindings live in dedicated `*-sys` crates following the `libfabric-sys` precedent (existing) and `libfuse-sys` (new). The `*-sys` crate contains only `bindgen`-generated C declarations and minimal safety wrappers; the higher-level Rust API lives in a sibling crate (`kiseki-fuse` for the libfuse case, paralleling `kiseki-fabric` for libfabric). This separates the unsafe boundary from the safe Rust surface and matches the workspace's existing `*-sys` / `kiseki-*` split.

Distribution implications:

- Build-time: distros pick up a build-dep on `libfuse3-dev` and a runtime-dep on `libfuse3.so.3`. `nfs-ganesha` is a separate binary package the operator installs alongside `kiseki-server`; kiseki provides a packaged FSAL plugin shared object (`libfsalkiseki.so` or similar) that ganesha loads.
- Cross-platform: `libfuse3` is Linux-only (via `/dev/fuse`). macOS uses `osxfuse`/`macfuse` with a different ABI; Windows has no equivalent. The kiseki-client `fuse` feature was already gated on libfuser-having-`fuser-0.17`; this ADR does not change that gating posture, only the underlying binding. `nfs-ganesha` is Linux-only.
- FFI/cdylib for Python and C++ wrappers (the `kiseki-client` cdylib path) picks up `libfuse3.so` transitively when the `fuse` feature is enabled. Wrappers built without `fuse` are unaffected.

### D7. SELinux / sandbox posture

The nfs-ganesha process MUST run with a sandbox profile that prevents loading aws-lc-rs at runtime. Concretely, kiseki ships a SELinux module (or AppArmor profile, whichever the distro supports) that confines `ganesha.nfsd` to: read/write the FSAL UDS socket, read its config, write its log; deny all crypto-library mmaps and any access to kiseki master-key material. This makes the FIPS module boundary enforceable at the OS level, not just at the source-code level.

### D8. Migration is per-component and ADR-gated

This ADR codifies the policy. Each component migration is its own ADR:

- **ADR-044 (planned)**: kiseki FSAL plugin for nfs-ganesha — replaces `kiseki-gateway`'s NFSv3/v4.1/v4.2/pNFS handlers
- **ADR-045 (planned)**: libfuse 3.x via `kiseki-fuse` crate — replaces `kiseki-client`'s `fuser`-based FUSE adapter

The implementation ADRs decide concrete shape (FSAL plugin C/Rust mix; libfuse binding crate choice; testing strategy; performance targets; perf-cluster validation order). They reference this ADR for the policy and inherit its FIPS-isolation rules (§D1, §D7) and its boundary discipline (§D2).

The existing pure-Rust NFS handlers and `fuser`-based FUSE adapter remain in tree and CI-tested until ADR-044 and ADR-045 deliver replacements that pass the existing `@integration` suite at parity. No big-bang removal.

### D9. Reversibility

If a permitted system library or external daemon turns out to be unfit (perf cliff, unmanageable dependency drift, FIPS-boundary leak discovered post-hoc), the ADR row is marked **Rejected** with a justification, and the code base reverts to the prior pure-Rust path. The reversibility cost is bounded because:

- All FFI lives in dedicated `*-sys` / `kiseki-*` crates (D6), which can be replaced wholesale.
- All external daemons are behind gRPC boundaries (D2), so swapping `nfs-ganesha` for an in-tree Rust NFS gateway is a config and binary change, not a contract change.
- The pre-existing pure-Rust paths remain in CI until D8's migration ADRs land replacements at parity.

## Rationale

### The libfabric precedent — ADR-001 already permits this

ADR-001 explicitly anticipated FFI to libfabric for Slingshot/Cassini transport ("libfabric-sys crate needed for Slingshot support"). The argument was implicit: a fabric library at the transport layer doesn't enter the FIPS module boundary; FFI is acceptable when the C code never touches plaintext, keys, or crypto primitives. This ADR generalizes that argument and makes it policy.

### The FIPS argument is about crypto, not about C

ADR-001's load-bearing rationale is the FIPS 140-3 module boundary. FIPS does not care about *bytes moving through transports*; it cares about *operations on cryptographic material*. A C library that parses NFS COMPOUND or dispatches FUSE opcodes is no more in the FIPS boundary than the kernel TCP stack is. Conflating "FIPS scope" with "language scope" is what makes ADR-001 read as more restrictive than its decision actually is.

### ADR-027's costs don't apply to system C libraries

ADR-027 enumerates four concrete costs of multi-language: domain-model drift, error-taxonomy duplication, second FIPS module, second CI lane. Linking `libfuse3.so` adds none of these:

- Domain-model: kiseki domain types (`OrgId`, `NamespaceId`, `CompositionId`, `Composition`) live in `kiseki-common` Rust. libfuse defines its own opcode types; we map them at the FFI boundary, just as we already map gRPC types at the tonic boundary. No duplication of kiseki domain logic.
- Error-taxonomy: libfuse returns `errno` integers; we already do the `errno`↔`GatewayError` mapping at the FUSE handler edge (see existing `crates/kiseki-client/src/fuse_fs.rs`). The taxonomy stays single.
- Second FIPS module: libfuse has no crypto. Same for nfs-ganesha when run with the SELinux confinement of §D7.
- Second CI lane: `libfuse3-dev` is an apt/yum/brew dependency, not a toolchain. CI installs the package; the language we develop in is still Rust.

The argument that ADR-027 has the most teeth on — "kiseki's own code in two languages drifts" — does not apply when the C code is **someone else's project that we depend on but do not maintain**.

### Industry comparison

Production HPC/AI distributed storage at scale falls into three patterns:

| Vendor / project | NFS pattern | FUSE pattern | Custom kernel module? |
|---|---|---|---|
| VAST | Standard kernel NFS client → custom userspace NFS server (proprietary) | Proprietary user-space client lib for AI/ML | No |
| Weka | Custom kernel module is the primary path; NFS as compatibility tier | (kmod replaces FUSE) | **Yes** — primary path |
| Ceph | nfs-ganesha + Ceph FSAL plugin | ceph-fuse via libfuse | Optional `ceph` kmod |
| GlusterFS | nfs-ganesha + Gluster FSAL plugin | gluster-fuse via libfuse | No |
| Lustre | Lustre client kernel module | (kmod replaces FUSE) | **Yes** — primary path |
| BeeGFS | BeeGFS client kernel module | (kmod replaces FUSE) | **Yes** — primary path |
| GPFS / Spectrum Scale | mmfs kernel module | (kmod replaces FUSE) | **Yes** — primary path |
| DAOS | dfuse via libfuse | dfuse via libfuse + libdfs/MPI-IO bypass for HPC apps | No (libfabric instead) |
| JuiceFS | (S3 + libfuse) | libfuse via cgofuse | No |
| sshfs / gcsfuse / gocryptfs / encfs | n/a | libfuse | No |

**Two patterns dominate**: a custom kernel module (Weka/Lustre/GPFS/BeeGFS) for absolute performance, or `nfs-ganesha + FSAL plugin` and `libfuse + binding` for everyone else. Hand-rolling NFS protocol parsing **and** FUSE protocol dispatch in pure Rust matches no production deployment we can find. The "we use system components for protocol mechanics" pattern is what JuiceFS, Ceph, GlusterFS, DAOS, gcsfuse, sshfs, and (for the server side only) VAST share. We are currently in a category of one.

### What stays differentiating in kiseki

Adopting D3 + D4 *narrows* the kiseki Rust surface to what is actually unique to us:

- Chunk-cluster (replication, EC, scrub, healing): unique kiseki design
- Composition store (versioning, deltas, persistent fjall-backed storage per ADR-040): unique
- View consistency + bounded staleness + read-your-writes: unique
- Raft topology + ADR-041 multiplexed transport: unique
- Native gateway data service per ADR-042: unique
- Encryption (ADR-002 two-layer, ADR-011 crypto-shred TTL, ADR-040 hydrator halt mode): unique and FIPS-bound — stays in Rust
- Workflow advisory, audit, retention holds, KMS providers: unique business logic — stays in Rust

What we delegate to system components is the **commodity protocol-mechanics work** — XDR, COMPOUND, FUSE opcode dispatch, session bookkeeping — which is decades-mature and not where kiseki adds value. This is not a retreat; it is focus.

### Why not "just keep going"?

The status-quo cost is sustained debug load on protocol mechanics (cited in Context). Each cluster run surfaces new structural bugs in the hand-rolled NFS server or FUSE daemon. Time spent debugging COMPOUND framing or FUSE opcode 50 is time not spent on chunk-cluster correctness, EC repair, or scrub. The cost is also escalating: ADR-038 (pNFS) added MDS+DS protocol surface, and any future NFS revision (NFSv4.2 extensions, RDMA support, Kerberos integration) compounds the maintenance bill on a per-RFC-section basis. nfs-ganesha already implements the bulk of this catalogue, with conformance-tested plumbing and a community that responds to upstream protocol changes.

## Alternatives

1. **Keep status quo (hand-roll everything in Rust).** Pro: maximum control; pro: ADR-001 untouched; pro: fewer runtime deps. Con: structural bug load, every protocol revision is our problem, perf-cluster runs continue to surface mechanics bugs that have nothing to do with kiseki's differentiation.
2. **Wholesale revise ADR-001 to "Rust where it matters, C where mature."** Too broad. Removes useful constraints (we'd lose the FIPS-boundary discipline). The targeted ADR-043 framing — positive list, FIPS-isolation rule, process-boundary discipline — preserves what ADR-001 was actually protecting (FIPS surface, audit clarity) while permitting what ADR-001 had already half-permitted (libfabric).
3. **Build a custom kernel module** (Weka/Lustre/GPFS pattern). Multi-month development cost. Distro-pinning maintenance burden (kernel ABI changes). Out-of-tree kmod packaging hell. Increases kiseki's surface area, opposite of D9's reversibility. Defer to a future ADR if perf data on Tier_1 + 200 GbE clusters shows we can't reach our targets via libfuse + ganesha + native data service (ADR-042).
4. **Punt POSIX entirely; ship kiseki as S3-only.** Radical scope cut. Loses the HPC/AI POSIX use case in CLAUDE.md's positioning. Doesn't address NFS (still hand-rolled if we keep it). Effectively replaces this ADR's narrow policy change with a strategic product decision that goes well beyond architecture.
5. **Permit FFI policy but only for libfuse, not nfs-ganesha (or vice versa).** Inconsistent. The FIPS-isolation rule (D1) and process-boundary rule (D2) apply identically to both. Permitting one without the other would re-litigate the policy on the next protocol gateway question. The policy should be principled, not piecewise.

## Consequences

### Positive

- `kiseki-gateway`'s NFS surface (XDR, COMPOUND, sessions, HandleRegistry, pNFS MDS code) shrinks to a thin FSAL plugin (~5–10 k lines, vs ~30 k+ in the current crates' NFS-related code). Conformance to RFC 7530/8881/8435 inherits decades of nfs-ganesha + Linux NFS client interop testing.
- `kiseki-client`'s FUSE surface drops the `fuser` 0.17 dependency and its known gaps (FUSE_SYNCFS opcode 50, single-thread inline dispatch). Inherits libfuse 3.x's multi-thread session loop, full opcode coverage, and the writeback-cache mode that mature FUSE filesystems use.
- Bug load on protocol mechanics drops to "issues we file upstream", not "issues we fix in our tree". Engineering time refocuses on chunk-cluster, composition, EC, scrub, view consistency, native gateway perf — kiseki's differentiation.
- ADR-001's libfabric exception is now codified rather than implicit. Future system-library questions are decided against this policy, not by re-reading ADR-001 each time.
- Aligns kiseki with the dominant non-kmod pattern in production HPC/AI storage (Ceph, Gluster, JuiceFS, DAOS, gcsfuse). Reduces the "novel architecture, novel bugs" exposure on protocol surfaces specifically.

### Negative

- New build-time deps: `libfuse3-dev`, `nfs-ganesha-devel` (or equivalent). New runtime deps: `libfuse3.so`, `nfs-ganesha`. Cross-distro packaging matrix grows.
- Linux-only protocol surfaces are reinforced: macOS / Windows clients via libfuse equivalents (`macfuse`, WinFsp) are out of scope under this policy; if cross-platform is later required, that becomes its own ADR with a different positive list.
- FFI/cdylib path for Python and C++ wrappers picks up `libfuse3.so` transitively when `fuse` feature is enabled. Wrappers must document this dep.
- Reversal cost for committed migrations: once ADR-044 (FSAL plugin) lands and the in-tree NFS handlers are removed, reverting requires re-implementing handlers. Mitigated by the parity-with-existing requirement in §D8 (no removal until parity in `@integration`).
- ADR-027's "single language" cognitive-load benefit is partially given up: contributors touching the FUSE adapter learn libfuse's C API; FSAL plugin contributors learn ganesha's plugin shape. The Rust-only invariant for kiseki's *own* code stays.
- SELinux confinement of ganesha (§D7) requires distro-specific module work and is itself maintenance. Without it, the FIPS-isolation argument for ganesha-as-process is "mostly conventional" rather than "OS-enforced."
- Performance characterisation gap: nfs-ganesha + FSAL plugin perf is unknown for kiseki-shaped workloads. Existing nfs-ganesha + Ceph deployments hit ~10–20 GB/s per node; we need to verify on c3-standard-44 / Tier_1 before committing to remove the in-tree NFS code. ADR-044 will own this measurement.

### Operational

- Operators install one extra package (`nfs-ganesha`) when they want NFS access. Existing operators using only S3 / native / FUSE are unaffected.
- Logging integration: ganesha's logs need to be aggregated alongside `kiseki-server` logs; ADR-044 specifies the journald / fluentd / syslog plumbing.
- Metrics integration: ganesha exposes its own Prometheus metrics (via `nfs-ganesha-utils` exporters); kiseki dashboards add panels for ganesha-side counters; ADR-044 covers the dashboard updates.
- Upgrade orchestration: a kiseki version bump may pin a minimum `nfs-ganesha` version. ADR-044 owns the compatibility matrix.

## Open items (escalated to analyst seed + adversary gate-1)

- **A**: Confirm `libfuse3` LGPL-2.1 license is compatible with kiseki's overall license (Rust workspace today is per-crate; verify kiseki-client and any wrappers don't end up in a viral position). nfs-ganesha is LGPL-3 / BSD per-component; FSAL plugin license decision belongs to ADR-044.
- **B**: Verify the FIPS argument concretely: under FIPS 140-3, does merely *transporting* a ciphertext through a non-FIPS C library constitute "use" of the cryptographic boundary? Existing precedent (kernel TCP, libfabric, network switches) says no, but get a written reference from a FIPS evaluator before locking the policy.
- **C**: Verify nfs-ganesha's Linux-mainline NFS client interop matrix at the kiseki versions of NFSv3 / NFSv4.1 / NFSv4.2 / pNFS. Specifically: does ganesha's pNFS MDS path support the layout types ADR-038 commits to (Flex Files mirror lists)?
- **D**: Specify the FSAL plugin language explicitly (C against ganesha's plugin ABI vs. Rust-via-cbindgen). ADR-044 owns this; flagging here so adversary gate-1 considers both options at the policy stage.
- **E**: Performance baseline: target perf-cluster numbers ganesha + FSAL must meet to displace the in-tree NFS code. Suggested floor: parity with current single-host NFS perf at the 2026-05-09 GCP run (pre-perf-fix-sweep numbers in `specs/performance/2026-05-07-gcp-compact-multinode/findings.md`); stretch goal: match or beat the post-fix-sweep numbers (NFSv4 GET 27 291 op/s, p99 4 ms; pNFS GET 16 549 op/s).
- **F**: Cross-platform FUSE policy: does this ADR implicitly retire macOS / Windows POSIX support, or is it silent (left to a later ADR)? Architect proposes silent + later ADR; analyst seed should confirm.
- **G**: Reversibility test: define what evidence would trigger marking the ganesha or libfuse row Rejected (e.g., perf cliff > X%, security finding, upstream community collapse). ADR-044 / ADR-045 should each commit to a "go / no-go" review at a specific milestone (e.g., 6 months of cluster operation).

## References

- ADR-001: Pure Rust, No Mochi Dependency — the libfabric precedent this ADR codifies.
- ADR-013: POSIX Semantics Scope — defines the FUSE-supported operation matrix; unaffected by this ADR (libfuse implementation, same matrix).
- ADR-019: Gateway Deployment Model — defines the gateway-as-binary pattern that an external daemon (nfs-ganesha) joins.
- ADR-023: Protocol RFC Compliance — defines the conformance bar new NFS-protocol-mechanics implementations (whether in-tree or via ganesha) must clear; the rev-2 BDD discipline still applies.
- ADR-027: Single-Language Rust Only — bounded by this ADR's reading: about kiseki's own code, not about system libraries we depend on.
- ADR-038: pNFS layout — constrains §Open-item-C above.
- ADR-040: Persistent metadata stores — composition store stays Rust-only; FSAL plugin reads composition through gRPC, not directly.
- ADR-042: Native Gateway Data Service — independent of this ADR (native clients don't go through ganesha or libfuse).
- `specs/performance/2026-05-07-gcp-compact-multinode/findings.md` — the cluster run that triggered the structural-bug pattern.
- `specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md` — the FUSE_SYNCFS gap on opcode 50; representative of the fuser-rs class of issue.

## Notes for analyst / adversary

This ADR deliberately scopes only **policy**, not implementation. The two follow-up implementation ADRs (FSAL plugin, libfuse binding) own the concrete shape questions: which crate, which plugin language, which migration order, which perf bar.

If the analyst seed surfaces ubiquitous-language additions (e.g. "FSAL", "ganesha sidecar", "libfuse handler thread"), they belong in `specs/ubiquitous-language.md` with this ADR cited as origin. If new invariants emerge (e.g. "the FSAL plugin MUST never receive master-key material"), they belong in `specs/invariants.md` with the same citation.

Adversary gate-1 should specifically attack:
- The FIPS-isolation rule's actual robustness (open-item B above) — does merely transporting ciphertext through C touch the FIPS module?
- The reversibility assumption in D9 — is "remove ganesha, restore in-tree NFS" actually cheap once the in-tree code is deleted?
- The §D7 SELinux confinement — does every supported distro have a working SELinux/AppArmor story we can ship, or is this a Red-Hat-only feature?
- The migration parity bar in D8 — what does "parity at @integration" actually mean operationally; can we game the metric?

If gate-1 surfaces critical/high findings, this ADR moves to **Rejected** or **Revised**, and the implementation ADRs (044, 045) wait.
