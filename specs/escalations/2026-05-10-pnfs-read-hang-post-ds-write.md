# Escalation: pNFS layout-side read hang persists after DS WRITE wiring

**Date:** 2026-05-10
**From:** implementer (pnfs-ds-write Phase 3 perf gate)
**To:** architect
**Severity:** Phase 3 perf-gate not met. Phase 1 (DS WRITE) is correct in code; Phase 3.1 (WRITE-mode MDS-fallback removal) is correct in code; Phase 3.2 (compose env-var removal) reverted because the kernel-driven NFSv4.1 read path still hangs at 0.5 MB/s when layouts are advertised. The hang sits in the READ path, not WRITE.
**Status:** Open. Phase 3.4 perf-gate failed; Phase 3.3 (BDD scenarios) deferred.

## Finding

The pnfs-ds-write plan (`specs/implementation/pnfs-ds-write.md`) assumed that wiring DS WRITE + dropping `KISEKI_DISABLE_PNFS_LAYOUT=1` would unblock pNFS perf. Phase 1 (DS WRITE buffer + handlers) shipped clean — 5 unit tests + 3 buffer-level tests green; in-process round-trip works (`commit_drains_to_gateway_and_records_redirect`). Phase 3.1 removed the conditional MDS-fallback for WRITE-mode `LAYOUTGET` requests; that change is unconditionally correct now that DS WRITE is wired (independent of the perf issue).

But Phase 3.4 (perf retest with layouts ON) reproduces the **2026-05-09 0.5 MB/s read regression** that originally led to setting `KISEKI_DISABLE_PNFS_LAYOUT=1`. The regression was previously assumed to be the per-file DS-session-establishment tax (per `nfs4_server.rs:1376-1393`). **The fresh trace captured 2026-05-10 disproves that assumption.**

## Trace evidence

3-node compose with `KISEKI_DISABLE_PNFS_LAYOUT=0` (layouts advertised). Single 8 MiB read via `cat`:

```
DEBUG NFSv4 compound xid=2920 ops=[53]                  status=0     elapsed_us=4
DEBUG NFSv4 compound xid=2936 ops=[53, 22, 46]          status=10044 elapsed_us=5  ← GET_DIR_DELEGATION rejected as expected
DEBUG NFSv4 compound xid=2953 ops=[53, 22, 9]           status=0     elapsed_us=3
DEBUG NFSv4 compound xid=2970 ops=[53, 22, 3, 9]        status=0     elapsed_us=4
DEBUG NFSv4 compound xid=2987 ops=[53, 22, 18, 3, 9]    status=0     elapsed_us=55 ← OPEN+ACCESS+GETATTR; got fh4
─── 5.07 second silence — NO compounds on the MDS socket ───
DEBUG NFSv4 compound xid=3003 ops=[53, 22, 9]           status=0     elapsed_us=78 ← post-read GETATTR
```

`dd if=/mnt/nfs41/<etag> bs=1M count=8` reports **5249 ms wall-clock**, almost entirely within the silent gap.

**Critical observations:**

1. **No `op::EXCHANGE_ID` (42) or `op::CREATE_SESSION` (43) in the trace.** The kernel reuses an existing session. The original "per-file session tax" hypothesis from `nfs4_server.rs:1376-1393` is **not the issue**.
2. **No `op::LAYOUTGET` (50) in the trace.** The kernel did NOT request a layout for this file. After OPEN it just goes silent.
3. **No `op::READ` (25) on the MDS socket.** The kernel isn't reading via MDS either.
4. **No DS-side activity logged at port 2052.** `kiseki_gateway::pnfs_ds_server` is silent during the gap.

5 seconds is the order of magnitude of an NFS retry timer. The kernel appears to be waiting for *something* that never arrives, then falling through to a recovery path.

## Three plausible causes

### Hypothesis A — kernel waiting for a delegation callback

Linux NFSv4.1 client's OPEN sequence may include a synchronous wait for a delegation grant, with a fallback timer at ~5s. Our server doesn't grant delegations (`nfs4_server.rs` has no `op_open` delegation logic) and doesn't open a back-channel for callbacks. If the client expects a "no delegation" reply differently than what we emit, it might wait for a callback that never comes.

Check: inspect the OPEN reply's `delegation` field encoding; verify it's `OPEN_DELEGATE_NONE` not `OPEN_DELEGATE_NONE_EXT` (4.1 deferred delegation). RFC 8881 §10.4.2.

### Hypothesis B — kernel attempting LAYOUTGET on a different connection that we silently drop

NFSv4.1 sessions can be bound to multiple connections (BIND_CONN_TO_SESSION). The kernel may issue LAYOUTGET on a NEW TCP connection to port 2049 that's then bound to the existing session. If our `handle_nfs4_connection` doesn't accept the second binding cleanly, the kernel could hang waiting for a reply.

Check: `op::BIND_CONN_TO_SESSION` (41) handling. `tcpdump` on the kernel-client side would reveal whether the kernel is opening a second TCP connection during the gap.

### Hypothesis C — kernel issued LAYOUTGET to port 2052 (DS) directly

If the kernel learned the DS address from a previous `op_getdeviceinfo` and is reusing it, it may issue `LAYOUTGET` directly to the DS, which our DS dispatcher rejects with `NFS4ERR_NOTSUPP` (LAYOUTGET is not in `ALLOWED_DS_OPS` and shouldn't be — DS gives data, not metadata). The kernel would then time out.

Check: trace `pnfs_ds_server::dispatch_ds_compound` — log every op_code received. If LAYOUTGET arrives at the DS, that's the smoking gun.

## What I tried

- **Skip Phase 2 (DS session cache):** the original plan blamed per-file session establishment, but the trace shows no per-file session ops. Skipping was correct.
- **Phase 1 in-process tests pass:** `write_then_read_serves_from_buffer`, `commit_drains_to_gateway_and_records_redirect`, etc. all green. The DS WRITE shape itself works; the problem is upstream of WRITE (in the layout-issuance path).
- **Phase 3.1 in code:** the `iomode >= 2` MDS-fallback block is removed; harmless when layouts are disabled (the env-var gate fires first). Correct unconditionally now that DS WRITE works.
- **Phase 3.2 reverted:** `KISEKI_DISABLE_PNFS_LAYOUT=1` re-set in `docker-compose.3node.yml` so the 3-node perf baseline (read 978 MB/s, write 1668 MB/s) stays healthy.

## Recommended next steps

This is implementer-shaped diagnostic work, but the order matters:

1. **Add DS-side compound trace logging** (similar to the MDS dispatcher's existing `tracing::debug!` line). One-line change in `pnfs_ds_server::dispatch_ds_compound`. Re-run with layouts ON; if LAYOUTGET appears at the DS we have hypothesis C confirmed.
2. **`tcpdump -i any -w /tmp/nfs.pcap port 2049 or port 2052`** while the read hangs. Decode in Wireshark; that will distinguish A vs B vs C definitively.
3. **Inspect `op_open`'s delegation encoding.** Hypothesis A is plausible because the kernel pNFS client is more delegation-aggressive than the legacy NFS client; OPEN_DELEGATE_NONE vs OPEN_DELEGATE_NONE_EXT is a 5.1-only distinction we may have wrong.

If you want me to dig into one of these now, say which. Otherwise, this stays open.

## What's deployable today

- **Phase 1 (DS WRITE buffer + handlers, commit `330b312`):** correct in code, deployable. Doesn't regress anything.
- **Phase 3.1 (WRITE-mode fallback removal):** correct in code, deployable; harmless when layouts are disabled.
- **`KISEKI_DISABLE_PNFS_LAYOUT=1`:** stays in `docker-compose.3node.yml` until layout-side hang is diagnosed. The 2026-05-09 NFSv4.1 baseline (read 923 MB/s, write 1644 MB/s via MDS-inline) is the production-realistic floor.
- **BDD scenarios for DS WRITE round-trip (Phase 3.3):** deferred. Once the layout-side hang is fixed and Phase 3 actually clears its perf gate, scenarios land then. In-process unit tests in `pnfs_ds_server::tests` cover the same logic without needing a kernel client.

## Cross-references

- `specs/implementation/pnfs-ds-write.md` (Phase 3.4 perf-gate now blocked on this).
- `specs/architecture/adr/038-pnfs-layout-and-ds-subprotocol.md` rev 3 (DS WRITE design — correct).
- `specs/escalations/2026-05-10-pnfs-ds-write-design.md` (Option C accepted; Phase 1 implementation matches).
- `specs/performance/2026-05-09-libfuse-swap.md` (the 923 / 1644 NFSv4.1 baseline this falls back to).
- Commits: `330b312` (Phase 1 DS WRITE), `7ff19d4` (ADR-038 rev 3 + plan), `da45687` (original env-var fix that this re-blesses).
- Memory: `project_nfs41_pnfs_disable_compose` (the env-var gate; its rationale is now narrower — was "DS WRITE not wired"; is "layout-side read hang under investigation").
