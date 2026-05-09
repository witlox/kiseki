# Adversary Gate-1 Findings — libfuse-swap implementation plan

**Type**: Adversary → Architect (gate-1 on plan, per ADR-043 §D6 review-discipline)
**Date**: 2026-05-09
**Reviewer**: adversary (architecture mode + impl-aware)
**Mode**: pre-implementer-phase-0 review against `7066574` (`specs/implementation/libfuse-swap.md` + ADR-043 rev 3).
**Verdict**: **CHANGES REQUESTED** — 0 CRITICAL, 5 HIGH, 8 MEDIUM, 4 LOW + 2 cross-cutting. Two of the HIGHs flag that the architect's §D6 checklist answers (every criterion **no**) are debatable; per §D6, "if any answer is plausibly **yes** but was answered **no**, the adversary requires a per-binding ADR before implementer phase 0." This review concludes one of the two genuinely qualifies and recommends a small ADR-044 OR an explicit in-plan justification for the **no** answer.

The plan's overall shape is **structurally sound**: the crate split, migration sequencing, cfg-flag rollback mechanism, go/no-go criteria, and rollback table are all the right shape. Issues are concentrated in:

1. **§D6 checklist verification** (F3-H1, F3-H2): the plan introduces a license-class change and ≥ 6 new wrapper-level invariants. The architect answered "no" to criteria 4 and 6 of §D6, but the evidence in the plan itself supports "yes."
2. **Trait surface coverage vs ADR-013**: the listed surface is partial; ADR-013-supported ops (xattr quartet, setattr / chmod / chown / truncate, symlinks, file locks) are not in the plan's listing and the plan's "default impls return ENOSYS" wording would silently regress POSIX scope.
3. **Async-bridge mechanics underspecified** (F3-H4): the safety contract names "tokio::sync::oneshot-bridged adapter" but doesn't specify cancellation-window semantics, max-pending-ops cap, or what happens on libfuse session crash.

---

## Summary

| Severity | Count |
|---|---|
| Critical | 0 |
| High     | 5 |
| Medium   | 8 |
| Low      | 4 |

---

## HIGH findings

### F3-H1: §D6 checklist criterion 4 (license change) is plausibly yes — was answered no

**Severity**: High
**Category**: Correctness > Specification compliance, Operational
**Location**: ADR-043 §D6 (architect's checklist answers); libfuse-swap.md §"Cross-references"

**Description**: §D6 criterion 4 asks: "Does the binding's license materially change downstream distribution shape (transitions across permissive ↔ LGPL ↔ copyleft; or wrapper LGPL exposure that didn't exist before)?"

The plan replaces `fuser` 0.17 (MIT OR Apache-2.0; permissive) with libfuse 3.x (LGPL-2.1; copyleft-with-dynamic-linking-exception). This **is** a permissive ↔ LGPL transition. The dynamic-linking exception softens it (kiseki-client doesn't become GPL/LGPL), but downstream wrappers (Python via PyO3, C++) must honor LGPL-2.1's source-availability requirement for the kiseki-client object code on the FUSE feature path. The plan itself acknowledges this in §"Negative consequences" of ADR-043 rev 3 ("LGPL-2.1 dynamic-linking obligations propagate to the kiseki-client cdylib's `fuse` feature builds") and assigns the explicit license decision as Open item A.

If the architect's answer to criterion 4 is **no**, that is contradicted by ADR-043's own §"Negative consequences" naming the LGPL obligation. If the answer is **yes**, §D6 says a per-binding ADR is required.

**Suggested resolution**: Either (a) escalate to ADR-044 with the license decision as the architectural content (a small ADR — ~30-50 lines — that names the LGPL-2.1 dynamic-linking decision, the wrappers/README disclosure, the CONTRIBUTING.md note, and the per-distro packaging implications); OR (b) add an explicit paragraph to libfuse-swap.md §"Why this binding (not the others)" that walks through criterion 4 in detail and concludes that the dynamic-linking exception keeps kiseki-client itself permissive, the wrapper exposure is documented in wrappers/README rather than enforced in code, and therefore the answer is **no** *despite* the surface evidence. (b) is simpler; (a) is more defensive. Architect picks.

---

### F3-H2: §D6 checklist criterion 6 (new invariants/failure modes) is plausibly yes — was answered no

**Severity**: High
**Category**: Correctness > Specification compliance, Robustness > Failure cascades
**Location**: ADR-043 §D6 (architect's checklist answers); libfuse-swap.md §"Safety contract"
**Spec reference**: `specs/invariants.md` (140 invariants), `specs/failure-modes.md` (P0-P3 catalogue)

**Description**: §D6 criterion 6 asks: "Does the binding add a new failure mode (per `specs/failure-modes.md`) or a new invariant (per `specs/invariants.md`)?"

The plan's §"Safety contract" defines six explicit rules that the wrapper enforces:

1. Reply tokens are consume-once.
2. Drop-without-consume = EIO + leak warning.
3. Reply tokens cross async boundaries through an explicit bridge (oneshot).
4. Cancellation produces EINTR, not leaks.
5. Session loop runs on a dedicated thread.
6. `Filesystem: Send + Sync + 'static`.

These are de facto **new invariants** on the wrapper crate. They are not in `specs/invariants.md` today. The plan also implicitly introduces failure modes:

- F-FUSE-1 (suggested): drop-without-consume → EIO returned to kernel + slot leaked-and-logged.
- F-FUSE-2: async cancellation mid-handler → EINTR returned to kernel.
- F-FUSE-3: libfuse session-thread panic → kiseki-client process state inconsistent (UNHANDLED — see F3-H5 below).

These are not in `specs/failure-modes.md` today.

If the answer to criterion 6 is **no**, that is contradicted by the plan's own §"Safety contract" enumerating six rules. If the answer is **yes**, §D6 says a per-binding ADR is required, OR the invariants/failure-modes get promoted to the project-level catalogues with this plan as the citation.

**Suggested resolution**: Promote the six §"Safety contract" rules to `specs/invariants.md` as I-FUSE-1 through I-FUSE-6 (or whichever numbering the catalogue uses) with citation back to libfuse-swap.md §"Safety contract." Add F-FUSE-1, F-FUSE-2, F-FUSE-3 (and the F3-H5 session-crash mode) to `specs/failure-modes.md`. Update the §D6 checklist answer for criterion 6 to **yes** with the resolution that the invariants are added to the catalogue (avoiding a per-binding ADR by promoting the work directly into the project-level specs).

This is the more durable fix: invariants belong in `specs/invariants.md`, not in an implementation plan that may be archived later.

---

### F3-H3: Trait surface in §"Crate layout" omits ADR-013-supported operations — silent POSIX regression

**Severity**: High
**Category**: Correctness > Specification compliance, Missing negatives
**Location**: libfuse-swap.md §"Crate layout" (the `Filesystem` trait listing); `specs/architecture/adr/013-posix-semantics-scope.md`

**Description**: The plan lists 17 methods on the `Filesystem` trait (lookup, getattr, read, write, create, unlink, flush, release, fsync, syncfs, mkdir, rmdir, rename, opendir, readdir, releasedir, statfs) and adds "default impls return ENOSYS for unimplemented ops; libfuse handles the rest."

ADR-013 §"Supported (full semantics)" requires:

- chmod, chown (FUSE `setattr` op)
- truncate, ftruncate (FUSE `setattr` op with size attr)
- extended attributes (xattr) — getxattr, setxattr, listxattr, removexattr (4 separate FUSE ops)
- symlink, readlink (2 FUSE ops)
- POSIX file locks (fcntl) — getlk, setlk (2 FUSE ops)

None of these appear in the plan's listed trait surface. The fallback ("default impls return ENOSYS") would mean the kernel sees ENOSYS for `chmod`, `chown`, `truncate`, all four xattr ops, both symlink ops, and both file-lock ops — userspace apps see EOPNOTSUPP / EINVAL on these calls.

This is an **invariant regression** vs ADR-013. Today's `fuser`-based `KisekiFuse` does NOT implement all of these either (the existing impl only has 14 methods per `fuse_daemon.rs`); but the existing partial coverage isn't the spec's promise — ADR-013 is. The libfuse swap is an opportunity to verify parity and close gaps; the plan should make that verification explicit.

**Evidence**: ADR-013 lines 17-27:
```
| chmod, chown | Permission changes (stored in delta attributes) |
| readdir, readdirplus | Directory listing from view |
| symlink, readlink | Stored as inline data in delta |
| truncate, ftruncate | Composition resize |
| ...
| extended attributes (xattr) | getxattr, setxattr, listxattr, removexattr |
| POSIX file locks (fcntl) | Per-gateway lock state |
```

The current `fuse_daemon.rs` has 14 FUSE methods (per `grep "^    fn"`). The full ADR-013 supported list requires ~25 FUSE methods. Either (a) the existing impl already silently regresses ADR-013 (likely — separate finding), or (b) some of these are handled by FUSE defaults that don't return ENOSYS but rather "do nothing" (chmod can be a no-op if the FS doesn't store mode bits). Either way, the plan's trait listing should be **exhaustive against ADR-013** rather than illustrative.

**Suggested resolution**: Add to libfuse-swap.md §"Crate layout" a sub-section "ADR-013 parity check" listing every ADR-013 §"Supported" operation and the FUSE method that backs it. Phase 2 step 7 amended: "the new `Filesystem` trait MUST cover every ADR-013-supported operation, with explicit default impls only where ADR-013 marks the op as unsupported." Phase 4 BDD step amended: "verify each ADR-013 supported operation has an `@integration` scenario that exercises it through the new path." Add to Acceptance criteria: "(10) ADR-013 parity verified — every supported op has a covering test that exercises the libfuse path." If the existing `fuser` impl already silently regresses some ADR-013 ops, file that as a separate bug; this plan does NOT inherit those regressions silently.

---

### F3-H4: §"Safety contract" §3 async-bridge mechanics underspecified

**Severity**: High
**Category**: Correctness > Concurrency, Security > Trust boundaries
**Location**: libfuse-swap.md §"Crate layout" → §"Safety contract" rules 3 and 4

**Description**: Rule 3 says "Reply tokens cross async boundaries through an explicit bridge ... `tokio::sync::oneshot`-bridged adapter." Rule 4 says "When a tokio task driving a handler is cancelled (its future dropped), the wrapper detects the unfinished bridge and replies `EINTR` to libfuse before destroying the request."

Underspecified mechanics that the wrapper crate's audit will hit:

1. **Cancellation detection mechanism**: Rule 4 says "the wrapper detects." How? `tokio::oneshot::Receiver`'s `Drop` impl runs on cancellation, but the libfuse session thread is blocked on `recv()` waiting for the result. A dropped `Sender` causes the `recv()` to error with `RecvError`. So far so good — but the timing is: between "async task cancelled" and "session thread observes RecvError," the session thread is still waiting. Is there a deadline? If the async handler hangs (not cancelled, just slow), the session thread blocks indefinitely. Need a bounded-timeout `recv` or a separate watchdog.

2. **Per-request bridge map**: Rule 3 implies "one channel per request, dropped on consume." How is the channel stored? In a `DashMap<RequestId, Sender>`? In thread-local state? If `DashMap`, what's the cleanup path on the cancel-then-late-arrival case (the async task completes after cancellation; the receiver is gone; the result is silently discarded)? Memory bookkeeping is unspecified.

3. **Max pending ops**: A misbehaving Rust handler can hold a bridge indefinitely (no progress). Each in-flight FUSE op holds a oneshot bridge; an unbounded count grows the bridge map without limit. Need a `max_pending_ops` config + backpressure or rejection policy.

4. **Plaintext zeroize on cancel**: When a `read()` handler is cancelled mid-decrypt (after `read_chunk + open_envelope` produced a 64 MiB plaintext Vec), the cancellation fires EINTR but the plaintext sits in the dropped Future's destructor chain. The plan doesn't require zeroizing on this path. Currently kiseki uses `zeroize::Zeroizing<Vec<u8>>` in the decrypt cache (per the recent ADR-043-rev-1-era fix); that protection should explicitly extend to cancelled-handler plaintext.

**Suggested resolution**: Expand §"Safety contract" rules 3-4 with concrete sub-rules:

> 3.1. The bridge type is `tokio::sync::oneshot::{Sender, Receiver}<Result<ReplyResult, FuseError>>`. The session thread blocks on `recv()` with a per-request bounded timeout (default 30 s, configurable); timeout returns `EIO` to libfuse.
> 3.2. In-flight bridges are tracked in a `DashMap<RequestId, BridgeHandle>` on the wrapper. On consume the entry is removed. On cancel-then-late-arrival the late-arriving sender's `send()` returns `Err(SendError)` and the wrapper logs at WARN.
> 3.3. The wrapper enforces `max_pending_ops` (default 1024) on the in-flight bridge map; when the cap is hit, new FUSE requests are immediately replied with `EAGAIN` rather than queued. Configurable via `KisekiFuseConfig::max_pending_ops`.
> 4.1. Async handlers MUST zeroize plaintext buffers on cancellation. The wrapper provides a `ZeroOnCancel<Vec<u8>>` smart pointer that calls `zeroize()` on `Drop`; handlers wrap their plaintext returns with this type so the cancellation path zeroes regardless of which await point was cancelled.

These promote to the I-FUSE-* invariants per F3-H2's resolution.

---

### F3-H5: libfuse session thread panic / crash unhandled

**Severity**: High
**Category**: Robustness > Failure cascades, Error handling quality
**Location**: libfuse-swap.md §"Crate layout" §"Safety contract" rule 5; missing §"Failure modes" entry

**Description**: Rule 5 puts the libfuse session loop on a dedicated thread (or `spawn_blocking`). Doesn't address: what happens if the session thread crashes? libfuse's session loop can panic on internal assertion failures (rare but documented in upstream issues), or the dedicated thread can be killed by an external signal, or `read(2)` on `/dev/fuse` can return EIO if the kernel disconnects.

Today's `fuser` impl runs the session inline on the calling thread; a panic there crashes the kiseki-client process (CRASH > inconsistency). The libfuse swap moves the session to a separate thread; a panic there leaves the main process running but the FUSE mount is dead. User-facing apps then hang on every syscall against the mount until OOM-killed or `fusermount3 -uz`'d.

**Suggested resolution**: Add to §"Safety contract" a rule 7: "If the libfuse session thread exits (panic, EIO from kernel, signal), the wrapper detects this via the thread's join handle, logs at ERROR, and either (a) aborts the kiseki-client process (default — fail-fast, preserves the today-fuser-process-crashes shape), OR (b) attempts a single re-mount if `KisekiFuseConfig::auto_remount` is set (operator opt-in, default off because it can mask serious bugs)." Document the failure mode as F-FUSE-3 in `specs/failure-modes.md`.

---

## MEDIUM findings

### F3-M1: cargo `links = "fuse3"` collision risk

**Severity**: Medium
**Category**: Correctness > Implicit coupling
**Location**: libfuse-swap.md §"Crate layout" — `kiseki-fuse-sys/Cargo.toml` (`links = "fuse3"`)

**Description**: cargo's `links` attribute is keyed on the system library name; cargo refuses to link two crates that declare the same `links` value. If any other crate transitively pulled in (now or in the future) declares `links = "fuse3"` (e.g., the `fuse3` crate from crates.io, or a future dependency that wraps libfuse), the workspace fails to build with a "native library `fuse3` is being linked to by more than one package" error.

**Suggested resolution**: Document the `links = "fuse3"` claim in libfuse-swap.md and add a `cargo deny check bans` rule that rejects competing `links = "fuse3"` declarations from non-`kiseki-fuse-sys` crates (extends the F2-M9 *-sys-enforceability lint). Belt-and-braces: also pre-emptively `[patch.crates-io]` block the `fuse3` crate in `Cargo.toml` if anyone ever adds it to the dep tree.

### F3-M2: bindgen pin (0.69) tied to Rust toolchain

**Severity**: Medium
**Category**: Correctness > Implicit coupling, Operational
**Location**: libfuse-swap.md `kiseki-fuse-sys/Cargo.toml` skeleton

**Description**: `bindgen = "0.69"` is the version current at plan-write time. bindgen's MSRV moves with new releases; a future rust-toolchain.toml bump or a bindgen 0.70+ release could break the `*-sys` build. No upgrade policy.

**Suggested resolution**: Add to plan: "bindgen version policy — pin to the major (0.69) at plan write; bump to next major as part of the libfuse-swap go/no-go review (every 6 months) unless a security advisory forces a sooner bump."

### F3-M3: pkg-config error message not specified

**Severity**: Medium
**Category**: Robustness > Observability gaps, Operational
**Location**: libfuse-swap.md §"Crate layout" `kiseki-fuse-sys/build.rs` description

**Description**: The plan says `build.rs` "fails the build with a clear error if absent" but doesn't quote the message. Operators hitting this need actionable text — "install libfuse3-dev" on Debian is different from "install fuse3-devel" on RHEL, and the error should name both.

**Suggested resolution**: Quote the error in the plan: `panic!("kiseki-fuse-sys: libfuse3 development headers not found. Install: \n  Debian/Ubuntu: apt-get install libfuse3-dev\n  RHEL/Fedora:   dnf install fuse3-devel\n  Arch:          pacman -S fuse3\n\nThe `fuse` feature on kiseki-client requires this. To build without FUSE support, omit the feature.");`

### F3-M4: tokio `spawn_blocking` vs dedicated `std::thread` — pick one

**Severity**: Medium
**Category**: Correctness > Concurrency
**Location**: libfuse-swap.md §"Safety contract" rule 5

**Description**: Rule 5 says "tokio::task::spawn_blocking (or a dedicated `std::thread`)." `spawn_blocking` pulls from tokio's blocking thread pool (default cap 512); if FUSE generates many concurrent ops, it competes for slots with other blocking work in the kiseki-client process. A dedicated `std::thread` is unconditional. The "(or)" phrasing leaves the decision open.

**Suggested resolution**: Pin "dedicated `std::thread` named `kiseki-fuse-session`" — unconditional, isolated from tokio's pool, easier to debug in `top -H` / `perf top`. Update Rule 5 accordingly.

### F3-M5: Acceptance criterion 6 (FUSE_SYNCFS test) requires privileged container

**Severity**: Medium
**Category**: Robustness > Operational, Specification compliance
**Location**: libfuse-swap.md §"Acceptance criteria" criterion 6

**Description**: Criterion 6 requires "a test that drives `sync(2)` against a real mount and asserts our impl runs." Mounting FUSE in CI requires `/dev/fuse` access, which requires `--privileged` Docker or a runner with FUSE enabled by the host. The plan asserts this is "already true for the existing fuser CI lane" but the existing tests (`fuse_linux.rs`, `concurrent_fuse.rs`) don't actually mount — they exercise the `Filesystem` trait directly without the kernel.

**Evidence**: `crates/kiseki-client/tests/fuse_sync_adjacent_ops.rs` (the recently-landed test) drives the `KisekiFuse` API directly, NOT through a kernel mount. The kernel-side `sync(2)` regression test would be the FIRST CI test that requires a real FUSE mount. CI infrastructure may need new privileges.

**Suggested resolution**: Add to Phase 0 step 5 (new): "Audit CI runner privileges. The FUSE_SYNCFS regression test (criterion 6) requires a real kernel mount — `/dev/fuse` access + `fusermount3` setuid. Existing CI tests don't mount; this is new. Ensure GitHub Actions runners or the equivalent have the privileges; document in CONTRIBUTING.md the local-dev requirements."

### F3-M6: cfg-flag rollback assumes both branches always compile

**Severity**: Medium
**Category**: Robustness > Failure cascades, Operational
**Location**: libfuse-swap.md Phase 2 step 6 (cfg-flag introduction); §"Rollback procedure" Phase 2 row

**Description**: The cfg-flag rollback (`kiseki_fuse_backend_libfuse` vs `_fuser`) only works if both code branches compile cleanly during phases 2-5. If the libfuse port has a compile error, the cfg flag should let CI build with the fuser path. But cargo features default-resolve from the workspace; if the libfuse feature is default-on and broken, even cargo invocations that don't enable FUSE may still try to evaluate the feature graph and break.

**Suggested resolution**: Add to Phase 2: "CI matrix gates: the libfuse-feature build is allowed-to-fail during phases 2-5 if the fuser-feature build is green. The default flips back if the libfuse build is red for >24 hours." Also: have phase 2's first PR keep `kiseki_fuse_backend_fuser` default-on; flip the default to libfuse only at phase 4 (after the BDD parity check). This makes the rollback path the always-default during the risky window.

### F3-M7: GCP build path audit (criterion 9) "audited and updated" undefined

**Severity**: Medium
**Category**: Specification compliance
**Location**: libfuse-swap.md acceptance criterion 9

**Description**: Criterion 9 says GCP and wrapper build paths "have been audited and updated to install `libfuse3-dev`." Doesn't specify what counts as "audited and updated" — a PR merged? A manual sysadmin step? A test that exercises the build?

**Suggested resolution**: Replace with: "(9) `.gcp-build/build.sh` and `infra/gcp/setup-*.sh` scripts updated by a merged PR before Phase 1 of this plan begins. The next GCP perf-cluster run after Phase 6 must succeed at the kiseki-client build step; this is verified by the `infra/gcp/benchmarks/perf-suite-*.sh` smoke check."

### F3-M8: BDD parity verification doesn't explicitly include the @flaky 6-node EC PUT scenario

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: libfuse-swap.md §"Risk register" mitigation for "BDD scenarios assume fuser's exact reply ordering"

**Description**: The risk-register row says "Run the @flaky `D-10 cross-stream` and `6-node EC PUT` scenarios specifically." Phase 4 step 18 asserts "Run the full BDD `@library` and `@integration` FUSE scenarios; verify parity." But the risk register's named scenarios are not FUSE-specific — they're the two `@flaky` scenarios in the broader project (per CLAUDE.md). The libfuse-swap shouldn't break them, but Phase 4 doesn't explicitly cover them.

**Suggested resolution**: Phase 4 step 18 amended: "Run the full BDD `@library` and `@integration` FUSE scenarios; **plus the two project-level `@flaky` scenarios (D-10 cross-stream and 6-node EC PUT) which the risk register flags as multi-thread-shape-sensitive.** Verify parity. Recovery target: 100% of currently-green scenarios stay green; the two `@flaky` scenarios at no worse than baseline retry rate."

---

## LOW findings

### F3-L1: Effort estimate timeline assumes single full-time engineer

**Severity**: Low
**Category**: Operational
**Location**: libfuse-swap.md §"Effort estimate"

**Description**: 4-6 days assumes one engineer focused full-time. If the work spans multiple people, has waiting periods (libfuse advisory triage), or competes with other priorities, the wall-clock can be much longer. Common for plans with this scope.

**Suggested resolution**: Add: "Wall-clock estimate assumes one engineer focused full-time. Calendar timeline 1-2 weeks under typical interleaving; longer if any phase fails review."

### F3-L2: §"Safety contract" picks `oneshot` without naming alternatives

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: libfuse-swap.md §"Safety contract" rule 3

**Description**: The plan picks `tokio::sync::oneshot` without arguing why over alternatives (mpsc with capacity 1, broadcast, custom). Architectural decision; should be a sentence on the trade-off.

**Suggested resolution**: Add: "`oneshot` over `mpsc(1)`: `oneshot` provides exact send-once semantics (sender consumed on send, receiver can detect dropped sender via `RecvError`); `mpsc(1)` allows multiple senders which is wrong for our shape. `oneshot` over a custom waker: less code, well-audited."

### F3-L3: Phase 6 gate "Phase 4 parity ≥ 1 week" needs a CI definition

**Severity**: Low
**Category**: Operational
**Location**: libfuse-swap.md §"Rollback procedure" mitigation

**Description**: "Phase 4 BDD has been at parity for ≥ 1 week in CI" — what constitutes "parity"? Same pass/skip count? Same wall-clock? Same flake rate?

**Suggested resolution**: "Phase 4 parity = the BDD suite's pass count is identical to pre-swap baseline AND the project-level `@flaky` count is unchanged AND the median wall-clock per scenario is within ±20%. CI tracks all three across the 7-day window via the existing nextest report."

### F3-L4: §"Why this binding" doesn't address "what about polyfuse maintainers' improvements?"

**Severity**: Low
**Category**: Specification compliance
**Location**: libfuse-swap.md §"Why this binding (not the others)" — polyfuse rejection

**Description**: The polyfuse rejection says "Pure-Rust reimplementation, same protocol-reimpl class as fuser." But polyfuse's maintenance shape is different (async-first, more recent activity); the rejection bundles all "pure-Rust FUSE crates" together. Could be sharper.

**Suggested resolution**: One-sentence elaboration: "polyfuse is async-first and more actively maintained than fuser at the current cadence, but the fundamental issue (pure-Rust protocol reimpl in a small maintainer pool) is the same class of risk we're avoiding. Selecting libfuse-direct settles the question for both reimpl crates simultaneously."

---

## Cross-cutting

### CC3-1: §D6 checklist verification convention

The §D6 review-discipline says "the architect documents the answer to each before merging the plan; gate-1 review verifies the answers." Currently the architect's libfuse answers live as a one-liner in ADR-043 §D6 ("every criterion **no**; therefore plan-only adoption is appropriate"). For machine-traceability, future plans should embed the seven-row answer in the plan's frontmatter as a YAML block or a labelled table.

**Resolution**: Add to ADR-043 §D6 (process note): "The plan's first heading section MUST contain a §D6 checklist table with one row per criterion and a yes/no/explain answer. This makes both the architect's reasoning and the adversary's verification easier to read against the plan content." Minor process tightening; not blocking this round.

### CC3-2: Invariants/failure-modes promotion path

F3-H2 recommends promoting §"Safety contract" rules to `specs/invariants.md` (I-FUSE-1..I-FUSE-6) and failure modes to `specs/failure-modes.md` (F-FUSE-1..F-FUSE-3). Doing this changes the catalogues and triggers ADR-027's "domain model in one place" discipline. Architect confirms whether this is the resolution path or whether a per-binding ADR-044 is cleaner.

**Resolution**: Architect picks F3-H2's (a) per-binding ADR or (b) catalogue promotion. Either keeps the policy honest; (b) is more durable.

---

## Verdict

**CHANGES REQUESTED — small-to-medium scope.** The plan is structurally sound; the issues are concentrated in checklist-answer integrity (F3-H1, F3-H2), trait-surface coverage (F3-H3), and async-bridge mechanics (F3-H4, F3-H5).

What blocks implementer phase 0:
- **F3-H1, F3-H2**: resolve the §D6 checklist disagreement. Either escalate to ADR-044 (small) OR (preferred) promote the §"Safety contract" rules to `specs/invariants.md` as I-FUSE-* and add failure modes to `specs/failure-modes.md`, then explicitly justify the **no** answers in the plan.
- **F3-H3**: ADR-013 parity check added to plan + acceptance criterion 10. Without this the swap silently regresses POSIX scope.
- **F3-H4**: §"Safety contract" expanded with concrete bridge mechanics (timeout, max_pending_ops, ZeroOnCancel for plaintext).
- **F3-H5**: session-thread crash handling added to §"Safety contract" + F-FUSE-3 promoted.

What can be addressed in the implementer phase or rolled into Phase 0:
- All MEDIUMs except F3-M3 (pkg-config error message — copy-paste in build.rs) and F3-M5 (CI privileges — Phase 0 audit), both of which should land in Phase 0 itself.
- All LOWs.

Estimated amendment: ~60-90 lines of additions to libfuse-swap.md, plus ~20-30 lines added to `specs/invariants.md` and `specs/failure-modes.md` (per F3-H2 resolution path b). Architect-only round; no analyst pass needed.

After amendment: the libfuse-swap plan is ready for implementer phase 0; ADR-043 acceptance still pending only on Open item B (FIPS evaluator written reference).
