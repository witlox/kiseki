# ADR-043 Implementation Plan — fuser-rs → libfuse 3.x Swap

**Status:** Draft (pending ADR-043 amendment to libfuse-only scope)
**Created:** 2026-05-09
**Tracks:** ADR-043 (revised); supersedes the broader ganesha+libfuse direction in `d65564d`.
**Owner role:** implementer; gate-1 already covered the policy in `specs/findings/2026-05-09-adv-gate1-adr043-findings.md`.

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

## Migration (file-by-file)

### Phase 0 — pre-work (no behavior change)

1. Confirm `libfuse3-dev` (Debian/Ubuntu) / `fuse3-devel` (RHEL/Fedora) is installed in dev + CI environments. Update `.github/workflows/ci.yml` to `apt-get install -y libfuse3-dev` before the `kiseki-client --features fuse` test lane.
2. Confirm runtime presence of `/dev/fuse` and `fusermount3` on test runners (already required for the existing `fuser` 0.17 path; this is a no-op).

### Phase 1 — land the new crates (no kiseki-client changes yet)

3. Add `crates/kiseki-fuse-sys/` with bindgen against libfuse 3.10+. Smoke test: build succeeds, `cargo doc -p kiseki-fuse-sys` shows the bound types.
4. Add `crates/kiseki-fuse/` with the safe wrapper. Cover the trait surface listed above. Add a smoke test that mounts an empty filesystem and reads `getattr` of the root, asserts no panic. (This requires `/dev/fuse` access in CI; the existing fuser CI lane already needs it.)
5. Mark `kiseki-fuse` as a workspace member; nothing depends on it yet.

### Phase 2 — port `kiseki-client/src/fuse_daemon.rs`

6. In `crates/kiseki-client/src/fuse_daemon.rs`:
   - Replace `use fuser::{...}` with `use kiseki_fuse::{...}`.
   - Update reply type names where the wrapper diverges from fuser's vocabulary (e.g., `ReplyEntry` was always called the same; `ReplyOpen` is identical; `ReplyDirectory` may differ in the iterator-vs-buffer shape — wrap on our side to match).
   - Implement the `syncfs` op against `gateway.fsync_pending()` — the hook chain that ADR-040 `docs/operations/durability.md` already documents. This closes the FUSE_SYNCFS opcode 50 gap that was the original trigger.
   - Replace `fuser::mount2(daemon, mountpoint, &options)` with `kiseki_fuse::mount(daemon, mountpoint, options)`.
7. In `crates/kiseki-client/src/bin/kiseki_client.rs`: the `evict_stale_fuse_mount` helper using `fusermount3 -uz` keeps working unchanged.
8. In `crates/kiseki-client/Cargo.toml`:
   - Replace `fuser = { version = "0.17", optional = true }` with `kiseki-fuse = { workspace = true, optional = true }`.
   - Keep the `fuse = ["kiseki-fuse"]` feature shape; downstream consumers (Python wrapper, kiseki-profile) flip without their own Cargo.toml changes.

### Phase 3 — port the test files

9. `crates/kiseki-client/tests/fuse_linux.rs`: update imports.
10. `crates/kiseki-client/tests/posix_semantics.rs`: update imports.
11. `crates/kiseki-client/tests/concurrent_fuse.rs`: update imports.
12. `crates/kiseki-client/tests/fuse_sync_adjacent_ops.rs`: update imports; **expand the test** — add a probe that drives `syncfs` directly via a kernel `sync(2)` against the mount, verifying our libfuse `syncfs` impl gets called and returns Ok. This is the regression test that pins the original FUSE_SYNCFS gap closed.
13. `crates/kiseki-client/tests/fuse_mount_cleanup.rs`: no `fuser` direct deps; should compile unchanged.
14. **`crates/kiseki-client/tests/fuse_macos.rs`: retire.** Move to `crates/kiseki-client/tests/_archive/fuse_macos.rs.txt` (so git history is preserved) or delete outright. Document the macOS retirement in the ADR-043 amendment §Negative consequences.

### Phase 4 — port BDD step defs

15. `crates/kiseki-acceptance/tests/steps/client.rs`: update FUSE-related step defs.
16. `crates/kiseki-acceptance/tests/steps/gateway.rs`: update FUSE-related step defs.
17. Run the full BDD `@library` and `@integration` FUSE scenarios; verify parity. Recovery target: 100% of currently-green scenarios stay green; 0 new flakes.

### Phase 5 — port kiseki-profile

18. `crates/kiseki-profile/src/main.rs` + `protocols.rs`: update FUSE perf path imports.
19. Re-run a local `kiseki-profile fuse_writeread_64m` matrix; numbers should be at parity or better (libfuse's multi-thread session loop should *help* concurrent FUSE workloads, addressing the `fuser-library single-thread inline dispatch limitation` flagged in user memory).

### Phase 6 — clean up

20. Delete `fuser` from `kiseki-client/Cargo.toml`.
21. `cargo update` to drop fuser from `Cargo.lock`.
22. Verify `cargo tree -p kiseki-client --features fuse` no longer shows fuser anywhere.
23. Run `make verify` (Tier 1) and `make test-slow` (Tier 2) to confirm no regressions.

## Acceptance criteria

This swap is "done" when:

1. `cargo nextest run -p kiseki-client` passes all FUSE-feature-gated tests.
2. The full BDD suite (`make test-slow`) shows the same pass/skip counts as the pre-swap baseline (321 scenarios per CLAUDE.md; 320 pass + 1 `@flaky` retried 2×).
3. Local `kiseki-profile` FUSE perf is at parity or better than the current numbers in `docs/performance/README.md`.
4. `cargo tree -p kiseki-client --features fuse` shows no `fuser` crate.
5. `cargo deny check` passes (license + advisory + bans).
6. **The FUSE_SYNCFS opcode 50 path is exercised** by a test that drives `sync(2)` against a real mount and asserts our impl runs (not the kernel default-ENOSYS fallback).

## macOS posture

`fuser` 0.17 documents support for `osxfuse`/`macfuse` via cargo features; we have a `tests/fuse_macos.rs` file. **Recommend retirement** in this swap:

- libfuse 3.x is Linux-only by upstream design.
- macFUSE has a different ABI (closer to libfuse 2.x); a separate `kiseki-fuse-mac-sys` would be needed to support it. That's a sibling crate effort, not in-scope here.
- No memory entry, no spec, no CLAUDE.md statement claims macOS as a target. The `fuse_macos.rs` test was opportunistic.
- Downstream wrappers (Python via PyO3, C++) build their cdylib on whatever platforms the wrapper user picks; with the `fuse` feature off, kiseki-client builds on macOS unchanged. macOS users get S3 + native + remote-http, just no FUSE.

If macOS FUSE later becomes a requirement, add a follow-up ADR plus `kiseki-fuse-mac-sys` against macFUSE; this swap doesn't preclude it.

## Sequencing relative to remote-nfs removal

The fuser swap and the `kiseki-client::remote-nfs` removal are independent. The fuser swap touches `fuse_*` files; the remote-nfs removal touches `remote_nfs/` files and BDD/perf consumers of NFS-via-userspace-client. Both can land in parallel branches. Recommend the fuser swap lands first (more urgent — concrete bug pattern), the remote-nfs work follows.

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

- ADR-043 (revised): the policy that permits this swap. Amendment in flight to drop ganesha + §D2 process-daemon scope.
- `specs/findings/2026-05-09-adv-gate1-adr043-findings.md`: gate-1 findings; F-M5 (cross-platform), F-M1 (license), F-M8 (fuser-PR alternative), F-M9 (`*-sys` enforceability) carry into this plan.
- `specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md`: the original FUSE_SYNCFS gap finding; this swap closes it.
- `specs/architecture/adr/013-posix-semantics-scope.md`: the operation matrix this swap preserves at parity (no scope change).
- User memory `project_fuse_3phase_pattern`: the 3-phase write-lock pattern in `KisekiFuse` is invariant across the swap; the trait surface preserves it.
- User memory `project_group_commit_contract`: `gateway.fsync_pending()` hook chain is the contract `syncfs` calls into; no change.
