# ADR-043 Implementation Plan — fuser-rs → libfuse 3.x Swap

**Status:** In progress. Phase 0 + 1a + 1b landed (commits `7bdfdbf` / `570a227` / `2b7fa0c`). Phase 2 in flight as a single rip-and-replace (cfg-flag/dual-compile/holding-period machinery dropped 2026-05-09 because kiseki is pre-production — no deployed clients, no rollback risk; see user memory `project_kiseki_pre_production`). Acceptance criterion 6 loosened per `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md` Option A (accepted 2026-05-09).

## Phase-collapse note (2026-05-09)

The original plan staged the swap across phases 2-6 with a `kiseki_fuse_backend_fuser` (default) + `kiseki_fuse_backend_libfuse` (opt-in) cfg flag, a default-flip at Phase 4 after BDD parity, and a 1-week holding period before fuser was deleted. That machinery was sized for the production-deployment-rollback risk shape.

kiseki has no deployed clients (pre-production; iterate freely). The cfg-flag dance just slows the swap without buying anything. Replaced with a single rip-and-replace: fuser dep dropped, `fuse_daemon.rs` rewritten against `kiseki_fuse::Filesystem` directly, tests + bin updated. If something regresses we revert the commit; the fuser code is in git history.

What this collapses, concretely:
- **Phase 2 step 9** (cfg flag): SKIPPED.
- **Phase 2 step 10**: simplified to "rewrite fuse_daemon.rs against kiseki_fuse::Filesystem; preserve the 3-phase RwLock pattern + ADR-040 fsync hook chain".
- **Phase 4 step 21** (default flip): SKIPPED — the new default is libfuse from the moment the rewrite lands.
- **Phase 4 step 22 holding-period** (≥ 1 week parity in CI before Phase 6): SKIPPED — Phase 6 (fuser deletion) lands in the same commit as the rewrite.
- **Phase 6**: collapsed into the rewrite commit — `fuser` is deleted from `Cargo.toml` and `Cargo.lock` immediately.
- **Risk-register row** "if Phase 4 step 21 regresses, revert one cfg edit": replaced with "revert the rewrite commit; fuser is in git history".

Phase 3 (test ports), Phase 5 (kiseki-profile port), and the §"Sequencing relative to remote-nfs removal" section remain as-is — they're independent of the cfg-flag machinery.
**Created:** 2026-05-09
**Last amended:** 2026-05-09 (round-3 findings F3-H1..H5, all 8 MEDIUMs, all 4 LOWs, both cross-cutting closed inline)
**Tracks:** ADR-043 rev 3.
**Owner role:** implementer.
**Reviews so far:** ADR-043 round-1 (`specs/findings/2026-05-09-adv-gate1-adr043-findings.md`); round-2 on rev-2 + this plan (`specs/findings/2026-05-09-adv-gate1-round2-adr043-findings.md`); round-3 plan-specific gate-1 (`specs/findings/2026-05-09-adv-gate1-libfuse-swap-findings.md`).

## §D6 checklist

Per ADR-043 §D6, the architect MUST document the answer to each criterion before merging the plan; the gate-1 review verifies the answers. Round-3 review challenged criteria 4 and 6 as plausibly **yes** despite originally answered **no**; resolution path **(b) catalogue promotion** chosen — rules and failure modes promoted to project-level catalogues, justifying the answer.

| # | Criterion | Answer | Justification |
|---|---|---|---|
| 1 | New bounded-context boundary (new OS process, new RPC service, new cross-language wire format) | **No** | The new crates `kiseki-fuse-sys` + `kiseki-fuse` are workspace crates loaded into the existing FUSE-daemon process. The libfuse C library is dynamically linked, not a separate process. No new RPC service. |
| 2 | Auth/authz model differs from kiseki's tenant identity propagation | **No** | The kiseki-client FUSE daemon is single-tenant per mount; tenant identity is set at mount time and unchanged by the binding swap. libfuse handles `op_context.uid/gid` exactly as fuser did. |
| 3 | New ubiquitous-language term in `specs/ubiquitous-language.md` | **No** | The `Filesystem` trait, reply tokens, etc. are FUSE-protocol vocabulary not domain-specific kiseki vocabulary. They don't appear in `specs/ubiquitous-language.md`. |
| 4 | License materially changes downstream distribution shape (LGPL ↔ permissive ↔ copyleft transitions; or wrapper LGPL exposure) | **No, with explicit justification** | The transition is fuser (MIT/Apache-2.0) → libfuse (LGPL-2.1) — surface-yes, but resolved as **no** because: (a) LGPL-2.1's dynamic-linking exception keeps kiseki-client's own object code permissive; (b) the Linux distros we target ship libfuse3 as a system package (apt/dnf) — same shape as `libc`, not a vendored dep; (c) downstream wrappers (Python PyO3, C++) build kiseki-client as a cdylib that links libfuse3 dynamically — wrappers/README documents that the `fuse` feature carries LGPL-2.1 dynamic-linking obligations (operational disclosure, not architectural change); (d) the existing `libfabric-sys` precedent under D2 already pulls in LGPL-flavored system libraries with the same exception shape. The license question is operational/legal, not bounded-context architectural. |
| 5 | New packaging steps on GCP perf cluster, dev environment, or downstream wrapper builds | **No** | Adding one apt/dnf package (`libfuse3-dev`) is the same shape as `libfabric-dev` already required by `libfabric-sys`. Phase 0 audits and updates the existing scripts. No new packaging "steps" beyond a single install. |
| 6 | New failure mode (per `specs/failure-modes.md`) or new invariant (per `specs/invariants.md`) | **No, after catalogue promotion** | The plan introduces six wrapper-level invariants (consume-once reply tokens, drop-without-consume = EIO, oneshot bridge for async, EINTR + zeroize on cancel, dedicated session thread, max-pending-ops cap, session-crash handling) and three failure modes. Round-3 finding F3-H2 surfaced this. **Resolution**: invariants promoted to `specs/invariants.md` as **I-FUSE-1..I-FUSE-8** in this same change-set; failure modes promoted to `specs/failure-modes.md` as **F-FUSE-1..F-FUSE-3**. With promotion done, the plan introduces no *new* invariants relative to the catalogues — the answer is **no**. (If catalogue promotion didn't happen, the answer would be **yes** and this plan would require a per-binding ADR.) |
| 7 | Changes any existing ADR's decision | **No** | ADR-013 (POSIX semantics) operation matrix preserved at parity (verified by §"ADR-013 parity check" + Acceptance criterion 10). ADR-040 (group-commit fsync) hook chain is the contract `syncfs` calls into; no change. ADR-001/027 already permit FFI under ADR-043's policy. |

Adversary verifies these answers in gate-1 round 3 findings. If round-N review surfaces evidence that any answer should flip, this table updates and the change re-triggers gate-1 (per ADR-043 §D6 amendment-trigger rule).

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
| **libfuse 3.x via FFI** | **Selected** | Reference impl maintained by FUSE upstream. Multi-thread session loop. Decades of bug history. Production deployment scale: sshfs (millions of installs), juicefs (thousands of clusters), gcsfuse (Google Cloud), dfuse (DAOS), ceph-fuse, gocryptfs. (Originally also cited "FUSE_SYNCFS userspace handling," but that hook is in libfuse master only — added 2026-01-29 in commit `074f0dfc`, not in any tagged release through 3.18.2. Per Option A of `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md`, deferred to libfuse 3.19+; correctness preserved by kernel ENOSYS-fallback to per-inode FUSE_FSYNC.) |
| `fuse3` crate | Rejected | Has both pure-Rust and libfuse backends; unstable choice surface. We'd carry the crate's design churn. Selecting libfuse-direct is more durable. |
| `polyfuse` | Rejected | Pure-Rust reimplementation; async-first and more actively maintained than fuser at the current cadence, but the fundamental issue (pure-Rust protocol reimpl in a small maintainer pool) is the same class of risk we're avoiding. Selecting libfuse-direct settles the question for both reimpl crates simultaneously. |
| Direct `/dev/fuse` + custom protocol code | Rejected | Becomes a fourth Rust FUSE protocol impl. Effort comparable to upstreaming PRs to fuser. No reliability win. |
| **Stay on fuser-rs + upstream PRs** | Rejected (Alternative 6 in the ADR) | The named gap (FUSE_SYNCFS opcode 50) is PR-able, but the structural issue (single-thread inline dispatch, smaller maintainer pool than libfuse, slower release cadence) remains. PR-cycle cost compounds across cumulative gaps; the swap is more durable. |

## Scope and non-scope

**In scope:**
- New `kiseki-fuse-sys` crate: bindgen against libfuse 3.x, raw FFI.
- New `kiseki-fuse` crate: safe Rust wrapper exposing a `Filesystem`-shaped trait covering every ADR-013-supported operation, plus a `mount()` entry point.
- Port `kiseki-client/src/fuse_daemon.rs` to call the new wrapper.
- Update test files in `kiseki-client/tests/` and BDD step defs in `kiseki-acceptance/`.
- Drop `fuser = "0.17"` from `kiseki-client/Cargo.toml`.
- Promote the wrapper-level invariants to `specs/invariants.md` (I-FUSE-1..8) and the failure modes to `specs/failure-modes.md` (F-FUSE-1..3).
- Verify: all FUSE BDD scenarios pass at parity; the recently-landed `fuse_sync_adjacent_ops`, `fuse_mount_cleanup`, `per_peer_cap_collision` tests stay green; the GCP-derived bug regressions stay green; ADR-013 parity verified per §"ADR-013 parity check" below.

**Out of scope (deferred to separate decisions):**
- macOS / Windows FUSE (see §"Platform scope" below).
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

**`links = "fuse3"` collision rule (per round-3 F3-M1):** cargo refuses to build a workspace where two crates declare the same `links` value. To prevent a future transitive dep from re-declaring `links = "fuse3"`, the workspace `Cargo.toml` adds a `[patch.crates-io]` block that pre-emptively redirects the `fuse3` crate to `kiseki-fuse-sys`, and `cargo-deny`'s `bans` rule rejects any non-`kiseki-fuse-sys` crate declaring this `links` value. The libfuse-swap PR also documents this in `CONTRIBUTING.md`.

**bindgen pin policy (per round-3 F3-M2):** `bindgen = "0.69"` is the version current at plan-write time. Pin to the major (0.69) at plan write; bump to next major as part of the libfuse-swap go/no-go review (every 6 months) unless a security advisory forces a sooner bump. CI's `cargo deny check advisories` lane catches this.

`build.rs` uses `pkg-config` to locate libfuse3 and runs bindgen against `wrapper.h`. **pkg-config error message (per round-3 F3-M3):**

```rust
panic!(
    "kiseki-fuse-sys: libfuse3 development headers not found.\n\
     Install:\n  \
       Debian/Ubuntu: apt-get install libfuse3-dev\n  \
       RHEL/Fedora:   dnf install fuse3-devel\n  \
       Arch:          pacman -S fuse3\n\n\
     The `fuse` feature on kiseki-client requires this. \
     To build without FUSE support, omit the feature."
);
```

Pinned minimum libfuse version: **3.10** (the Rocky 9 / RHEL 9 / GCP-perf-cluster shipped version — `fuse3-3.10.2-9.el9`). Kernel-side floor: **≥ 5.4** (FUSE_SYNCFS opcode 50 dispatch by the kernel; userspace handling deferred per Option A above). Future bump target: libfuse 3.19+ when it ships, at which point the `Filesystem` trait gains a `syncfs` method and Acceptance criterion 6 fires for real.

### Release-strategy implication: SONAME pin to Rocky 9 (2026-05-09)

libfuse 3.17 bumped the shared-library SONAME from `libfuse3.so.3` (3.10–3.16) to `libfuse3.so.4` (3.17+). Binaries built against 3.17+ won't load on systems with `libfuse3.so.3` — including Rocky 9 (3.10), Ubuntu 24.04 LTS (3.14), and Debian bookworm (3.14). Discovered the hard way 2026-05-09 when a host-built (Arch, libfuse 3.18 → `.so.4`) `kiseki-client` binary failed to start in `tests/e2e/Dockerfile.fuse-client` (Ubuntu 24.04 base): `error while loading shared libraries: libfuse3.so.4: cannot open shared object file`.

**Release contract:** all kiseki release artifacts (binaries shipped via `release.yml`, the `tests/e2e/Dockerfile.fuse-client` build, the GCP perf-cluster build path in `.gcp-build/build.sh`) MUST be built against **libfuse 3.10–3.16** (SONAME `.so.3`). The build environment is **rockylinux:9** which ships exactly `fuse3-3.10.2`. Any binary built against libfuse 3.17+ is a dev-box artifact only and MUST NOT be deployed.

The dev-box (Arch / Fedora 41 / Debian trixie / Ubuntu 25.04+) has `.so.4`. Local `cargo build` works for development, but produces non-deployable binaries. CI / release / e2e harnesses MUST go through a Rocky 9 build container.

`tests/e2e/Dockerfile.fuse-client` was updated 2026-05-09 to multi-stage build kiseki-client inside `rockylinux:9` (mirroring `.gcp-build/build.sh` step-for-step). The runtime stage is also `rockylinux:9` so SONAME alignment is by construction.

The same constraint applies to `Dockerfile.server` only if/when kiseki-server gains a FUSE-linked dep (today it doesn't link libfuse). Watch for this when the data plane gains in-process FUSE support.

**Future-proofing:** when libfuse 3.19 ships and brings the `syncfs` userspace hook (Option A re-target trigger), the SONAME-`.so.4` migration becomes unavoidable. At that point Rocky 9 (which won't have 3.19 in its base appstream) needs either an EPEL backport, a vendored libfuse build, or the kiseki release strategy moves to a newer base distro. Tracked in the next libfuse-swap go/no-go review.

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
│   ├── bridge.rs       # async-bridge for cross-thread reply finalization (per Safety contract §3)
│   ├── zeroize.rs      # `ZeroOnCancel<Vec<u8>>` for plaintext on cancellation
│   └── request.rs      # Request context (caller_uid, caller_gid, pid)
└── tests/              # smoke-test the wrapper without needing a real /dev/fuse
```

Trait shape (covers every ADR-013-supported operation; see §"ADR-013 parity check" below):

```rust
pub trait Filesystem: Send + Sync + 'static {
    // ---- Inode + lookup ----
    fn lookup(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry);
    fn forget(&self, req: &Request, ino: u64, nlookup: u64);
    fn getattr(&self, req: &Request, ino: u64, reply: ReplyAttr);
    fn setattr(&self, req: &Request, ino: u64, attr: SetAttrRequest, reply: ReplyAttr);  // chmod, chown, truncate

    // ---- File I/O ----
    fn open(&self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen);
    fn read(&self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, reply: ReplyData);
    fn write(&self, req: &Request, ino: u64, fh: u64, offset: i64, data: &[u8],
             write_flags: u32, lock_owner: Option<LockOwner>, reply: ReplyWrite);
    fn flush(&self, req: &Request, ino: u64, fh: u64, lock_owner: LockOwner, reply: ReplyEmpty);
    fn release(&self, req: &Request, ino: u64, fh: u64, flags: i32,
               lock_owner: Option<LockOwner>, flush: bool, reply: ReplyEmpty);
    fn fsync(&self, req: &Request, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty);
    // syncfs DEFERRED to libfuse 3.19+ per Option A of
    // specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md.
    // libfuse 3.18.2 has no fuse_lowlevel_ops::syncfs callback; kernel
    // ENOSYS-fallback gives per-inode FUSE_FSYNC which routes through
    // our `fsync` method. Re-add when libfuse 3.19 ships.
    // fn syncfs(&self, req: &Request, reply: ReplyEmpty);

    // ---- Namespace / dir ops ----
    fn create(&self, req: &Request, parent: u64, name: &OsStr, mode: u32,
              umask: u32, flags: i32, reply: ReplyCreate);
    fn unlink(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty);
    fn mkdir(&self, req: &Request, parent: u64, name: &OsStr, mode: u32,
             umask: u32, reply: ReplyEntry);
    fn rmdir(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty);
    fn rename(&self, req: &Request, parent: u64, name: &OsStr, newparent: u64,
              newname: &OsStr, flags: u32, reply: ReplyEmpty);
    fn opendir(&self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen);
    fn readdir(&self, req: &Request, ino: u64, fh: u64, offset: i64, reply: ReplyDirectory);
    fn releasedir(&self, req: &Request, ino: u64, fh: u64, flags: i32, reply: ReplyEmpty);

    // ---- Symlinks (ADR-013 §"Supported full") ----
    fn symlink(&self, req: &Request, parent: u64, name: &OsStr, link: &Path, reply: ReplyEntry);
    fn readlink(&self, req: &Request, ino: u64, reply: ReplyData);

    // ---- Extended attributes (ADR-013 §"Supported full") ----
    fn getxattr(&self, req: &Request, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr);
    fn setxattr(&self, req: &Request, ino: u64, name: &OsStr, value: &[u8],
                flags: i32, position: u32, reply: ReplyEmpty);
    fn listxattr(&self, req: &Request, ino: u64, size: u32, reply: ReplyXattr);
    fn removexattr(&self, req: &Request, ino: u64, name: &OsStr, reply: ReplyEmpty);

    // ---- POSIX file locks (ADR-013 §"Supported full"; fcntl) ----
    fn getlk(&self, req: &Request, ino: u64, fh: u64, lock_owner: LockOwner,
             start: u64, end: u64, lk_type: i32, pid: u32, reply: ReplyLock);
    fn setlk(&self, req: &Request, ino: u64, fh: u64, lock_owner: LockOwner,
             start: u64, end: u64, lk_type: i32, pid: u32, sleep: bool, reply: ReplyEmpty);

    // ---- Filesystem stats ----
    fn statfs(&self, req: &Request, ino: u64, reply: ReplyStatfs);

    // Default impls return ENOSYS only for ops NOT in ADR-013's supported list
    // (e.g., copy_file_range, fallocate, ioctl, poll, bmap). libfuse handles
    // the kernel-side fallback for those.
}
```

Aim: a `KisekiFuse` impl that today derives `fuser::Filesystem` derives `kiseki_fuse::Filesystem` instead, with mostly-1:1 method signature changes plus the ADR-013 ops the existing impl is missing (see §"ADR-013 parity check").

### ADR-013 parity check (added round-3 per F3-H3)

ADR-013 §"Supported (full semantics)" enumerates the POSIX operations kiseki commits to. Today's `fuse_daemon.rs` implements 14 FUSE methods — fewer than ADR-013 promises. The libfuse swap is the natural moment to close the gap (or, per §"Sequencing" below, identify which ops are in fact gaps in the existing fuser impl that pre-date this swap). The wrapper trait listed above covers every ADR-013-supported op.

| ADR-013 op | FUSE method backing it | Today's fuse_daemon.rs status |
|---|---|---|
| open, close (release) | `open`, `release` | ✅ implemented |
| read, write | `read`, `write` | ✅ |
| create, unlink | `create`, `unlink` | ✅ |
| mkdir, rmdir | `mkdir`, `rmdir` | ✅ mkdir; **❌ rmdir** (must add in port) |
| rename (within namespace) | `rename` | ✅ |
| stat, fstat, lstat | `getattr`, `lookup` | ✅ |
| **chmod, chown** | `setattr` | **❌ missing** — must add |
| readdir, readdirplus | `readdir` | ✅ |
| **symlink, readlink** | `symlink`, `readlink` | **❌ missing** — must add |
| **truncate, ftruncate** | `setattr` (size) | **❌ missing** — must add |
| fsync, fdatasync | `fsync` | ✅ |
| **getxattr, setxattr, listxattr, removexattr** | `getxattr`, `setxattr`, `listxattr`, `removexattr` | **❌ missing** — must add (4 ops) |
| **POSIX file locks (fcntl)** | `getlk`, `setlk` | **❌ missing** — must add |
| O_APPEND | (handled in `write` op flags) | ✅ |
| O_CREAT, O_EXCL | (handled in `create` op flags) | ✅ |

**Phase 2 step 7** (below) is amended: the new `Filesystem` trait MUST cover every ADR-013-supported operation, with explicit default impls only where ADR-013 marks the op as unsupported. The "❌ missing" ops above are added during the port — that's a real scope expansion vs. simply re-wiring fuser → libfuse, and it's intentional.

**Phase 4 step 18** (below) is amended: every ADR-013-supported operation has at least one `@integration` BDD scenario that exercises it through the new path. Acceptance criterion 10 (below) verifies this.

If during Phase 2 it turns out the "❌" ops are unimplementable against the current `KisekiFuse` data plane (e.g., xattr storage doesn't exist in the composition store yet), file separate spec-only invariants and escalate to architect via `specs/escalations/` rather than silently regressing ADR-013.

### Safety contract (round-3 expansion of round-2 contract)

The `kiseki-fuse` safe wrapper enforces an explicit contract to prevent the use-after-free / leaked-slot bugs that plague FFI'd FUSE filesystems. The contract is the audit target for the gate-2 review on `kiseki-fuse` (see Acceptance criterion 8 below). Each rule is also promoted to `specs/invariants.md` as **I-FUSE-1..I-FUSE-8** in this same change-set.

#### Rule 1 — Reply tokens are consume-once (I-FUSE-1)

`ReplyAttr`, `ReplyData`, `ReplyEmpty`, `ReplyWrite`, `ReplyCreate`, `ReplyEntry`, `ReplyOpen`, `ReplyDirectory`, `ReplyStatfs`, `ReplyXattr`, `ReplyLock` each consume themselves on `.attr(...)`, `.data(...)`, `.ok()`, etc. After consume, the token is dropped without further FFI calls.

#### Rule 2 — Drop-without-consume = EIO + counter-incremented warning (I-FUSE-2)

If a Rust handler returns or panics without consuming the reply token, the `Drop` impl issues `fuse_reply_err(req, EIO)` to libfuse, increments a Prometheus counter `kiseki_fuse_drop_without_consume_total{op=...}`, and logs at WARN level. Debug builds additionally panic to surface the bug during development. This guarantees libfuse never holds a request slot indefinitely; correctness over cleanliness. The counter alerts ops on non-zero count (operator runbook is part of Phase 6).

#### Rule 3 — Reply tokens cross async boundaries through an explicit oneshot bridge (I-FUSE-3)

Reply tokens are not directly `Send` across the libfuse-session-thread / tokio-task boundary. The wrapper provides a `tokio::sync::oneshot`-bridged adapter that hands a typed result from the async handler back to the session thread for finalization.

**3.1.** The bridge type is `tokio::sync::oneshot::{Sender, Receiver}<Result<ReplyResult, FuseError>>`. The session thread blocks on `recv()` with a per-request bounded timeout (default **30 s**, configurable via `KisekiFuseConfig::handler_timeout`); timeout returns `EIO` to libfuse.

**3.2.** In-flight bridges are tracked in a `DashMap<RequestId, BridgeHandle>` on the wrapper. On consume the entry is removed. On cancel-then-late-arrival (the async task completes after cancellation; the receiver is gone), the late-arriving sender's `send()` returns `Err(SendError)` and the wrapper logs at WARN with the request ID.

**3.3.** The wrapper enforces `max_pending_ops` (default **1024**) on the in-flight bridge map; when the cap is hit, new FUSE requests are immediately replied with `EAGAIN` rather than queued. Configurable via `KisekiFuseConfig::max_pending_ops`. (This is **I-FUSE-7**.)

**Why oneshot over alternatives** (per round-3 F3-L2): `oneshot` provides exact send-once semantics — sender consumed on send, receiver detects dropped sender via `RecvError`. `mpsc(1)` allows multiple senders, which is wrong for our shape. A custom waker would be more code with less audit history.

#### Rule 4 — Cancellation produces EINTR; plaintext zeroized on drop (I-FUSE-4)

When a tokio task driving a handler is cancelled (its future dropped), the wrapper detects the unfinished bridge and replies `EINTR` to libfuse before destroying the request. The kernel sees a clean error, the slot is freed.

**4.1.** Async handlers MUST zeroize plaintext buffers on cancellation. The wrapper provides a `ZeroOnCancel<Vec<u8>>` smart pointer that calls `zeroize::zeroize()` on `Drop`; handlers wrap their plaintext returns with this type so the cancellation path zeroes regardless of which `await` point was cancelled. This extends the existing `Zeroizing<Vec<u8>>` discipline used in `mem_gateway::DecryptCache` (per the recent ADR-043-rev-1-era fix `de5a239`).

#### Rule 5 — Session loop runs on a dedicated `std::thread` (I-FUSE-5)

libfuse's session is C-thread-based and cannot be driven from the tokio executor directly. The wrapper spawns a dedicated `std::thread` named `kiseki-fuse-session` (visible in `top -H` / `perf top` for diagnosis), NOT `tokio::task::spawn_blocking`. The dedicated thread is unconditional — chosen over `spawn_blocking` to avoid competing with kiseki's other blocking work for tokio's blocking-pool slots, which has a soft cap of 512 (per round-3 F3-M4).

Opcodes received by the session thread are routed to async handlers via the bridge in Rule 3.

#### Rule 6 — Filesystem trait bounds: `Send + Sync + 'static` (I-FUSE-6)

The trait itself is `Send + Sync + 'static`. The tight constraint is the reply-token lifetime (per Rules 1-3), not the handler's own state. Reply tokens are `!Send` outside the bridge; sending one via direct `tokio::spawn` fails to compile (verified by Acceptance criterion 8's compile-fail test).

#### Rule 7 — Max pending FUSE ops bounded (I-FUSE-7)

See Rule 3.3.

#### Rule 8 — Session-thread crash → process abort or remount (I-FUSE-8)

If the libfuse session thread exits unexpectedly (panic, EIO from kernel `/dev/fuse`, signal), the wrapper detects this via the thread's join handle and:

- **Default**: aborts the kiseki-client process. Fail-fast preserves the pre-swap shape (today's fuser session crash effectively crashes the daemon process).
- **Operator opt-in** via `KisekiFuseConfig::auto_remount = true`: attempts a single re-mount with the same options. If the re-mount also crashes, falls back to abort. Auto-remount is OFF by default because it can mask serious bugs (a bad libfuse version or kernel mismatch keeps re-crashing).

Documented as **F-FUSE-3** in `specs/failure-modes.md`.

The gate-2 audit on `kiseki-fuse` validates this contract is honored in code, with concrete attack tests:
- Cancel a handler future mid-call; assert `EINTR` reaches the kernel and any plaintext in the cancelled future is zeroed (verified via `unsafe { std::ptr::read_volatile(...) }` against the post-drop memory in the test).
- Drop a reply token without consume; assert `EIO` is returned (debug build: assert panic; release build: assert counter incremented).
- Kill the session thread (synthetic panic); assert the wrapper's join-handle observer fires and the process aborts.
- Send a reply token to another thread without the bridge; assert this fails to compile.
- Saturate `max_pending_ops`; assert further requests get `EAGAIN`.

## Migration (file-by-file)

### Phase 0 — pre-work (no behavior change)

1. Confirm `libfuse3-dev` (Debian/Ubuntu) / `fuse3-devel` (RHEL/Fedora) is installed in dev + CI environments. Update `.github/workflows/ci.yml` to `apt-get install -y libfuse3-dev` before the `kiseki-client --features fuse` test lane.
2. Confirm runtime presence of `/dev/fuse` and `fusermount3` on test runners (already required for the existing `fuser` 0.17 path; this is a no-op).
3. **Audit GCP perf-cluster build paths**: `.gcp-build/build.sh` and the `infra/gcp/` setup scripts must install `libfuse3-dev` / `fuse3-devel` before building kiseki-client with `--features fuse,remote-http,native`. Without this, the next perf-cluster run breaks at `kiseki-fuse-sys`'s `pkg-config` lookup. **Phase 0 ships a merged PR to those scripts BEFORE Phase 1 begins.**
4. **Audit downstream wrapper builds**: the Python (PyO3) and C++ wrappers' build scripts must either install `libfuse3-dev` or document that the `fuse` feature is off-by-default. Wrappers/README updated in Phase 6.
5. **Audit CI-runner privileges (added round-3 per F3-M5)**: Acceptance criterion 6 requires a kernel-mount FUSE test (driving real `sync(2)` against a real mount). CI runners need `/dev/fuse` access + `fusermount3` setuid-root. Existing FUSE tests don't actually mount — they exercise `Filesystem` traits directly — so this is a NEW CI requirement. Phase 0 verifies the `fuse_linux.rs` and `fuse_sync_adjacent_ops.rs` paths work on the CI runners; if not, document the privilege requirement in CONTRIBUTING.md and gate criterion 6 on a privileged CI lane.

### Phase 1 — land the new crates (no kiseki-client changes yet)

6. Add `crates/kiseki-fuse-sys/` with bindgen against libfuse 3.10+. Smoke test: build succeeds, `cargo doc -p kiseki-fuse-sys` shows the bound types.
7. Add `crates/kiseki-fuse/` with the safe wrapper. Cover the trait surface listed above. Add a smoke test that mounts an empty filesystem and reads `getattr` of the root, asserts no panic. Add the gate-2-targeted safety-contract tests from §"Safety contract" (cancellation/drop/compile-fail/session-crash/max-pending).
8. Mark `kiseki-fuse` as a workspace member; nothing depends on it yet.

### Phase 2 — port `kiseki-client/src/fuse_daemon.rs`

9. **Introduce the `kiseki_fuse_backend` cfg switch.** Add two mutually-exclusive cargo features to `kiseki-client`: `kiseki_fuse_backend_fuser` (default-enabled at Phase 2 merge — keeps current behavior!) and `kiseki_fuse_backend_libfuse` (the new path). Both compile during phases 2-5. The default flips to `kiseki_fuse_backend_libfuse` only at Phase 4 merge (after the BDD parity check); this makes the rollback path the always-default during the risky window (per round-3 F3-M6). CI matrix gate: during phases 2-3 the libfuse-feature build is allowed-to-fail if the fuser-feature build is green (caught by a CI matrix entry, not a workflow override).
10. In `crates/kiseki-client/src/fuse_daemon.rs`:
    - Wrap the existing `use fuser::{...}` in `#[cfg(feature = "kiseki_fuse_backend_fuser")]` and add a parallel `#[cfg(feature = "kiseki_fuse_backend_libfuse")] use kiseki_fuse::{...};`.
    - Duplicate the handler-impl block under both cfgs initially (the new branch is the working port; the legacy branch keeps fuser-shape unchanged).
    - **Implement the "❌ missing" ADR-013 ops** (chmod/chown/truncate via setattr; symlink/readlink; xattr quartet; getlk/setlk) — see §"ADR-013 parity check." If any op is unimplementable against the current `KisekiFuse` data plane, file as a separate spec-only invariant and escalate.
    - Update reply type names where the wrapper diverges from fuser's vocabulary.
    - **(deferred per Option A — 2026-05-09)** Originally: implement `syncfs` against `gateway.fsync_pending()`. libfuse 3.18.2 has no userspace hook for FUSE_SYNCFS; kernel ENOSYS-fallback dispatches per-inode `FUSE_FSYNC` which already routes through our `fsync` method on `gateway.fsync_pending()` for `force_fsync = true`. End-user `sync(2)` semantics preserved. Re-enable when libfuse 3.19 ships.
    - Replace `fuser::mount2(daemon, mountpoint, &options)` with `kiseki_fuse::mount(daemon, mountpoint, options)` (libfuse branch).
11. In `crates/kiseki-client/src/bin/kiseki_client.rs`: the `evict_stale_fuse_mount` helper using `fusermount3 -uz` keeps working unchanged.
12. In `crates/kiseki-client/Cargo.toml`:
    - Add `kiseki-fuse = { workspace = true, optional = true }` alongside the existing `fuser = { version = "0.17", optional = true }` (both kept until Phase 6).
    - Update the `fuse` feature to: `fuse = ["kiseki_fuse_backend_fuser"]` initially (Phase 2 merge); flips to `fuse = ["kiseki_fuse_backend_libfuse"]` at Phase 4 merge. `kiseki_fuse_backend_libfuse = ["dep:kiseki-fuse"]`; `kiseki_fuse_backend_fuser = ["dep:fuser"]`. Downstream consumers (Python wrapper, kiseki-profile) flip without their own Cargo.toml changes.

### Phase 3 — port the test files

13. `crates/kiseki-client/tests/fuse_linux.rs`: update imports.
14. `crates/kiseki-client/tests/posix_semantics.rs`: update imports.
15. `crates/kiseki-client/tests/concurrent_fuse.rs`: update imports.
16. `crates/kiseki-client/tests/fuse_sync_adjacent_ops.rs`: update imports. **Acceptance criterion 6 is deferred** to libfuse 3.19+ per Option A; the syncfs-via-`sync(2)` probe is a follow-up. The test continues to drive `fsync(2)` per inode via existing `flush`/`fsync` paths, which are the kernel's ENOSYS-fallback target.
17. `crates/kiseki-client/tests/fuse_mount_cleanup.rs`: no `fuser` direct deps; should compile unchanged.
18. **`crates/kiseki-client/tests/fuse_macos.rs`: delete.** Per the §"Platform scope" decision macOS is officially out; the test is removed outright (git history preserves the prior content for reference).

### Phase 4 — port BDD step defs + flip the default

19. `crates/kiseki-acceptance/tests/steps/client.rs`: update FUSE-related step defs (libfuse path).
20. `crates/kiseki-acceptance/tests/steps/gateway.rs`: update FUSE-related step defs.
21. **Flip the cfg-flag default**: `kiseki-client/Cargo.toml`'s `fuse = ["kiseki_fuse_backend_fuser"]` → `fuse = ["kiseki_fuse_backend_libfuse"]`. Both backends still compile; CI runs the libfuse path as default and the fuser path as the @smoke-only fallback.
22. **Run the full BDD `@library` and `@integration` FUSE scenarios + the project-level `@flaky` scenarios (D-10 cross-stream and 6-node EC PUT) called out in CLAUDE.md and the risk register.** Verify parity. Recovery target: 100% of currently-green scenarios stay green; the `@flaky` scenarios at no worse than baseline retry rate (per round-3 F3-M8). **Phase 4 parity** for go-to-Phase-6 purposes is defined as: BDD pass count identical to pre-swap baseline AND project-level `@flaky` count unchanged AND median wall-clock per FUSE scenario within ±20% of baseline (per round-3 F3-L3). CI tracks all three across the 7-day window via the existing nextest report.

### Phase 5 — port kiseki-profile

23. `crates/kiseki-profile/src/main.rs` + `protocols.rs`: update FUSE perf path imports.
24. Re-run a local `kiseki-profile fuse_writeread_64m` matrix; numbers should be at parity or better (libfuse's multi-thread session loop should *help* concurrent FUSE workloads, addressing the `fuser-library single-thread inline dispatch limitation` flagged in user memory).

### Phase 6 — clean up

Phase 6 only lands once **Phase 4 parity (defined above) has held for ≥ 1 week** in CI **and** Phase 5 has had **≥ 1 perf matrix run at parity** (per §"Rollback procedure" mitigations).

25. Remove the `kiseki_fuse_backend_fuser` feature from `kiseki-client/Cargo.toml`; collapse the cfg-`if` in `fuse_daemon.rs` to libfuse-only.
26. Delete `fuser = { version = "0.17", optional = true }` from `kiseki-client/Cargo.toml`.
27. `cargo update` to drop fuser from `Cargo.lock`.
28. Verify `cargo tree -p kiseki-client --features fuse` no longer shows fuser anywhere.
29. Run `make verify` (Tier 1) and `make test-slow` (Tier 2) to confirm no regressions.

## Acceptance criteria

This swap is "done" when:

1. `cargo nextest run -p kiseki-client` passes all FUSE-feature-gated tests (libfuse branch).
2. The full BDD suite (`make test-slow`) shows the same pass/skip counts as the pre-swap baseline (321 scenarios per CLAUDE.md; 320 pass + 1 `@flaky` retried 2×) AND the two project-level `@flaky` scenarios at no worse than baseline retry rate.
3. Local `kiseki-profile` FUSE perf is at parity or better than the current numbers in `docs/performance/README.md`.
4. `cargo tree -p kiseki-client --features fuse` shows no `fuser` crate.
5. `cargo deny check` passes (license + advisory + bans, including the `links = "fuse3"` ban rule).
6. **The FUSE_SYNCFS opcode 50 path is exercised** by a test that drives `sync(2)` against a real mount and asserts our impl runs (not the kernel default-ENOSYS fallback). **DEFERRED to libfuse 3.19+** per Option A of `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md` (accepted 2026-05-09): libfuse 3.18.2 has no `fuse_lowlevel_ops::syncfs` hook to wire into. Until libfuse 3.19 ships, kiseki replies ENOSYS to FUSE_SYNCFS and the kernel falls back to per-inode FUSE_FSYNC, which our `fsync` method handles on `gateway.fsync_pending()`. The acceptance criterion fires when libfuse 3.19 lands; until then, this row is satisfied-by-deferral.
7. ADR-043 §D2 libfuse row is updated with D1.1 security-posture data: upstream CVE history reference (link to `libfuse/libfuse` GitHub Security Advisories), the most recent CVE id and the kiseki-internal time-to-patch we'd commit to (or "no CVE in window" if the advisories list is clean at acceptance), and the kiseki triage SLA (default per D1.1: CRITICAL ≤ 7 days, HIGH ≤ 30 days, MEDIUM at next release). Locks the plan's "done" state to the policy's stated requirement.
8. `kiseki-fuse` crate has a gate-2 audit pass against the §"Safety contract" above; concretely: cancellation tests (cancel a handler future; assert `EINTR` reaches the kernel and plaintext is zeroed), drop-without-consume tests (drop a reply token; assert `EIO` is returned in release, panic in debug, counter incremented), session-crash test (synthetic panic; assert process aborts), max-pending-ops test (saturate; assert `EAGAIN`), and a compile-fail test asserting reply tokens cannot be sent across the C-thread boundary without the bridge.
9. **GCP perf-cluster build scripts** (`.gcp-build/build.sh`, `infra/gcp/`) and **downstream wrapper build paths** (Python PyO3, C++) are updated by a **merged PR before Phase 1 of this plan begins**. The next GCP perf-cluster run after Phase 6 must succeed at the kiseki-client build step; this is verified by the `infra/gcp/benchmarks/perf-suite-*.sh` smoke check.
10. **(added round-3 per F3-H3) ADR-013 parity verified.** Every operation in ADR-013 §"Supported (full semantics)" has a covering `@integration` BDD scenario that exercises it through the new libfuse path. The "❌ missing" rows in §"ADR-013 parity check" are addressed (implemented in Phase 2 or escalated as spec-only invariants).

## Platform scope (Linux only; macOS / Windows out of scope)

**macOS FUSE compatibility is not a kiseki target.** Architect's decision recorded 2026-05-09 (round-2 closure of F2-L3): kiseki's positioning is HPC/AI workloads, all of which run on Linux. The `fuser` 0.17 dependency happened to compile on macOS via `osxfuse`/`macfuse`, and `tests/fuse_macos.rs` was opportunistic coverage of that side-effect — never a committed product surface.

Concrete consequences of this scope decision:

- `crates/kiseki-client/tests/fuse_macos.rs` is **deleted** (Phase 3 step 18, no archive). The test was never load-bearing; deleting reduces drift.
- The `fuse` feature on `kiseki-client` is **Linux-only** post-swap. On macOS, building with `--features fuse` returns a compile error from `kiseki-fuse-sys`'s `build.rs` ("libfuse3 not available on this platform; the `fuse` feature is Linux-only"). Building **without** the `fuse` feature works on macOS unchanged — kiseki-client still provides S3, native, and remote-http on macOS.
- Downstream wrappers (Python PyO3, C++) document that the `fuse` feature is Linux-only; their macOS build paths must omit it.

Windows FUSE (WinFsp) is also out of scope, never claimed; this is a no-op restatement.

Re-introducing macOS FUSE would require: (a) a new ADR-043 §D2 row for `macfuse-sys` under the same FIPS-isolation rule, (b) a new `kiseki-fuse-mac-sys` crate against macFUSE's libfuse-2.x-shaped ABI, (c) a parallel safe-wrapper layer or an abstraction over `kiseki-fuse` and `kiseki-fuse-mac`. None of this is planned; this section exists to record the explicit decision, not to imply a future path.

## Sequencing relative to remote-nfs removal

The fuser swap and the `kiseki-client::remote-nfs` removal are independent. The fuser swap touches `fuse_*` files; the remote-nfs removal touches `remote_nfs/` files and BDD/perf consumers of NFS-via-userspace-client. Both can land in parallel branches. Recommend the fuser swap lands first (more urgent — concrete bug pattern), the remote-nfs work follows.

## Go/no-go review

Six months after `fuser` is removed from `Cargo.lock` (final Phase 6 step), the architect runs a review of the libfuse swap against the criteria below. Date target is set when the final phase merges and recorded in ADR-043 §"Review schedule" alongside other binding rows.

| Criterion | Threshold | Source |
|---|---|---|
| Tier_1 perf-cluster numbers | within ±10% of pre-swap baseline (2026-05-09 GCP `compact` numbers in `specs/performance/`) | next `kiseki-profile` matrix run on the GCP cluster |
| Unpatched libfuse CVE in the kiseki-pinned version range | 0 CRITICAL or HIGH older than the D1.1 SLA | `libfuse/libfuse` GitHub Security Advisories review |
| FUSE BDD scenario flake rate | ≤ baseline + 1 scenario flagged `@flaky` | full BDD suite (Tier 2) trailing-30-day pass rate |
| FUSE_SYNCFS regression coverage | per Option A: deferred until libfuse 3.19 ships; the per-inode FUSE_FSYNC fallback path (`fsync` op) is green | `cargo nextest` |

If any criterion fails, mark the libfuse row Rejected per ADR-043 §D5 and revert via §"Rollback procedure" below.

## Rollback procedure

Each phase merges as a standalone PR; rollback granularity matches:

| Phase | If we need to roll back this phase | Cost |
|---|---|---|
| 1 (new crates) | Revert the PR. `kiseki-fuse-sys` + `kiseki-fuse` remain unused; nothing else broke. | Trivial — revert. |
| 2 (port `fuse_daemon.rs`) | The port is gated by the `kiseki_fuse_backend` cargo feature with values `libfuse` (new) and `fuser` (default through Phase 4 — see step 21). Both code paths compile during phases 2-5. Rollback during phases 2-3 means CI was already running fuser as default; no action needed. | Zero — fuser is the default. |
| 3-4 (test files + BDD) | If Phase 4 step 21 (the default flip) regresses, revert that one-line edit. Both backends still compile. | One-line cfg edit. |
| 5 (kiseki-profile) | Same cfg flag. | One-line cfg edit. |
| 6 (drop `fuser` dep) | `fuser` is removed from `Cargo.toml`. Rollback re-adds it (one-line edit) and re-introduces the cfg selector in `fuse_daemon.rs`. | ~30 lines. |
| Post-Phase-6 (production rollback) | `fuser` is gone from `Cargo.lock`. Re-introduce as a fresh dep + re-port `fuse_daemon.rs` from the (still-in-git-history) pre-swap version. | 4-6 days, comparable to the original swap. |

Mitigations against expensive late rollback:

- Phase 4's default flip only happens after BDD parity check passes.
- Phase 6 only lands once Phase 4 parity (defined in step 22) has held for **≥ 1 week** in CI (no new flakes; no functional regression in the `@library` or `@integration` lanes; `@flaky` retry rate at baseline).
- Phase 6 only lands once Phase 5 perf has been at parity for **≥ 1 perf matrix run**.
- Both pre-conditions are listed as Acceptance criteria above (criteria 2-3); Phase 6 cannot be merged before they pass.

## Risk register

| Risk | Mitigation |
|---|---|
| libfuse 3.10 features (FUSE_SYNCFS) require kernel ≥ 5.1; older systems regress | CLAUDE.md doesn't pin a minimum kernel; document min kernel ≥ 5.4 in operator docs (5.4 is Ubuntu 20.04 LTS, well-established). ADR-043 §D2 libfuse row already documents this. |
| BDD scenarios assume fuser's exact reply ordering or threading model | libfuse's multi-thread session loop changes concurrency shape. Phase 4 step 22 explicitly covers the `@flaky` `D-10 cross-stream` and `6-node EC PUT` scenarios; if the multi-thread reordering changes their pass rate, flag for separate investigation. |
| `kiseki-fuse-sys` build fails on a contributor's machine without `libfuse3-dev` | `build.rs` panics with the explicit `pkg-config` error message above (§"crates/kiseki-fuse-sys/"); CONTRIBUTING.md gets a one-line install note. |
| FFI safety bugs in `kiseki-fuse-sys` (use-after-free in a reply, session-thread crash leaks plaintext, …) | Safe wrapper layer in `kiseki-fuse` is the audit point. Adversary review on `kiseki-fuse` crate before merge — gate-2 audit at minimum (per Acceptance criterion 8). |
| Cargo `links = "fuse3"` collision from a future transitive dep | Pre-emptive `[patch.crates-io]` block on the `fuse3` crate; `cargo-deny bans` rule rejects competing `links = "fuse3"` declarations from non-`kiseki-fuse-sys` crates (per F3-M1). |
| ADR-013-supported ops (xattr, file locks, etc.) discovered unimplementable against current `KisekiFuse` data plane during Phase 2 | Escalate to architect via `specs/escalations/` rather than silently regressing ADR-013; document as spec-only invariants. |

## Test-impact estimate

- New tests: ~8-12 (libfuse-specific syncfs probe, smoke test in `kiseki-fuse`, 5 wrapper-level safety-contract tests, ADR-013 parity tests for the previously-missing ops).
- Modified tests: 6 (`kiseki-client/tests/fuse_*.rs` + 2 BDD step files).
- Retired tests: 1 (`fuse_macos.rs`).
- Net: workspace test count moves from ~1650 unit + 321 BDD to ~1660 unit + 321 BDD (no BDD scenario count change; ADR-013 parity covered by existing `@integration` scenarios + new ones for any newly-implemented ops).

## Effort estimate

- Phase 0-1 (new crates): 1-2 days.
- Phase 2 (port `fuse_daemon.rs` including the ADR-013-missing ops): 2-3 days (revised up from 1-2 to account for the previously-missing ops).
- Phase 3 (port test files): 0.5 day.
- Phase 4 (BDD step defs + verification + default flip): 1 day.
- Phase 5-6 (perf + cleanup): 0.5 day.

**Total: 5-7 days of focused work for one engineer.** Wall-clock estimate: **calendar timeline 1-2 weeks under typical interleaving**; longer if any phase fails review (per round-3 F3-L1). Net code change: replaces ~518 lines of `fuser`-using `fuse_daemon.rs` with similar-size + ADR-013-missing-ops libfuse-using version (estimate ~700-900 lines), plus two new crates totaling ~600-1000 lines (the `*-sys` is mostly bindgen output; the safe wrapper is the real engineering).

## Cross-references

- ADR-043 rev 3: the policy that permits this swap. Architect's §D6 checklist answers documented at the top of this plan.
- `specs/findings/2026-05-09-adv-gate1-adr043-findings.md`: round-1 gate-1 findings (the rev-1 review).
- `specs/findings/2026-05-09-adv-gate1-round2-adr043-findings.md`: round-2 findings on rev-2 + this plan.
- `specs/findings/2026-05-09-adv-gate1-libfuse-swap-findings.md`: round-3 plan-specific gate-1 findings; all 5 HIGHs / 8 MEDIUMs / 4 LOWs / 2 cross-cutting closed inline in this rev.
- `specs/invariants.md` §"FUSE wrapper invariants (libfuse-swap)": **I-FUSE-1..I-FUSE-8** — promoted from this plan's §"Safety contract."
- `specs/failure-modes.md` §"FUSE wrapper failures (libfuse-swap)": **F-FUSE-1..F-FUSE-3** — promoted from this plan.
- `specs/performance/2026-05-09-gcp-compact-fixes-verify/sync-kills-daemon.md`: the original FUSE_SYNCFS gap finding; this swap closes it (Acceptance criterion 6).
- `specs/architecture/adr/013-posix-semantics-scope.md`: the operation matrix this swap MUST cover at parity; explicit verification per §"ADR-013 parity check" + Acceptance criterion 10.
- User memory `project_fuse_3phase_pattern`: the 3-phase write-lock pattern in `KisekiFuse` is invariant across the swap; the trait surface preserves it.
- User memory `project_group_commit_contract`: `gateway.fsync_pending()` hook chain is the contract `syncfs` calls into; no change.
