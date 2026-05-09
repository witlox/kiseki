# ADR-043 Implementation Plan — fuser-rs → libfuse 3.x Swap

**Status:** Draft (gate-1 round 2 amendments applied; awaits the plan-specific gate-1 round 3 before implementer phase 0).
**Created:** 2026-05-09
**Last amended:** 2026-05-09 (round-2 findings F2-H2 / F2-H3 / F2-M3 / F2-M4 / F2-M5 / F2-L3 closed inline)
**Tracks:** ADR-043 rev 3.
**Owner role:** implementer (after the plan-specific gate-1 passes).
**Reviews so far:** ADR-043 round-1 findings (`specs/findings/2026-05-09-adv-gate1-adr043-findings.md`); round-2 findings on rev-2 + this plan (`specs/findings/2026-05-09-adv-gate1-round2-adr043-findings.md`).

## Goal

Replace the `fuser` 0.17 Rust FUSE protocol implementation in
`kiseki-client/src/fuse_daemon.rs` with a thin Rust wrapper over
the upstream libfuse 3.x C library. Preserve the existing
`KisekiFuse` API surface so call sites in `kiseki-client/src/bin/`,
`kiseki-acceptance`, and `kiseki-profile` continue to work with
minimal diffs.

## Why this binding (not the others)

| Candidate | Verdict | Reason |
|---|---|---|
| **libfuse 3.x via FFI** | **Selected** | Reference impl maintained by FUSE upstream. Multi-thread session loop. All opcodes implemented including FUSE_SYNCFS (the 2026-05-09 sync-investigation gap). Decades of bug history. Used by sshfs, juicefs, gcsfuse, ceph-fuse, dfuse, gocryptfs. |
| `fuse3` crate | Rejected | Has both pure-Rust and libfuse backends; unstable choice surface. We'd carry the crate's design churn. Selecting libfuse-direct is more durable. |
| `polyfuse` | Rejected | Pure-Rust reimplementation, same protocol-reimpl class as fuser. Trades one young Rust FUSE crate for another; doesn't address the structural concern. |
| Direct `/dev/fuse` + custom protocol code | Rejected | Becomes a fourth Rust FUSE protocol impl. Effort comparable to upstreaming PRs to fuser. No reliability win. |
| **Stay on fuser-rs + upstream PRs** | Rejected (Alternative 6 in the ADR) | The named gap (FUSE_SYNCFS opcode 50) is PR-able, but the structural issue (single-thread inline dispatch, smaller maintainer pool than libfuse, slower release cadence) remains. PR-cycle cost compounds across cumulative gaps; the swap is more durable. |

## Scope and non-scope

**In scope:**
- New `kiseki-fuse-sys` crate: bindgen against libfuse 3.x, raw FFI.
- New `kiseki-fuse` crate: safe Rust wrapper exposing a `Filesystem`-shaped trait close to fuser's, plus a `mount()` entry point.
- Port `kiseki-client/src/fuse_daemon.rs` to call the new wrapper.
- Update test files in `kiseki-client/tests/` and BDD step defs in `kiseki-acceptance/`.
- Drop `fuser = "0.17"` from `kiseki-client/Cargo.toml`.
- Verify: all FUSE BDD scenarios pass at parity; the recently-landed `fuse_sync_adjacent_ops`, `fuse_mount_cleanup`, `per_peer_cap_collision` tests stay green; the GCP-derived bug regressions stay green.

**Out of scope (deferred to separate decisions):**
- macOS FUSE (`fuse_macos.rs`) — see §"macOS posture" below; recommend retire-now, revisit if macOS users surface.
- Windows FUSE (WinFsp) — never claimed, stays out.
- ADR-043 itself — the libfuse-only amendment is in `specs/architecture/adr/043-...md`; this file is the implementation plan that follows from that amendment.
- Removal of `kiseki-client::remote-nfs` — separate work item; tracked in §"Sequencing relative to remote-nfs removal" below for context only.

## Crate layout

Two new crates, following the `*-sys` / safe-wrapper convention codified in ADR-043 §D6 (mirroring `libfabric-sys` / `kiseki-fabric` in the existing workspace):

### `crates/kiseki-fuse-sys/`

Raw FFI bindings to libfuse 3.x.

```
crates/kiseki-fuse-sys/
├── Cargo.toml          # links = "fuse3", build.rs invokes bindgen
├── build.rs            # pkg-config lookup of fuse3; bindgen the headers
├── src/lib.rs          # `include!(concat!(env!("OUT_DIR"), "/bindings.rs"))`
└── wrapper.h           # `#include <fuse3/fuse.h>` + `<fuse3/fuse_lowlevel.h>`
```

`Cargo.toml` skeleton:

```toml
[package]
name = "kiseki-fuse-sys"
version.workspace = true
edition.workspace = true
links = "fuse3"

[build-dependencies]
bindgen = "0.69"
pkg-config = "0.3"
```

`build.rs` uses `pkg-config` to locate libfuse3, fails the build with a clear error if absent, and runs bindgen against `wrapper.h`. Pinned minimum libfuse version: **3.10** (chosen because that's where FUSE_SYNCFS landed in the upstream FUSE protocol per kernel ≥ 5.1, which was the original gap that triggered this work).

### `crates/kiseki-fuse/`

Safe Rust wrapper exposing the `Filesystem`-shaped trait that `KisekiFuse` already conforms to in spirit, plus the session loop and mount entry points.

```
crates/kiseki-fuse/
├── Cargo.toml          # depends on kiseki-fuse-sys
├── src/
│   ├── lib.rs          # public re-exports
│   ├── filesystem.rs   # the `Filesystem` trait (lookup, getattr, read, write, …)
│   ├── reply.rs        # ReplyAttr, ReplyData, ReplyCreate, ReplyEmpty, ReplyWrite — wraps libfuse reply funcs
│   ├── session.rs      # session_new, session_loop_mt, session_destroy
│   ├── mount.rs        # `mount(daemon, mountpoint, options) -> Result<JoinHandle>`
│   ├── error.rs        # FuseError enum, errno conversions
│   └── request.rs      # Request context (caller_uid, caller_gid, pid)
└── tests/              # smoke-test the wrapper without needing a real /dev/fuse
```

Trait shape matches what `fuse_daemon.rs` consumes today (modulo libfuse-vs-fuser protocol differences):

```rust
pub trait Filesystem: Send + Sync + 'static {
    fn lookup(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry);
    fn getattr(&self, req: &Request, ino: u64, reply: ReplyAttr);
    fn read(&self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, reply: ReplyData);
    fn write(&self, req: &Request, ino: u64, fh: u64, offset: i64, data: &[u8],
             write_flags: u32, lock_owner: Option<LockOwner>, reply: ReplyWrite);
    fn create(&self, req: &Request, parent: u64, name: &OsStr, mode: u32,
              umask: u32, flags: i32, reply: ReplyCreate);
    fn unlink(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty);
    fn flush(&self, req: &Request, ino: u64, fh: u64, lock_owner: LockOwner, reply: ReplyEmpty);
    fn release(&self, req: &Request, ino: u64, fh: u64, flags: i32,
               lock_owner: Option<LockOwner>, flush: bool, reply: ReplyEmpty);
    fn fsync(&self, req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty);
    fn syncfs(&self, req: &Request, reply: ReplyEmpty);  // <-- the opcode 50 fuser doesn't implement
    fn mkdir(&self, req: &Request, parent: u64, name: &OsStr, mode: u32,
             umask: u32, reply: ReplyEntry);
    fn rmdir(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty);
    fn rename(&self, req: &Request, parent: u64, name: &OsStr, newparent: u64,
              newname: &OsStr, flags: u32, reply: ReplyEmpty);
    fn opendir(&self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen);
    fn readdir(&self, req: &Request, ino: u64, fh: u64, offset: i64, reply: ReplyDirectory);
    fn releasedir(&self, req: &Request, ino: u64, fh: u64, flags: i32, reply: ReplyEmpty);
    fn statfs(&self, req: &Request, ino: u64, reply: ReplyStatfs);
    // …default impls return ENOSYS for unimplemented ops; libfuse handles the rest
}
```

Aim: a `KisekiFuse` impl that today derives `fuser::Filesystem` derives `kiseki_fuse::Filesystem` instead, with mostly-1:1 method signature changes.

### Safety contract (added round-2 per F2-H3)

The `kiseki-fuse` safe wrapper enforces an explicit contract to prevent the use-after-free / leaked-slot bugs that plague FFI'd FUSE filesystems. The contract is the audit target for the gate-2 review on `kiseki-fuse` (see Acceptance criterion 8 below).

1. **Reply tokens are consume-once.** `ReplyAttr`, `ReplyData`, `ReplyEmpty`, `ReplyWrite`, `ReplyCreate`, `ReplyEntry`, `ReplyOpen`, `ReplyDirectory`, `ReplyStatfs` each consume themselves on `.attr(...)`, `.data(...)`, `.ok()`, etc. After consume, the token is dropped without further FFI calls.
2. **Drop-without-consume = EIO + leak warning.** If a Rust handler returns or panics without consuming the reply token, the `Drop` impl issues `fuse_reply_err(req, EIO)` to libfuse and logs at WARN level (debug builds: panic to surface the bug). This guarantees libfuse never holds a request slot indefinitely. Correctness over cleanliness.
3. **Reply tokens cross async boundaries through an explicit bridge.** Reply tokens are not directly `Send` across the libfuse-session-thread / tokio-task boundary. The wrapper provides a `tokio::sync::oneshot`-bridged adapter that hands a typed result from the async handler back to the session thread for finalization. The bridge is one channel per request, dropped on consume.
4. **Cancellation produces EINTR, not leaks.** When a tokio task driving a handler is cancelled (its future dropped), the wrapper detects the unfinished bridge and replies `EINTR` to libfuse before destroying the request. The kernel sees a clean error, the slot is freed.
5. **Session loop runs on a dedicated thread.** libfuse's session is C-thread-based and cannot be driven from the tokio executor directly. The wrapper spawns the session loop on `tokio::task::spawn_blocking` (or a dedicated `std::thread`) and routes opcodes to async handlers via the bridge in (3).
6. **Trait bounds: `Filesystem: Send + Sync + 'static`.** The trait itself is fine; what's tightly constrained is the reply-token lifetime, not the handler's own state.

The gate-2 audit on `kiseki-fuse` validates this contract is honored in code, with concrete attack tests:
- Cancel a handler future mid-call; assert `EINTR` reaches the kernel.
- Drop a reply token without consume; assert `EIO` is returned (debug build: assert panic).
- Send a reply token to another thread without the bridge; assert this fails to compile.

## Migration (file-by-file)

### Phase 0 — pre-work (no behavior change)

1. Confirm `libfuse3-dev` (Debian/Ubuntu) / `fuse3-devel` (RHEL/Fedora) is installed in dev + CI environments. Update `.github/workflows/ci.yml` to `apt-get install -y libfuse3-dev` before the `kiseki-client --features fuse` test lane.
2. Confirm runtime presence of `/dev/fuse` and `fusermount3` on test runners (already required for the existing `fuser` 0.17 path; this is a no-op).
3. **Audit GCP perf-cluster build paths (added round-2 per F2-M3)**: `.gcp-build/build.sh` and the `infra/gcp/` setup scripts must install `libfuse3-dev` / `fuse3-devel` before building kiseki-client with `--features fuse,remote-http,native`. Without this, the next perf-cluster run breaks at `kiseki-fuse-sys`'s `pkg-config` lookup. Produce a small PR to those scripts before Phase 1 lands; the PR is part of Phase 0 acceptance.
4. **Audit downstream wrapper builds**: the Python (PyO3) and C++ wrappers' build scripts must either install `libfuse3-dev` or document that the `fuse` feature is off-by-default. Wrappers/README updated in Phase 6.

### Phase 1 — land the new crates (no kiseki-client changes yet)

3. Add `crates/kiseki-fuse-sys/` with bindgen against libfuse 3.10+. Smoke test: build succeeds, `cargo doc -p kiseki-fuse-sys` shows the bound types.
4. Add `crates/kiseki-fuse/` with the safe wrapper. Cover the trait surface listed above. Add a smoke test that mounts an empty filesystem and reads `getattr` of the root, asserts no panic. (This requires `/dev/fuse` access in CI; the existing fuser CI lane already needs it.)
5. Mark `kiseki-fuse` as a workspace member; nothing depends on it yet.

### Phase 2 — port `kiseki-client/src/fuse_daemon.rs`

6. **Introduce the `kiseki_fuse_backend` cfg switch** (per §"Rollback procedure"). Add two mutually-exclusive cargo features to `kiseki-client`: `kiseki_fuse_backend_libfuse` (default-enabled at Phase 2 merge) and `kiseki_fuse_backend_fuser` (legacy fallback). Both compile during phases 2-5. Rollback during this window flips the default in one line.
7. In `crates/kiseki-client/src/fuse_daemon.rs`:
   - Wrap the existing `use fuser::{...}` in `#[cfg(feature = "kiseki_fuse_backend_fuser")]` and add a parallel `#[cfg(feature = "kiseki_fuse_backend_libfuse")] use kiseki_fuse::{...};`.
   - Duplicate the handler-impl block under both cfgs initially (the new branch is the working port; the legacy branch keeps fuser-shape unchanged).
   - Update reply type names where the wrapper diverges from fuser's vocabulary (e.g., `ReplyEntry` was always called the same; `ReplyOpen` is identical; `ReplyDirectory` may differ in the iterator-vs-buffer shape — wrap on our side to match).
   - Implement the `syncfs` op against `gateway.fsync_pending()` — the hook chain that ADR-040 `docs/operations/durability.md` already documents. This closes the FUSE_SYNCFS opcode 50 gap that was the original trigger.
   - Replace `fuser::mount2(daemon, mountpoint, &options)` with `kiseki_fuse::mount(daemon, mountpoint, options)` (libfuse branch).
8. In `crates/kiseki-client/src/bin/kiseki_client.rs`: the `evict_stale_fuse_mount` helper using `fusermount3 -uz` keeps working unchanged.
9. In `crates/kiseki-client/Cargo.toml`:
   - Add `kiseki-fuse = { workspace = true, optional = true }` alongside the existing `fuser = { version = "0.17", optional = true }` (both kept until Phase 6).
   - Update the `fuse` feature to: `fuse = ["kiseki_fuse_backend_libfuse"]` (default to the new backend); `kiseki_fuse_backend_libfuse = ["dep:kiseki-fuse"]`; `kiseki_fuse_backend_fuser = ["dep:fuser"]`. Downstream consumers (Python wrapper, kiseki-profile) flip without their own Cargo.toml changes.

### Phase 3 — port the test files

10. `crates/kiseki-client/tests/fuse_linux.rs`: update imports.
11. `crates/kiseki-client/tests/posix_semantics.rs`: update imports.
12. `crates/kiseki-client/tests/concurrent_fuse.rs`: update imports.
13. `crates/kiseki-client/tests/fuse_sync_adjacent_ops.rs`: update imports; **expand the test** — add a probe that drives `syncfs` directly via a kernel `sync(2)` against the mount, verifying our libfuse `syncfs` impl gets called and returns Ok. This is the regression test that pins the original FUSE_SYNCFS gap closed.
14. `crates/kiseki-client/tests/fuse_mount_cleanup.rs`: no `fuser` direct deps; should compile unchanged.
15. **`crates/kiseki-client/tests/fuse_macos.rs`: delete.** Per the §"Platform scope" decision macOS is officially out; the test is removed outright (git history preserves the prior content for reference).

### Phase 4 — port BDD step defs

16. `crates/kiseki-acceptance/tests/steps/client.rs`: update FUSE-related step defs.
17. `crates/kiseki-acceptance/tests/steps/gateway.rs`: update FUSE-related step defs.
18. Run the full BDD `@library` and `@integration` FUSE scenarios; verify parity. Recovery target: 100% of currently-green scenarios stay green; 0 new flakes.

### Phase 5 — port kiseki-profile

19. `crates/kiseki-profile/src/main.rs` + `protocols.rs`: update FUSE perf path imports.
20. Re-run a local `kiseki-profile fuse_writeread_64m` matrix; numbers should be at parity or better (libfuse's multi-thread session loop should *help* concurrent FUSE workloads, addressing the `fuser-library single-thread inline dispatch limitation` flagged in user memory).

### Phase 6 — clean up

Phase 6 only lands once Phase 4 has been at parity ≥ 1 week and Phase 5 has had ≥ 1 perf matrix run at parity (per §"Rollback procedure" mitigations).

21. Remove the `kiseki_fuse_backend_fuser` feature from `kiseki-client/Cargo.toml`; collapse the cfg-`if` in `fuse_daemon.rs` to libfuse-only.
22. Delete `fuser = { version = "0.17", optional = true }` from `kiseki-client/Cargo.toml`.
23. `cargo update` to drop fuser from `Cargo.lock`.
24. Verify `cargo tree -p kiseki-client --features fuse` no longer shows fuser anywhere.
25. Run `make verify` (Tier 1) and `make test-slow` (Tier 2) to confirm no regressions.

## Acceptance criteria

This swap is "done" when:

1. `cargo nextest run -p kiseki-client` passes all FUSE-feature-gated tests.
2. The full BDD suite (`make test-slow`) shows the same pass/skip counts as the pre-swap baseline (321 scenarios per CLAUDE.md; 320 pass + 1 `@flaky` retried 2×).
3. Local `kiseki-profile` FUSE perf is at parity or better than the current numbers in `docs/performance/README.md`.
4. `cargo tree -p kiseki-client --features fuse` shows no `fuser` crate.
5. `cargo deny check` passes (license + advisory + bans).
6. **The FUSE_SYNCFS opcode 50 path is exercised** by a test that drives `sync(2)` against a real mount and asserts our impl runs (not the kernel default-ENOSYS fallback).
7. **(added round-2 per F2-H2)** ADR-043 §D2 libfuse row is updated with D1.1 security-posture data: upstream CVE history reference (link to `libfuse/libfuse` GitHub Security Advisories), the most recent CVE id and the kiseki-internal time-to-patch we'd commit to (or "no CVE in window" if the advisories list is clean at acceptance), and the kiseki triage SLA (default per D1.1: CRITICAL ≤ 7 days, HIGH ≤ 30 days, MEDIUM at next release). Locks the plan's "done" state to the policy's stated requirement.
8. **(added round-2 per F2-H3)** `kiseki-fuse` crate has a gate-2 audit pass against the §"Safety contract" above; concretely: cancellation tests (cancel a handler future; assert `EINTR` reaches the kernel), drop-without-consume tests (drop a reply token; assert `EIO` is returned in release, panic in debug), and a compile-fail test asserting reply tokens cannot be sent across the C-thread boundary without the bridge.
9. **(added round-2 per F2-M3)** GCP perf-cluster build scripts (`.gcp-build/build.sh`, `infra/gcp/`) and downstream wrapper build paths (Python PyO3, C++) have been audited and updated to install `libfuse3-dev` / `fuse3-devel` where the `fuse` feature is enabled. The next GCP perf run after this swap must not fail at the `pkg-config` lookup.

## Platform scope (Linux only; macOS / Windows out of scope)

**macOS FUSE compatibility is not a kiseki target.** Architect's decision recorded 2026-05-09 (round-2 closure of F2-L3): kiseki's positioning is HPC/AI workloads, all of which run on Linux. The `fuser` 0.17 dependency happened to compile on macOS via `osxfuse`/`macfuse`, and `tests/fuse_macos.rs` was opportunistic coverage of that side-effect — never a committed product surface.

Concrete consequences of this scope decision:

- `crates/kiseki-client/tests/fuse_macos.rs` is **deleted** (Phase 3 step 14, no archive). The test was never load-bearing; deleting reduces drift.
- The `fuse` feature on `kiseki-client` is **Linux-only** post-swap. On macOS, building with `--features fuse` returns a compile error from `kiseki-fuse-sys`'s `build.rs` ("libfuse3 not available on this platform; the `fuse` feature is Linux-only"). Building **without** the `fuse` feature works on macOS unchanged — kiseki-client still provides S3, native, and remote-http on macOS.
- Downstream wrappers (Python PyO3, C++) document that the `fuse` feature is Linux-only; their macOS build paths must omit it.

Windows FUSE (WinFsp) is also out of scope, never claimed; this is a no-op restatement.

Re-introducing macOS FUSE would require: (a) a new ADR-043 §D2 row for `macfuse-sys` under the same FIPS-isolation rule, (b) a new `kiseki-fuse-mac-sys` crate against macFUSE's libfuse-2.x-shaped ABI, (c) a parallel safe-wrapper layer or an abstraction over `kiseki-fuse` and `kiseki-fuse-mac`. None of this is planned; this section exists to record the explicit decision, not to imply a future path.

## Sequencing relative to remote-nfs removal

The fuser swap and the `kiseki-client::remote-nfs` removal are independent. The fuser swap touches `fuse_*` files; the remote-nfs removal touches `remote_nfs/` files and BDD/perf consumers of NFS-via-userspace-client. Both can land in parallel branches. Recommend the fuser swap lands first (more urgent — concrete bug pattern), the remote-nfs work follows.

## Go/no-go review (added round-2 per F2-M4)

Six months after `fuser` is removed from `Cargo.lock` (final D6 step in this plan), the architect runs a review of the libfuse swap against the criteria below. Date target is set when the final phase merges and recorded in ADR-043 §"Review schedule" alongside other binding rows.

| Criterion | Threshold | Source |
|---|---|---|
| Tier_1 perf-cluster numbers | within ±10% of pre-swap baseline (2026-05-09 GCP `compact` numbers in `specs/performance/`) | next `kiseki-profile` matrix run on the GCP cluster |
| Unpatched libfuse CVE in the kiseki-pinned version range | 0 CRITICAL or HIGH older than the D1.1 SLA | `libfuse/libfuse` GitHub Security Advisories review |
| FUSE BDD scenario flake rate | ≤ baseline + 1 scenario flagged `@flaky` | full BDD suite (Tier 2) trailing-30-day pass rate |
| FUSE_SYNCFS regression coverage | the round-2-mandated test (Acceptance criterion 6) is still green | `cargo nextest` |

If any criterion fails, mark the libfuse row Rejected per ADR-043 §D5 and revert via §"Rollback procedure" below.

## Rollback procedure (added round-2 per F2-M5)

Each phase merges as a standalone PR; rollback granularity matches:

| Phase | If we need to roll back this phase | Cost |
|---|---|---|
| 1 (new crates) | Revert the PR. `kiseki-fuse-sys` + `kiseki-fuse` remain unused; nothing else broke. | Trivial — revert. |
| 2 (port `fuse_daemon.rs`) | The port is gated by a `kiseki_fuse_backend` cargo feature with values `libfuse` (new default) and `fuser` (legacy). Both code paths compile during phases 2-5. Rollback flips the default back to `fuser`; both fuser and kiseki-fuse remain in `Cargo.toml`. No commits revert. | One-line cfg edit. |
| 3-4 (test files + BDD) | Same cfg flag — tests run against the legacy fuser path until parity is re-verified. | One-line cfg edit. |
| 5 (kiseki-profile) | Same cfg flag. | One-line cfg edit. |
| 6 (drop `fuser` dep) | `fuser` is removed from `Cargo.toml`. Rollback re-adds it (one-line edit) and re-introduces the cfg selector in `fuse_daemon.rs`. | ~30 lines. |
| Post-Phase-6 (production rollback) | `fuser` is gone from `Cargo.lock`. Re-introduce as a fresh dep + re-port `fuse_daemon.rs` from the (still-in-git-history) pre-swap version. | 4-6 days, comparable to the original swap. |

Mitigations against expensive late rollback:

- Phase 6 only lands once Phase 4 BDD has been at parity for **≥ 1 week** in CI (no new flakes; no functional regression in the `@library` or `@integration` lanes).
- Phase 6 only lands once Phase 5 perf has been at parity for **≥ 1 perf matrix run**.
- Both pre-conditions are listed as Acceptance criteria above (criteria 2-3); Phase 6 cannot be merged before they pass.

## Risk register

| Risk | Mitigation |
|---|---|
| libfuse 3.10 features (FUSE_SYNCFS) require kernel ≥ 5.1; older systems regress | CLAUDE.md doesn't pin a minimum kernel; document min kernel ≥ 5.4 in operator docs (5.4 is Ubuntu 20.04 LTS, well-established). |
| BDD scenarios assume fuser's exact reply ordering or threading model | libfuse's multi-thread session loop changes concurrency shape. Run the @flaky `D-10 cross-stream` and `6-node EC PUT` scenarios specifically; if the multi-thread reordering changes their pass rate, flag for separate investigation. |
| `kiseki-fuse-sys` build fails on a contributor's machine without `libfuse3-dev` | `build.rs` panics with a clear `pkg-config` error message naming the package; CONTRIBUTING.md gets a one-line install note. |
| Python wrapper / cdylib consumers break on macOS due to dropped feature | Default-off `fuse` feature; macOS consumers compile fine without it. Document in wrappers/README. |
| FFI safety bugs in `kiseki-fuse-sys` (use-after-free in a reply) | Safe wrapper layer in `kiseki-fuse` is the audit point. Adversary review on `kiseki-fuse` crate before merge — gate-2 audit at minimum. |

## Test-impact estimate

- New tests: ~3-5 (the libfuse-specific syncfs probe, a smoke test in `kiseki-fuse`, a wrapper-level safety test).
- Modified tests: 6 (`kiseki-client/tests/fuse_*.rs` + 2 BDD step files).
- Retired tests: 1 (`fuse_macos.rs`).
- Net: workspace test count moves from ~1650 unit + 321 BDD to ~1652-1654 unit + 321 BDD (no BDD scenario count change).

## Effort estimate

- Phase 0-1 (new crates): 1-2 days.
- Phase 2 (port `fuse_daemon.rs`): 1-2 days.
- Phase 3 (port test files): 0.5 day.
- Phase 4 (BDD step defs + verification): 1 day.
- Phase 5-6 (perf + cleanup): 0.5 day.

**Total: 4-6 days of focused work.** Net code change: replaces ~518 lines of `fuser`-using `fuse_daemon.rs` with similar-size libfuse-using version, plus two new crates totaling ~600-800 lines (the `*-sys` is mostly bindgen output; the safe wrapper is the real engineering).

## Cross-references

- ADR-043 rev 3: the policy that permits this swap. Architect's §D6 checklist answers (every criterion **no**) recorded in the ADR's §D6.
- `specs/findings/2026-05-09-adv-gate1-adr043-findings.md`: round-1 gate-1 findings (the rev-1 review); F-M5 (cross-platform → §"Platform scope"), F-M1 (license → Acceptance criterion 5 + Open item A), F-M8 (fuser-PR alternative → §"Why this binding"), F-M9 (`*-sys` enforceability → ADR-043 §D4) all carry into this plan.
- `specs/findings/2026-05-09-adv-gate1-round2-adr043-findings.md`: round-2 gate-1 findings; F2-H2 (D1.1 acceptance check → criterion 7), F2-H3 (FFI safety contract → §"Safety contract" + criterion 8), F2-M3 (GCP build audit → Phase 0 step 3 + criterion 9), F2-M4 (go/no-go review → §"Go/no-go review"), F2-M5 (rollback → §"Rollback procedure"), F2-L3 (macOS upgrade → §"Platform scope") all closed inline in this rev.
- `specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md`: the original FUSE_SYNCFS gap finding; this swap closes it (Acceptance criterion 6).
- `specs/architecture/adr/013-posix-semantics-scope.md`: the operation matrix this swap preserves at parity (no scope change).
- User memory `project_fuse_3phase_pattern`: the 3-phase write-lock pattern in `KisekiFuse` is invariant across the swap; the trait surface preserves it.
- User memory `project_group_commit_contract`: `gateway.fsync_pending()` hook chain is the contract `syncfs` calls into; no change.
