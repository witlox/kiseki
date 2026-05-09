# Adversary Gate-1 Findings — ADR-043 System Library FFI and External Daemon Policy

**Type**: Adversary → Architect (gate-1)
**Date**: 2026-05-09
**Reviewer**: adversary (architecture mode)
**Mode**: pre-acceptance review against the architect's draft (`d65564d`).
**Verdict**: **CHANGES REQUESTED** — 3 CRITICAL, 6 HIGH, 9 MEDIUM, 4 LOW. CRITICALS block ADR acceptance; HIGHs should be resolved in the ADR amendment before ADR-044 / ADR-045 (the implementation ADRs) are written, or they will inherit unresolved policy questions.

ADR-043's overall direction is **structurally defensible** — the libfabric-precedent argument in §Rationale holds, the FIPS-isolation rule (D1) is the right shape, and the process-boundary rule (D2) cleanly extends ADR-027 §Enforcement. The structural problems are concentrated in three areas:

1. **The FIPS-isolation rule conflates "doesn't perform crypto" with "is safe."** Plaintext flowing into a non-Rust process is a security concern even when FIPS conformance is preserved. ADR-011's 60-second crypto-shred ceiling and F-CC3's cached-plaintext exposure window do not naturally extend across a process boundary; the ADR is silent on what happens when ganesha holds plaintext during a crypto-shred.
2. **Tenant identity propagation through the FSAL plugin is unspecified.** kiseki-gateway today routes per-tenant on the data path with type-checked `OrgId` / `NamespaceId` end-to-end. ganesha's FSAL ABI passes file handles + RPC credentials, not kiseki tenant types. Lossy translation here is a cross-tenant key-leak class of bug.
3. **Ganesha-as-process is a different reversibility shape than libfuse-FFI.** The ADR's D9 reversibility argument was crafted for the FFI-binding case (replace the `*-sys` crate). For ganesha, "reverse" means "rewrite the in-tree NFS server we removed at parity per D8." That cost is multi-month after the in-tree code is deleted. The two cases need separate reversibility analyses.

The architect's pre-emptive list of open items (A–G in the ADR itself) covers some of these — but several are policy-load-bearing decisions deferred to "gate-1 will tell us," and gate-1 should now decide them rather than punt to ADR-044 / ADR-045.

## Summary

| Severity | Count |
|---|---|
| Critical | 3 |
| High     | 6 |
| Medium   | 9 |
| Low      | 4 |

---

## CRITICAL findings (block ADR acceptance)

### F-C1: D1's FIPS-isolation rule conflates "doesn't enter FIPS module" with "is safe to hold plaintext"

**Severity**: Critical
**Category**: Security > Cryptographic correctness, Trust boundaries
**Location**: ADR-043 §D1; §Rationale "The FIPS argument is about crypto, not about C"
**Spec reference**: ADR-002 (two-layer encryption model), ADR-011 (crypto-shred TTL), F-CC3 (cached plaintext exposure window), I-K3 (crypto-shred propagation)

**Description**: D1 reads "FFI to a system C library is permitted iff the library does not enter the FIPS crypto module boundary." This is correct for FIPS 140-3 conformance — transporting ciphertext or plaintext through a non-FIPS module is not a violation. But the ADR uses the FIPS rule as the *only* gating predicate, treating "non-FIPS" as synonymous with "non-load-bearing for security." It is not.

Concrete example: kiseki-server decrypts a chunk and hands the plaintext to nfs-ganesha over UDS for delivery to the NFS client. Ganesha's address space now contains the plaintext for the duration of the response. Operations on that plaintext:

- Subject to ganesha's threading model (pthreads with shared heap; not bounded by ADR-011's 60s TTL).
- Visible in `/proc/<ganesha-pid>/mem` to any process with `CAP_SYS_PTRACE` or `ptrace_scope <= 1`.
- Persisted in core dumps if ganesha crashes and `kernel.core_pattern` writes them out.
- Logged if ganesha's logging level is set above WARN and a request handler trace-logs payload bytes.
- Reachable from any other FSAL plugin loaded into the same ganesha process (an operator-installed FSAL plugin from another vendor, common in mixed deployments, is now in kiseki's plaintext path).

ADR-011 caps the in-process plaintext window at 60s via the crypto-shred TTL. F-CC3 is the failure mode that motivated that cap. Once plaintext crosses the kiseki-server → ganesha process boundary, neither the TTL nor the broadcast invalidation in ADR-011 §2.b naturally extend — ganesha doesn't subscribe to kiseki's invalidation channel.

**Evidence**: ADR-011 §Mechanism step 2.b: "Invalidation broadcast to all known gateways, stream processors, and native clients for that tenant." Ganesha is not in this list. ADR-043 §D2 names ganesha as a daemon "behind the existing gRPC boundary" but does not require it to subscribe to the crypto-shred broadcast or to bound its in-process plaintext lifetime to ≤ ADR-011's max TTL.

**Suggested resolution**: Add D1.1 "Plaintext exposure rule": any external daemon holding plaintext on behalf of kiseki MUST (a) subscribe to the ADR-011 crypto-shred broadcast over the same gRPC channel and purge in-flight plaintext on receipt; (b) document its plaintext-lifetime bound (response-buffer lifetime ≤ X seconds; X enumerated per daemon); (c) disable core dumps in its systemd unit; (d) restrict log levels so payload bytes are never logged. The ADR's positive-list rows (D4 ganesha) MUST cite the daemon's plaintext-lifetime bound; if the daemon cannot meet (a)–(d), it is forbidden under D1 regardless of FIPS posture.

---

### F-C2: Tenant identity propagation through FSAL plugin is unspecified — cross-tenant leak surface

**Severity**: Critical
**Category**: Security > Tenant isolation, Semantic drift
**Location**: ADR-043 §D4 (ganesha row), §D2 (rule (3) "does not receive raw chunk envelope keys, master-key material, or unencrypted user-data plaintext unless its role is plaintext-handling"), absence of D2 sub-rule on tenant identity
**Spec reference**: I-T1, I-T2 (tenant authentication / authorization invariants), kiseki-common's `OrgId` type, kiseki-gateway's tenant-routing path

**Description**: kiseki-gateway today carries `OrgId` and `NamespaceId` as type-safe Rust types through every read/write call from protocol handler to chunk store. Ganesha's FSAL ABI (`fsal_api.h`) passes:

- An `op_context` containing creds (`creds_t` — a struct with `caller_uid`, `caller_gid`, optional `caller_principal` for Kerberos)
- A `fsal_obj_handle` (an opaque blob the FSAL plugin previously minted)
- The op-specific args (read offset/length, write buffer, etc.)

Translating `caller_uid` / `caller_gid` to `OrgId` / `NamespaceId` requires a mapping that is **not** part of NFS protocol mechanics — it's a kiseki tenancy decision. The ADR delegates this to "ADR-044 owns concrete shape" but D2 rule (3) merely says ganesha can receive plaintext if needed. There is no rule saying:

- The FSAL plugin MUST attach a tenant identity claim to every gRPC call to kiseki-server, signed in such a way that ganesha cannot impersonate a different tenant.
- The FSAL plugin MUST NOT cache plaintext or handles across tenant identities (a single ganesha export shared by tenants A and B has a leak surface if handle minting collides).
- kiseki-server MUST verify tenant identity on every FSAL-originated call rather than trusting the plugin.

Without these rules, ADR-044 will hit a class of bugs where uid-1000 in tenant-A's NFS export resolves to a chunk in tenant-B because the FSAL plugin's handle cache returned a stale entry. This is exactly the cross-tenant data-leak failure mode I-T1 was written to prevent.

**Evidence**: kiseki-gateway's NFSv3 `nfs_ops.rs` derives tenant-id from the namespace bound to the export at mount time (single-tenant-per-export), enforced by namespace-meta lookup. Ganesha's standard deployment pattern is multi-export-per-process; if kiseki adopts the standard pattern without amendment, the cross-tenant boundary previously enforced by per-export gateway processes collapses into per-export FSAL routing inside one ganesha process.

**Suggested resolution**: Add D2 rule (5): External daemons that translate from non-kiseki credentials (uid/gid, RPC creds, X.509 SAN) into kiseki tenant identity MUST do so via a verified mapping, the mapping MUST be signed by `kiseki-control` over gRPC, and `kiseki-server` MUST re-verify the tenant identity claim on every plugin-originated call (no daemon-side trust). ADR-044 inherits this rule; the FSAL plugin must implement it before it can be deployed even in test.

---

### F-C3: Ganesha-as-process implicitly drags MIT krb5 into the FIPS scope when RPCSEC_GSS is enabled

**Severity**: Critical
**Category**: Security > Cryptographic correctness, FIPS scope
**Location**: ADR-043 §D4 (ganesha row), §D5 (forbidden libraries — TLS/crypto stays Rust on aws-lc-rs), no statement on ganesha's optional crypto stack
**Spec reference**: ADR-001 (FIPS module = aws-lc-rs single boundary), ADR-023 + protocol-compliance.md (RPCSEC_GSS marked "❌ not implemented today, N until enterprise"), RFCs 2203 / 5403 / 7204 (RPCSEC_GSS)

**Description**: nfs-ganesha builds with `--with-krb5` by default on Red Hat / Ubuntu / SUSE distros, which links MIT krb5's `libgssapi_krb5` and `libk5crypto` for RPCSEC_GSS_KRB5 mechanisms (RFC 2203). MIT krb5's crypto code path performs AES-CTS, HMAC-SHA1, RC4-HMAC-MD5 (legacy), Camellia, etc. — all primitives FIPS 140-3 cares about. Pulling ganesha into kiseki's deployment without explicitly disabling Kerberos at build time would silently introduce a second crypto module into the boundary, directly violating D5 ("Crypto primitives ... MUST stay in Rust on aws-lc-rs").

Today this is dormant because RPCSEC_GSS is `❌ not implemented` per protocol-compliance.md. The moment kiseki enables it (ADR-023 lists it as "N until enterprise tenants need Kerberos" — i.e. on the roadmap), the contradiction becomes load-bearing.

**Evidence**: nfs-ganesha's `configure.ac` defaults: `enable_krb5=yes` if `pkg-config --exists krb5-gssapi`. Default Red Hat / Ubuntu builds ship with this on. Per the FIPS 140-3 IG (Implementation Guidance), §A.5: any cryptographic library linked into a process within the cryptographic module boundary becomes part of the certification scope.

**Suggested resolution**: Add D5 sub-rule: "External daemons under D2 MUST be built with all crypto submodules disabled (e.g., for nfs-ganesha: `--disable-krb5 --disable-rdma-crypto`); kiseki packages a kiseki-specific build of ganesha (or pins to a distro package whose options exclude krb5)." Add D4 row column "build flags": for ganesha, `--disable-krb5 --without-gssapi --disable-tls`. If RPCSEC_GSS is later required, it is implemented in kiseki-server (Rust on aws-lc-rs) and exposed through the FSAL plugin via a callback into kiseki — the C krb5 implementation is forbidden under D5. ADR-044 inherits this constraint.

---

## HIGH findings (resolve in ADR amendment before ADR-044/045)

### F-H1: D9 reversibility argument doesn't apply to ganesha-as-process

**Severity**: High
**Category**: Correctness > Implicit coupling, Robustness > Failure cascades
**Location**: ADR-043 §D9 ("Reversibility"), §D8 ("Migration is per-component and ADR-gated"), §Rationale "Reversibility test"

**Description**: D9 enumerates three reversibility mechanisms:

1. "All FFI lives in dedicated `*-sys` / `kiseki-*` crates (D6), which can be replaced wholesale."
2. "All external daemons are behind gRPC boundaries (D2), so swapping `nfs-ganesha` for an in-tree Rust NFS gateway is a config and binary change, not a contract change."
3. "The pre-existing pure-Rust paths remain in CI until D8's migration ADRs land replacements at parity."

Mechanism (1) applies to libfuse but not to ganesha (there is no FFI). Mechanism (3) is correct only **until D8's parity bar is reached and the in-tree NFS code is removed**, which D8 explicitly intends. After removal, mechanism (2) is the only reversibility lever — and "swap ganesha for a Rust NFS gateway" reduces to "rewrite the NFS gateway you just deleted."

D9 reads as if the reversibility cost is symmetric across the libfuse and ganesha cases. It is not. The ganesha case has a one-way ratchet at the moment in-tree code is deleted (D8 step 4: "Once stable, remove crates/kiseki-gateway's NFS server code").

**Suggested resolution**: Split D9 into D9.1 (libfuse-class FFI; reversibility cost = swap `*-sys` crate, weeks) and D9.2 (external-daemon-class; reversibility cost = re-implement gateway, multi-month — and must therefore meet a higher acceptance bar before in-tree code is deleted in D8). ADR-044 must define, before any in-tree NFS code is removed, a rollback procedure that does NOT require re-implementation. Candidates: keep the in-tree code in tree but unwired (not removed); ship both code paths under a feature flag for one full release cycle; ensure the FSAL plugin can be stopped and the in-tree gateway re-enabled by config change alone.

---

### F-H2: Plaintext exposure ≠ FIPS conformance — D1 needs a security predicate beyond FIPS

**Severity**: High
**Category**: Security > Trust boundaries
**Location**: ADR-043 §D1, §Rationale "The FIPS argument is about crypto, not about C"
**Spec reference**: ADR-002, ADR-011, F-CC3

**Description**: The ADR uses "doesn't enter the FIPS module" as the sole permitted-or-not predicate for system C libraries. FIPS conformance is necessary but not sufficient for security — see F-C1 for the plaintext-exposure variant. Beyond plaintext: a non-FIPS C library that mishandles request parsing (buffer overflow → RCE) is a security problem even when FIPS is satisfied. ADR-043 should state a second predicate: "the library has a credible security-response posture" (active CVE handling, recent releases, supported by upstream).

For libfuse and nfs-ganesha this is met; for hypothetical future entries on the positive list (D3/D4), the policy should require evidence.

**Suggested resolution**: Add D1.2 "Security-posture rule": every entry on the D3/D4 positive list MUST, at the time of addition, name (a) the upstream's security-issue handling history, (b) the most recent CVE and time-to-patch, (c) the kiseki team's stated triage SLA for upstream advisories. Codify ganesha's and libfuse's entries with this data filled in.

---

### F-H3: SELinux confinement (D7) is presented as enforceable but distro support is unspecified

**Severity**: High
**Category**: Robustness > Observability gaps, Operational
**Location**: ADR-043 §D7

**Description**: D7 reads "kiseki ships a SELinux module (or AppArmor profile, whichever the distro supports) that confines `ganesha.nfsd`." The ADR doesn't specify:

- Which distros are in scope (Red Hat / Ubuntu / SUSE / Debian / Alpine?).
- What the fallback is when neither SELinux nor AppArmor is enforcing on the host (containerized deployments commonly run in unconfined modes).
- How kiseki-server **detects** the absence of confinement at runtime and either refuses to start, downgrades capability, or warns visibly. Without runtime detection, an operator who disables the SELinux module silently weakens FIPS isolation.
- Whether D7 is enforced by kiseki-server's own startup checks (probe `ls -Z /proc/<ganesha-pid>` for the expected context) or trusted to operator discipline.

The ADR's claim that confinement makes "the FIPS module boundary enforceable at the OS level, not just at the source-code level" depends entirely on this enforcement chain working in practice.

**Suggested resolution**: Add D7.1 "Runtime confinement check": at kiseki-server startup with the `nfs-via-ganesha` feature enabled, kiseki-server MUST query the OS for the ganesha process's confinement label and refuse to forward FSAL traffic if the expected label is absent. List supported confinement backends (SELinux RHEL ≥ 8, AppArmor Ubuntu ≥ 22.04, containerized seccomp+namespaces fallback) and what each enforces concretely. ADR-044 owns the implementation; this ADR should pin the policy.

---

### F-H4: Open item B (FIPS evaluator written reference) is a load-bearing gating dependency, not a follow-up

**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §Open items B

**Description**: B says: "Verify the FIPS argument concretely: under FIPS 140-3, does merely *transporting* a ciphertext through a non-FIPS C library constitute 'use' of the cryptographic boundary? Existing precedent (kernel TCP, libfabric, network switches) says no, but get a written reference from a FIPS evaluator before locking the policy."

This is the ADR's central argument for why D1 is sound. If the answer comes back "ambiguous" or "requires module-by-module review," D1 needs a stricter formulation. Treating it as a routine open item — to be resolved later by analyst or implementer — defers the question past acceptance, after which removing rows from D3/D4 is significantly more disruptive.

**Suggested resolution**: B is reclassified as a **gating prerequisite**: ADR-043 cannot move from Proposed to Accepted until a written reference is on file. The reference should specifically cite FIPS 140-3 Implementation Guidance §A.5 (cryptographic boundary scope) and §C.G (loadable libraries). Until the reference is filed, the ADR's status remains Proposed and ADR-044/045 are blocked from drafting.

---

### F-H5: ADR-038 pNFS interop with ganesha is unverified and may block the migration

**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §D4 (ganesha row), §Open item C, §References (ADR-038)
**Spec reference**: ADR-038 (pNFS layout, Flex Files mirror lists), ADR-039 (Flex Files mirror list encoding), nfs-ganesha's `FSAL_PNFS` MDS surface

**Description**: ADR-038 commits kiseki to pNFS with Flex Files layout type (RFC 8435), with mirror-list encoding per ADR-039. nfs-ganesha's pNFS MDS path supports several layout types (NFS4_FILE, NFS4_BLOCK, NFS4_FLEX_FILES) but the FSAL plugin has to surface the layout-get / layout-commit / layout-return / get-deviceinfo callbacks the layout type requires. For Flex Files specifically, the FSAL_PNFS_FLEXFILES contract requires the plugin to expose multi-replica data-server identities + per-DS credentials. Whether kiseki's existing chunk-cluster (ADR-040 + ADR-041) topology can be mapped onto Flex Files DS identities cleanly is unverified.

If ganesha + FSAL plugin cannot serve Flex Files at the protocol fidelity ADR-038 requires, the migration cannot replace the in-tree pNFS MDS code. Either ganesha's pNFS support is an addressable gap (file an upstream feature request), or kiseki keeps the in-tree pNFS code (NFS-via-ganesha for v3/v4.1/v4.2 only; pNFS stays in-tree). Either way, ADR-043's D4 ganesha row needs a sub-row for "pNFS MDS scope" that is honest about coverage.

**Suggested resolution**: Add D4 column "scope coverage": for ganesha, "NFSv3 + NFSv4.1 + NFSv4.2; pNFS MDS deferred until FSAL_PNFS_FLEXFILES coverage is verified." ADR-044 owns the verification step (a small probe deployment that exercises the existing pNFS BDD scenarios against ganesha + a stub FSAL); only on verified coverage may pNFS MDS migrate. Without this scoping, ADR-044 inherits an unbounded research task.

---

### F-H6: Performance regression from the new gRPC boundary on every NFS op is unmeasured

**Severity**: High
**Category**: Robustness > Failure cascades, Correctness > Implicit coupling
**Location**: ADR-043 §Open item E, §Negative consequences "Performance characterisation gap"
**Spec reference**: 2026-05-09 GCP perf table (NFSv4 GET 27 291 op/s, p99 4 ms post-perf-fix-sweep)

**Description**: Today's path: NFS client → in-tree kiseki-gateway NFS handler → mem_gateway::read (in-process call) → chunk store. Post-migration: NFS client → ganesha (process A) → FSAL plugin (in-A) → gRPC over UDS → kiseki-server (process B) → mem_gateway::read → chunk store. Every NFS read/write op now crosses an extra UDS gRPC round-trip.

UDS gRPC adds ~10–20 µs per call on Linux at small-message sizes. At 27 k op/s, that's ~50% of the current 4 ms p99 if the regression is per-op. The post-perf-fix-sweep numbers in `specs/performance/` were achieved by aggressive in-process optimization (read decrypt cache, fast-path composition lookup, persistent fjall stores per ADR-040). Adding a process boundary on the hot path is a structurally different shape; a 5–20% regression is plausible, a 50% regression is plausible if the round-trip happens per-op rather than per-batch.

ADR-043 lists "Performance characterisation gap" as a Negative consequence and defers the measurement to Open item E + ADR-044. But performance is **strategic** per the project's positioning ("HPC/AI workloads") — Open item E should set a hard floor below which the migration is rejected, not a soft "gather data and decide later."

**Suggested resolution**: Add to Open item E: "ADR-044 MUST establish a perf-floor commitment before in-tree code is removed (per F-H1). Suggested floor: 90% of post-perf-fix-sweep numbers on c3-standard-44 / Tier_1 (e.g., ≥ 24 562 op/s NFSv4 GET, p99 ≤ 5 ms; ≥ 14 894 op/s pNFS GET). If the actual measurement misses the floor, the migration moves from D8 'land at parity' to D8 'rejected' or 'partial' (e.g., FUSE migration proceeds; NFS migration paused pending an optimization pass)."

---

## MEDIUM findings

### F-M1: License interaction is "open" but is policy-load-bearing

**Severity**: Medium
**Category**: Correctness > Specification compliance, Operational
**Location**: ADR-043 §Open item A

**Description**: Open item A flags LGPL-2.1 (libfuse) and LGPL-3 / BSD (ganesha) license review but doesn't close the question. The kiseki-client cdylib path (`crate-type = ["lib", "cdylib"]` in `crates/kiseki-client/Cargo.toml`) compiled with `--features fuse` will transitively depend on `libfuse3.so`. LGPL-2.1's dynamic-linking exception covers this for libfuse, but kiseki's downstream wrappers (Python via PyO3, C++ wrapper) need to honor LGPL re-distribution requirements. If kiseki's overall license is more permissive, the wrappers inherit LGPL-2.1 constraints on the FUSE feature.

For ganesha as an external process, LGPL-3 doesn't reach into kiseki's source (process boundary, not link boundary). But the FSAL plugin (`libfsalkiseki.so`) is loaded into ganesha at runtime. The plugin's license must be LGPL-3-compatible (BSD, MIT, Apache-2.0 with explicit AGPL-compatibility, or LGPL-3 itself). If the plugin is GPL-3, the combined work is GPL-3 with linking exception handled by ganesha's LGPL-3 form.

**Suggested resolution**: Close A in this ADR (or in ADR-044/045) with the explicit license decisions: kiseki-client `fuse` feature → kiseki-overall license + LGPL-2.1 dynamic-linking compatibility statement; FSAL plugin → BSD-3 or LGPL-3 (matching ganesha). Document the constraint that downstream wrappers (Python, C++) inherit LGPL-2.1 dynamic-linking obligations on the FUSE path.

---

### F-M2: Multi-tenancy deployment shape for ganesha is unspecified

**Severity**: Medium
**Category**: Correctness > Implicit coupling, Operational
**Location**: ADR-043 §D4, §D7

**Description**: One ganesha process per cluster? Per tenant? Per export? Per shard? The implications differ:

- One per cluster: simplest, but cross-tenant boundary collapses (F-C2).
- One per tenant: tenant isolation by process boundary, but operationally heavy at 100+ tenants and complicates SELinux confinement (one policy per tenant?).
- One per export: matches typical multi-export ganesha deployments, but requires kiseki to materialize exports as ganesha config entries — non-trivial sync between kiseki tenancy and ganesha config.

ADR-044 will hit this question on day 1.

**Suggested resolution**: ADR-043 picks one as the recommended pattern (architect's recommendation: one ganesha process per kiseki-server, with one export per namespace; the process boundary plus FSAL-routed tenant identity is the isolation, not separate processes per tenant). ADR-044 can revise but defaults to the pattern this ADR recommends.

---

### F-M3: Ganesha and libfuse CVE response posture is not committed

**Severity**: Medium
**Category**: Robustness > Observability gaps, Operational
**Location**: ADR-043 §Negative consequences, no §Operational policy on upstream advisories

**Description**: Adopting ganesha makes its CVE history kiseki's CVE history. ganesha has had remote-exploitable bugs in its NLM and NFSv4 paths in the last 5 years (CVE-2022-46343, CVE-2020-10739, etc.). libfuse has had less-severe issues. The ADR doesn't specify:

- Who watches upstream advisories (ops? security? a kiseki engineer named in CLAUDE.md?).
- What the SLA is for landing a CVE patch (urgent / weekly / next-release).
- How an emergency patch is tested + shipped without breaking the BDD acceptance suite.

**Suggested resolution**: Add to §Operational: "Upstream advisory triage: kiseki-security (or named contact) subscribes to nfs-ganesha-announce, libfuse-announce, and CVE feeds. Triage SLA: CRITICAL within 7 days, HIGH within 30 days, MEDIUM at next release. Emergency-patch path: a `kiseki-server`-pinned ganesha version is bumped, BDD suite is re-run against the new build, and a hotfix release ships within the SLA."

---

### F-M4: BDD acceptance test relocation cost is missing from D8

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §D8, §Negative consequences

**Description**: kiseki's NFS BDD acceptance scenarios (~321 scenarios per CLAUDE.md, including the cross-stream and EC scenarios) drive against the in-tree NFS server via ClusterHarness. After D8's migration, the harness must spin up ganesha + FSAL plugin + kiseki-server. The CI runners need:

- `nfs-ganesha` package installed (apt/yum) or compiled from source.
- The kiseki-FSAL plugin built and available at the path ganesha expects.
- A working SELinux policy or unconfined-with-warning fallback.
- Per-test-suite ganesha config generation.

This is a non-trivial CI cost not enumerated in §Negative consequences. The cost compounds if BDD tests run in parallel (multiple ganesha processes per CI worker → port collisions, mount-point collisions).

**Suggested resolution**: Add to §Negative consequences: "BDD acceptance test infrastructure must be migrated alongside the production code path. ADR-044 owns the test-harness migration and budgets the CI cost." Acknowledge that during the migration window CI runs both paths (twice the cost) until in-tree code is retired.

---

### F-M5: Cross-platform FUSE policy is silently dropped

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §D6 ("Cross-platform: `libfuse3` is Linux-only"), §Open item F

**Description**: kiseki-client's `fuser` 0.17 dependency is currently Linux-only by default but the crate compiles on macOS via `osxfuse`/`macfuse`. ADR-013 (POSIX semantics) does not pin a target platform — it specifies the operation matrix without saying "Linux only." The Python and C++ wrappers (cdylib) target broad portability.

D6 effectively retires macOS / Windows POSIX support without an explicit decision. Open item F asks the question but defers it. Silent retirement of supported platforms (without a platform-deprecation ADR) leaves users guessing.

**Suggested resolution**: Architect explicitly states: "macOS and Windows FUSE are out of scope under this ADR. If a future requirement re-introduces them, that ADR adds rows to D3 (`macfuse`-sys, WinFsp-sys) under the same FIPS-isolation rule." Add an entry in §Negative consequences: "macOS / Windows FUSE is out of scope; downstream wrappers building for those platforms must disable the `fuse` feature."

---

### F-M6: Ganesha process supervision model is unspecified

**Severity**: Medium
**Category**: Robustness > Failure cascades, Operational
**Location**: ADR-043 §D4, §Operational consequences

**Description**: How is ganesha started, monitored, restarted? systemd unit shipped by kiseki? Sidecar container with the kiseki-server image? Kubernetes pod with kiseki-server and ganesha as separate containers?

Each shape has consequences:

- systemd: simplest, requires kiseki to install and own a unit file; ganesha lifecycle is not part of `kiseki-server`'s lifecycle, so a kiseki upgrade doesn't atomically upgrade ganesha.
- Sidecar container: lifecycle coupled with kiseki-server, but kiseki now owns a more complex container image; SELinux confinement needs container-aware shape.
- Separate K8s pod: most cloud-native, but UDS-based gRPC between kiseki-server and ganesha is replaced with TCP; latency budget per F-H6 changes.

ADR-019 covers gateway deployment but doesn't anticipate an external daemon.

**Suggested resolution**: ADR-043 picks a default deployment shape; ADR-044 can adjust. Architect's suggestion: kiseki ships a `kiseki-nfs.service` systemd unit that starts ganesha with the kiseki-FSAL plugin + the kiseki SELinux module, with `Requires=kiseki-server.service` so ganesha doesn't start until kiseki-server is healthy. K8s deployments use a sidecar container in the same pod, with shared UDS volume.

---

### F-M7: Side-channel exposure (ptrace, /proc/PID/mem, core dumps) is not in §D7

**Severity**: Medium
**Category**: Security > Trust boundaries
**Location**: ADR-043 §D7

**Description**: D7 confines ganesha "with a sandbox profile that prevents loading aws-lc-rs at runtime." That blocks one threat (loading kiseki crypto into ganesha) but does not block:

- `ptrace` from a co-located unconfined process (operator with sudo, or a sibling daemon misconfigured)
- Reading `/proc/<ganesha-pid>/mem` (same threat surface)
- Core dumps to disk on crash (`kernel.core_pattern`)
- ganesha's own log files capturing payload bytes if log level is set above WARN

Each of these can leak the plaintext F-C1 cited.

**Suggested resolution**: Expand D7 to require: `PrCtl=PR_SET_DUMPABLE 0` to disable core dumps; SELinux/AppArmor policy that denies `ptrace` from unconfined sources; ganesha config pinned to log level `WARN` or below for payload-touching code paths. ADR-044 implements these in the systemd unit + SELinux module.

---

### F-M8: The fuser-PR pathway is missing from formal Alternatives

**Severity**: Medium
**Category**: Correctness > Missing negatives
**Location**: ADR-043 §Alternatives (lists 5 options; missing the "PR upstream to fuser" option)
**Spec reference**: 2026-05-09 sync-kills-daemon investigation finding (FUSE_SYNCFS gap on opcode 50)

**Description**: The conversation thread that triggered ADR-043 considered an explicit alternative: file a PR to fuser 0.18+ adding FUSE_SYNCFS. The ADR's Alternatives section lists 5 options (status quo, wholesale ADR-001 revise, kernel module, S3-only, piecemeal-permit-just-one) but does not list "fix the fuser-rs gaps and stay on fuser." This is a real alternative — fuser is open-source, the gap is small per opcode, and the upstream maintainer is responsive.

Excluding it from Alternatives weakens the ADR's argument. The architect should either argue why "PR upstream to fuser" is insufficient (e.g., even with PRs landing, fuser-rs's single-thread inline dispatch limitation is a structural gap that won't be fixed quickly), or list it as a serious option that's been considered and rejected.

**Suggested resolution**: Add Alternative 6: "Fix fuser-rs upstream (PR FUSE_SYNCFS, PR multi-thread dispatch, etc.)." Acknowledge the smaller cost. Reject on: (a) cumulative upstream-maintenance burden if multiple gaps surface; (b) fuser-rs's smaller maintainer pool vs libfuse's; (c) the multi-thread inline dispatch is documented as a known limitation that has been open for >2 years.

---

### F-M9: D6's `*-sys` crate convention is not enforceable by the existing tooling

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §D6

**Description**: D6 says "System library FFI bindings live in dedicated `*-sys` crates following the `libfabric-sys` precedent." This is a convention; cargo-deny does not natively reject FFI in non-`*-sys` crates. ADR-027 §Enforcement points 1 mentions cargo-deny for crate-graph rules; D6 inherits the same enforceability question but doesn't mention it.

**Suggested resolution**: Add D6.1 "Enforcement": a workspace lint (custom or `cargo-deny`'s `bans` section) rejects crates that declare a `links =` attribute in Cargo.toml unless the crate name ends with `-sys`. This makes the convention machine-checked rather than convention-only.

---

## LOW findings

### F-L1: Industry comparison table missing one row (Hammerspace / DataStream)

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §Rationale "Industry comparison"

**Description**: The table is comprehensive but omits Hammerspace, which uses Linux-mainline `nfsd` (kernel server) plus their own metadata-tier — a fourth pattern not represented. Doesn't change the conclusion (we are still a category of one for hand-rolled NFS server in Rust), but completeness matters in a load-bearing rationale.

**Suggested resolution**: Add Hammerspace + DataStream / Versity rows. Or scope the table to "open-source / community-known patterns" and note that proprietary closed-source vendors are surveyed via published architecture docs.

---

### F-L2: D9 reversibility doesn't define the decision-maker

**Severity**: Low
**Category**: Correctness > Implicit coupling, Operational
**Location**: ADR-043 §D9, §Open item G

**Description**: G says "Reversibility test: define what evidence would trigger marking the ganesha or libfuse row Rejected." Doesn't say *who* makes the call — architect alone? security review? operator vote? PR-mediated discussion with the maintainers?

**Suggested resolution**: Architect names the trigger + decision-maker: "If a kiseki-internal incident traces to a ganesha bug whose CVSS ≥ 7.0 and which upstream cannot patch within the F-M3 SLA, the architect (in consultation with security) may mark the row Rejected and trigger an emergency revert per F-H1's rollback procedure."

---

### F-L3: "Category of one" claim is rhetorical without code-level citations

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §Rationale "Industry comparison" + "What stays differentiating in kiseki"

**Description**: The argument hinges on the claim that no production HPC/AI distributed storage system implements both NFS-server-side protocol parsing AND FUSE-protocol-dispatch in pure Rust. This is plausible but unsupported by direct citation. A skeptical reader could push back: "did you check Crucible, Antithesis, OpenZFS, MinIO Direct-CSI, ...?"

**Suggested resolution**: Either tighten with citations (specifically: which open-source project's repo + which file demonstrates each row in the table — e.g., "Ceph: src/rgw/ for S3, ceph-fsal-ganesha for NFS, src/client/fuse_ll.cc on libfuse for FUSE"), or downgrade the rhetorical "category of one" claim to "we found no comparable example in our survey of open-source HPC storage projects (X, Y, Z)."

---

### F-L4: ADR-042 cross-reference is single-direction

**Severity**: Low
**Category**: Correctness > Implicit coupling
**Location**: ADR-043 §References (ADR-042 listed as "independent of this ADR")

**Description**: ADR-042 (native gateway data service) is independent of ADR-043 in the sense that native clients don't go through ganesha or libfuse. But ADR-042's libfabric / ibverbs bindings (§3.2) are exactly the FFI shape ADR-043 generalizes from. The two ADRs should reference each other consistently — ADR-042 should add a forward note: "FFI-binding precedent codified in ADR-043."

**Suggested resolution**: At ADR-043 acceptance, amend ADR-042 §References to point to ADR-043 as the codified policy.

---

## Cross-cutting concerns

### CC1: Audit chain integrity across the process boundary

ADR-043 doesn't address how kiseki-audit's tamper-evident log (per ADR-009 audit log sharding) handles events generated inside ganesha. Per I-A1..A3, every audited operation must be appended to the chain with a verifiable hash. NFS operations served by ganesha + FSAL plugin generate two audit signals: ganesha's own log (free-form, not hash-chained) and the kiseki-server-side audit on the FSAL-call. Without a rule, the audit invariant "every NFS op is in the chain" weakens to "every kiseki-side FSAL call is in the chain" — which may be fine if FSAL is the right level of granularity, but ADR-043 should state it explicitly.

**Resolution**: ADR-044 owns the audit-integration story; ADR-043 should add a one-line cross-cutting note that audit is NOT delegated to ganesha — kiseki-server records every FSAL-originated call into the audit chain.

### CC2: Observability — metrics from ganesha vs kiseki-server are siloed

ganesha exposes Prometheus metrics via its own exporter; kiseki-server has its own Prometheus surface. Operators correlating an NFS client error to a kiseki composition store hiccup need to join across two metric namespaces. Not blocking, but a real day-2 cost.

**Resolution**: ADR-044 owns metric integration (relabel ganesha metrics into kiseki's namespace? scrape both into the same Prometheus job?). ADR-043 should note this as a known integration cost.

### CC3: Schema versioning of the FSAL gRPC contract

The FSAL plugin's gRPC service definition will live in `kiseki-proto`. Like any gRPC service that crosses process boundaries with independent release cadences (kiseki-server release N can talk to FSAL plugin release N-1 or N+1 within a window), it needs a versioning policy per ADR-004 (schema versioning + upgrade). ADR-043 doesn't mention this; ADR-044 will hit it on day 1.

**Resolution**: ADR-044 owns the schema-versioning policy and inherits the ADR-004 patterns. ADR-043 should add a one-line note in §References that ADR-004 governs the FSAL gRPC schema's evolution.

---

## Verdict

ADR-043's overall direction is **structurally sound** and the "library policy + process-daemon policy" framing is a real improvement over the implicit reading of ADR-001. **CHANGES REQUESTED**, not REJECTED.

Highest-risk areas, in order:
1. **F-C1** (plaintext exposure expansion): security predicate beyond FIPS conformance is missing — must be added before D1 is sound.
2. **F-C2** (tenant identity propagation): cross-tenant leak surface in ganesha-as-multi-export deployment.
3. **F-C3** (ganesha + krb5 implicit FIPS scope leak): the build-flag pin must be in the ADR, not deferred.
4. **F-H4** (FIPS evaluator written reference): policy is conditional on a question that hasn't been answered.

What blocks ADR-044 / ADR-045 from drafting:
- All 3 CRITICAL findings resolved in the ADR amendment.
- F-H1 (reversibility split), F-H4 (FIPS reference filed), F-H5 (pNFS scope) resolved or scoped explicitly in ADR-043's amendment.

Other HIGHs and MEDIUMs may be inherited by ADR-044 / ADR-045 with explicit cross-references; LOWs may be deferred. Cross-cutting concerns (CC1–CC3) need one-line notes in the ADR amendment.

Estimated amendment size: ~50–80 lines of additions to ADR-043 covering D1.1, D1.2, D5 sub-rule, D7.1, D9.1+D9.2 split, Open item E perf-floor, Open item B reclassification, Alternative 6 (fuser PR), Industry-comparison citations, three cross-cutting notes. Architect-only round; no need for a second analyst pass.
