# Escalation: libfuse syncfs hook not in any tagged release

**Date:** 2026-05-09
**From:** implementer (libfuse-swap Phase 1b)
**To:** architect
**Plan:** `specs/implementation/libfuse-swap.md`
**ADR:** ADR-043 rev 4 (`specs/architecture/adr/043-system-library-ffi.md`)
**Severity:** Plan-level scope conflict (does not block the rest of Phase 1b)
**Status:** **RESOLVED — Option A accepted 2026-05-09.** Plan + ADR amended; implementer-side interim plan is now the durable plan until libfuse 3.19 ships.

## Resolution

Architect accepted **Option A** on 2026-05-09. Concrete consequences:

- ADR-043 §D2 libfuse row updated to note the syncfs deferral and the libfuse 3.19+ retarget.
- libfuse-swap.md §"Why this binding" no longer cites FUSE_SYNCFS as a primary reason; the win is now multi-thread session loop + maintainer-pool depth + production deployment scale.
- libfuse-swap.md Phase 2 step 10's syncfs wiring is omitted; the kernel ENOSYS-fallback (per-inode FUSE_FSYNC routed through `fsync`) preserves correctness.
- libfuse-swap.md Phase 3 step 16's `syncfs`-via-`sync(2)` probe is a follow-up rather than a Phase 4 gate.
- libfuse-swap.md Acceptance criterion 6 marked DEFERRED until libfuse 3.19 ships.
- The `Filesystem` trait in `crates/kiseki-fuse/src/filesystem.rs` does NOT include a `syncfs` method (Phase 1b landed without it).
- When libfuse 3.19 ships:
  - bump `kiseki-fuse-sys` minimum version to 3.19 (one-line change in `build.rs`'s `pkg_config::Config::new().atleast_version("3.19")`),
  - add `syncfs` to the `Filesystem` trait + the trampolines table,
  - add the FUSE_SYNCFS regression test (Acceptance criterion 6),
  - retire this escalation.

## Finding

The plan's §"Migration / Phase 2 step 10" mandates implementing
`syncfs` against `gateway.fsync_pending()`, and Acceptance criterion 6
mandates that the FUSE_SYNCFS opcode 50 path be exercised by a test
that drives `sync(2)` against a real mount and asserts our impl runs
"not the kernel default-ENOSYS fallback."

In libfuse 3.18.2 (the latest tagged release on
github.com/libfuse/libfuse, dated 2025-mid, packaged on Ubuntu 24.04+
and Arch as `fuse3`), `fuse_lowlevel_ops` does **not** have a
`syncfs` callback. Verified by:

- `grep -in syncfs /usr/include/fuse3/*.h` on the dev box returns
  zero hits.
- `pkg-config --modversion fuse3` → `3.18.2`.

The hook DOES exist on libfuse master, added by commit `074f0dfc`
on 2026-01-29 ("fuse_lowlevel: add support for the FUSE_SYNCFS
operation"). Master has NOT been tagged yet — `git tag --sort
=-creatordate | head -1` on libfuse master returns `fuse-3.18.2`.

Implication: Phase 2 step 10 cannot be implemented as written, and
Acceptance criterion 6 cannot be passed, against any libfuse version
shipped by mainstream Linux distros today.

## Why this matters

The plan's §"Why this binding (not the others)" cites "All opcodes
implemented including FUSE_SYNCFS (the 2026-05-09 sync-investigation
gap)" as a primary reason for choosing libfuse over fuser. This is
correct on master but not on any released libfuse — so the
plan-stated win versus fuser does not hold for the pinned-stable
dependency we'd ship in Phase 1.

The dev-box notes (user memory `feedback_hand_rolled_is_hardening`)
explicitly call out the "sync kills daemon" diagnosis as a docker
quirk, not a fuser bug — so the actual end-user impact of the
syncfs gap may be smaller than the plan implies.

## Three options

### Option A — Loosen Acceptance criterion 6; defer syncfs to libfuse 3.19

Accept the libfuse-default ENOSYS-fallback for FUSE_SYNCFS as the
post-swap behavior. The kernel's behavior on receiving ENOSYS for
FUSE_SYNCFS is to fall back to per-inode `FUSE_FSYNC` on every
open inode — which goes through `fuse_lowlevel_ops::fsync` → our
existing `fsync_pending()` hook chain. End-user-visible
`sync(2)` behavior is preserved; the only difference is one
syscall round trip per open inode versus one for the whole FS.

When libfuse 3.19 (or whatever the next tagged release containing
`074f0dfc`) ships:
- bump `kiseki-fuse-sys` minimum version,
- add the `syncfs` callback to `Filesystem`,
- add the regression test (the one Acceptance criterion 6 asks for).

ADR-043 §D2 libfuse row gets a note: "FUSE_SYNCFS lowlevel hook
deferred to libfuse 3.19+; kernel ENOSYS fallback covers
correctness in the interim."

**Pros:** unblocks the swap immediately. The sync-investigation
diagnosis points to docker, not a hard sync-correctness failure.
The kernel's ENOSYS fallback is what every other libfuse-based
filesystem ships with today (sshfs, juicefs, gcsfuse all run
with libfuse 3.18 or earlier).

**Cons:** the plan's "FUSE_SYNCFS opcode 50 gap closed" claim
becomes "deferred to libfuse 3.19" — the swap's primary advertised
win versus fuser is delayed.

### Option B — Vendor libfuse master into the build

Vendor commit `074f0dfc` (or a stable-ish snapshot of master) as
a build-script-managed download, build from source, link statically
or against a vendored shared lib.

**Pros:** Acceptance criterion 6 satisfied immediately.

**Cons:** breaks the plan's "system component, distro-packaged"
justification (ADR-043 §D2). Re-introduces the maintenance shape
of "we vendor a C dep" that ADR-001 / ADR-027 push against.
Operator cost: build container needs cmake + meson + ninja for
libfuse build. CI runner cost ditto. Probably contradicts the
ADR-043 §"D1.1 security posture" intent (we'd track libfuse master
for security advisories rather than the distro's tracked release).

### Option C — Stay on fuser-rs

Cancel the libfuse swap. The stated benefit was FUSE_SYNCFS;
libfuse doesn't actually deliver that benefit in any tagged
release. Phase 0 + 1a stay landed; Phase 1b doesn't begin.

**Pros:** zero net work.

**Cons:** loses the multi-thread session loop benefit (which
**is** a real win on tagged libfuse 3.18.2 — the multi-thread
loop is `fuse_session_loop_mt_31`, available since libfuse 3.10).
Loses the larger-maintainer-pool / wider-deployment-scale
justifications too. Plan's §"Why this binding" §"Stay on fuser"
row already enumerated why this option was rejected; the syncfs
finding doesn't itself flip that decision because the other reasons
(maintainer pool, multi-thread loop, deployment-scale-tested) are
independent of syncfs.

## Recommendation

**Option A.** The multi-thread session loop, the larger-maintainer-
pool, and the bug-history depth (sshfs / juicefs / gcsfuse / ceph-fuse
all production-scale on libfuse 3.x) are the durable wins; FUSE_SYNCFS
is a small optimization on top whose absence is visible only in
benchmarks (one syscall per open inode vs one for the whole FS) and
is invisible to correctness. Option A keeps the swap's primary value
and gives us a clean re-target when libfuse 3.19 ships.

## Implementer's interim plan (does not require architect approval)

While awaiting the architect's resolution:

1. **Skip the syncfs trait method in `kiseki-fuse`'s `Filesystem`
   trait.** Add a top-of-file comment referencing this escalation;
   revisit when the architect chooses A/B/C.
2. **Phase 2 step 10's syncfs wiring is not blocked yet** — Phase 1b
   is the safe-wrapper crate, the trait method shape can change in
   Phase 2 when we port `fuse_daemon.rs`. No syncfs-touching code
   ships in Phase 1b regardless of the architect's choice.
3. The rest of Phase 1b proceeds as planned (reply tokens, bridge,
   ZeroOnCancel, session, mount, the other ADR-013 ops).

## What changes if the architect picks each option

- **Option A picked:** add the §"Platform scope" amendment in
  ADR-043 §D2 noting libfuse 3.19+ for syncfs; defer Acceptance
  criterion 6 with a follow-up; Phase 1b code stays as written.
- **Option B picked:** Phase 0 step 6 added — vendor libfuse master.
  `kiseki-fuse-sys` build.rs gets a vendored-build path. Phase 1b
  syncfs trait method ships. Acceptance criterion 6 stays.
- **Option C picked:** revert Phase 0 + 1a; this escalation closes;
  separate plan for "fuser-rs incremental hardening" needed.

## Cross-references

- `specs/implementation/libfuse-swap.md` §"Migration Phase 2 step 10"
  + §"Acceptance criteria #6" — the claims this escalation contests.
- ADR-043 rev 4 §D2 libfuse row — the policy claim this escalation
  modifies under Option A.
- `specs/findings/2026-05-09-adv-gate1-libfuse-swap-findings.md` —
  this is the kind of finding gate-1 should have caught; round-3
  did not because the gap is in the *upstream library* not in our
  plan, and the round-3 reviewer didn't grep `/usr/include/fuse3/`.
- User memory `feedback_hand_rolled_is_hardening` — the "sync kills
  daemon" diagnosis was a docker quirk, which is partial corroboration
  that this gap doesn't have major end-user-visible impact.
