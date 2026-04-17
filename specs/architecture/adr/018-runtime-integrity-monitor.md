# ADR-018: Runtime Integrity Monitor

**Status**: Accepted
**Date**: 2026-04-17
**Context**: ADV-ARCH-04 (master key in memory), analyst backpass contention 1

## Decision

A runtime integrity monitor runs as a side process on every storage node,
detecting signs of key material extraction attempts.

### Detection signals

| Signal | Detection method | Severity |
|---|---|---|
| ptrace attachment to kiseki processes | Monitor /proc/pid/status TracerPid | Critical |
| /proc/pid/mem reads on kiseki processes | inotify/audit on /proc/pid/mem | Critical |
| Debugger presence (gdb, lldb, strace) | Process enumeration | High |
| Core dump generation attempt | Monitor core_pattern, catch SIGABRT | Critical |
| Unexpected LD_PRELOAD on kiseki processes | Check /proc/pid/environ at startup | High |
| Process memory mapping changes | Monitor /proc/pid/maps periodically | Medium |

### Response

1. **Alert**: cluster admin + affected tenant admin(s) immediately
2. **Log**: audit event with full context (pid, signal, timestamp)
3. **Optional auto-response** (configurable):
   - Rotate system master key (new epoch, invalidate cached key)
   - Evict cached tenant KEKs (force re-fetch from KMS)
   - Kill the suspect process
4. **Do NOT**: shut down the storage node (availability over prevention —
   the attacker may already have the key; shutting down just causes an outage)

### Performance impact

Negligible. The monitor checks /proc periodically (every 1-5 seconds),
not on every crypto operation. Crypto operations themselves are not
a performance concern:
- HKDF derivation: ~1μs per call, ~25,000 calls/sec at line rate = ~25ms CPU/sec
- AES-256-GCM (the actual encryption): with AES-NI, ~5-10% of one core at 200 Gbps
- The bottleneck is the AEAD data encryption, not key derivation or monitoring

## Consequences

- Additional process per storage node (lightweight)
- Linux-specific (/proc-based detection); needs platform abstraction for other OS
- Not a prevention mechanism — it's detection and response
- False positives possible (legitimate debugging during development);
  monitor should be disableable in dev/test mode
