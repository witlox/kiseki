# 2026-05-09 follow-up: `sync(1)` kills the kiseki-client daemon

## Symptom

After landing the bugs #1 + #2 + #3 fixes (per-peer cap → 32,
SO_LINGER=0 on TCP-framed client sockets, pre-mount fusermount3
eviction), I retried the docker-FUSE matrix locally to verify the
GCP `compact` `FUSE-daemon-hang at mount` behavior is fixed. The
mount itself now succeeds reliably:

```
Mounting at /mnt/kn via native kiseki://kiseki-node1:9103 (pool=16, cache_mode: bypass)
MOUNT-OK
```

Stat, write, cached read all succeed against the FUSE mount.
The daemon dies (SIGKILL — `bash: line 1: 8 Killed`) the moment
the script runs `sync(1)`.

## Probe (granular daemon survival)

```
=== idle 10s ===           DAEMON-ALIVE-AFTER-IDLE
=== stat root ===          STAT-OK / DAEMON-ALIVE-AFTER-STAT
=== write 4 MiB ===        191 MB/s / DAEMON-ALIVE-AFTER-WRITE
=== read 4 MiB cached ===  233 MB/s / DAEMON-ALIVE-AFTER-READ
=== sync ===               <bash reports>: 8 Killed kiseki-client mount …
```

The daemon is healthy through every step until `sync(1)` runs.
Confirmed at both 4 MiB and 64 MiB write sizes — size-independent.

## What `sync(1)` does to a FUSE mount

`sync(1)` calls `sync(2)`. The kernel iterates all mounted
filesystems and calls `sync_fs` on each. For a FUSE mount this
sends `FUSE_SYNCFS` (kernel ≥ 5.1) to the userspace daemon.
fuser 0.17 doesn't implement `syncfs` on the `Filesystem` trait,
so the default returns `ENOSYS` — kernel marks the op as
unsupported and stops trying.

`ENOSYS` should NOT kill the daemon. Something else is happening.

## Hypotheses (ranked)

1. **Kernel sends a different op our impl panics on.** Maybe
   `FUSE_FORGET` or a release-stage op fires during sync, our
   handler hits an `unwrap`/`expect`, panic propagates, fuser
   session aborts. Need strace or a `-Dpanic=abort` flame to confirm.
2. **fuser 0.17 has a bug with sync_fs.** The 0.17 release on
   crates.io is known to have rough edges (memory: "fuser-library
   single-thread inline dispatch limitation"). A non-impl
   `syncfs` might be replied to incorrectly, kernel sends a fatal
   error, fuser exits.
3. **SIGPIPE from a tracing write that races with stdout
   close.** Less likely — docker stream stays open.

## Why this didn't get caught on the 2026-05-09 GCP run

The GCP run hit the FUSE-daemon-hang at MOUNT (the daemon never
reached a working mount, blocked on the per-peer cap collision).
We never got far enough to issue a `sync` — would have surfaced
the same SIGKILL there too. With bugs #1+#2+#3 fixed, mount now
succeeds and the next bug surfaces.

## Next slice

Repro outside the agent sandbox (the agent's bwrap has
`no_new_privs` set, blocking `fusermount3`'s setuid bit; so I
can't host-mount FUSE myself). Either:

1. **Operator host-side test**: run kiseki-client mount on a real
   Linux host (not in privileged docker), write a small file, run
   `sync`, observe whether daemon dies. If it does, panic
   capture is much easier on the host.
2. **strace inside docker**: `strace -f -p $(pidof kiseki-client)`
   to see the syscall that immediately precedes the daemon exit.
   Look for `rt_sigaction` setup, `FUSE_*` writes, `recvmsg` from
   /dev/fuse just before SIGKILL.
3. **Mock `fuse_session_loop`**: add a unit test that drives
   KisekiFuse's `Filesystem` impl through the methods sync would
   trigger (`statfs`, `release`, `forget`, `flush`, etc.) and
   assert each completes without panicking.

---

## 2026-05-09 update — strace evidence + revised RCA

Path 2 (read fuser source) + path 1 (strace inside docker) ran:

**fuser 0.17 does NOT implement `FUSE_SYNCFS` (opcode 50).** The
opcode is kernel ≥5.1 (Linux 5.1, 2019). fuser's
`Operation::try_from` returns `UnknownOperation` for unrecognized
opcodes; `dispatch_req` maps that to `Errno::ENOSYS` and the
default reply path sends an `ENOSYS` response back to the kernel.
That SHOULD be fine — kernel is supposed to fall back to per-inode
sync.

**strace shows the daemon SURVIVES.** Attached `strace -f` to the
kiseki-client process during sync: thread 183 (the FUSE session
reader) processes FUSE_LOOKUP, FUSE_GETATTR, FUSE_OPEN, FUSE_READ
(1 MiB each), FUSE_WRITE, FUSE_FSYNC (opcode 22), FUSE_FLUSH (25),
FUSE_RELEASE (18) cleanly. Then `read(29, …)` blocks waiting for
the next FUSE message. **No more messages arrive.** The daemon is
healthy; the kernel-side `sync(2)` just hangs without sending any
new FUSE op.

So the earlier "8 Killed" message was misread — that was the
**docker `timeout`** cascading SIGTERM→SIGKILL when the test
script's `sync` command itself hung past the outer 60 s timeout.
The daemon never crashed; it was killed by the test harness when
the test was abandoned.

**Revised diagnosis**: this is a Linux-kernel-FUSE interaction
quirk specific to privileged-docker FUSE mounts on this host. On
a real GCP VM (no docker FUSE), the kernel may behave differently
— we already saw the GCP VM run hung at MOUNT (different code
path, fixed by bugs #1+#2+#3). Whether sync hangs there too is
unverified — the GCP run never reached sync because mount was
broken.

**Pragmatic resolution**:
- **Drop `sync` from the perf test scripts.** `iflag=direct` on
  `dd` is sufficient for cache-miss reads; `vm.drop_caches`
  doesn't strictly need a preceding `sync` for read-only tests.
- **File the fuser-FUSE_SYNCFS gap as an upstream issue** — fuser
  0.17 should implement the opcode (or reply with a more
  kernel-friendly error). Track in a follow-up; not blocking.
- **Add a unit test** (path 3) that drives KisekiFuse's
  Filesystem impl through `flush`, `release`, `fsync`, etc. with
  cheap mocks; pins the no-panic contract for the sync-adjacent
  ops.

The 4th-bug claim above stands corrected: there's no daemon
crash. There IS a sync-hang, but it's not the kiseki-client
crashing, it's the kernel deadlocking. Workaround: don't sync on
docker-FUSE; the read tests don't need it.

## What's commited as of `dbe957f`

Bugs #1, #2, #3 are fixed and pinned by 5 unit tests. The
sync-kills-daemon bug is the next layer to peel. Documenting it
here so the next session has a clean starting point — the symptom
is reproducible on demand (3-node compose + privileged docker FUSE
container + write + sync = SIGKILL).
