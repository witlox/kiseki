# WI-2c — Adversarial Gate: Raft Log Store

**Reviewer**: adversary role. **Date**: 2026-04-19.

## Finding: Timestamp in state machine is synthetic, not from request

Severity: **Medium**. Location: `raft_store.rs:192-203`.
The `apply_to_sm` method constructs a `DeltaTimestamp` from the log
index, not from the original `AppendDeltaRequest.timestamp`. The
request timestamp is lost during serialization to `LogCommand`. In
production, the timestamp should be carried through the command log.

**Status**: OPEN — non-blocking (reference impl; production will
serialize the full timestamp in the command).

## Finding: Lock released and re-acquired in append_delta

Severity: **Low**. Location: `raft_store.rs:272-308`.
`append_delta` locks for pre-check, drops lock, then `apply_command`
re-locks. TOCTOU: state could change between the check and the apply.
In production Raft, the leader serializes all commands, so this is
not exploitable.

**Status**: OPEN — non-blocking.

## Summary: 0 blocking.
