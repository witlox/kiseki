# BDD Step Audit — Phase Q2 (2026-04-22)

456/456 scenarios pass, but ~126 steps are behaviorally shallow.
This audit identifies steps that pass without testing the domain
behavior they claim to test.

## Summary

| File | Total Steps | Shallow | % | HIGH | MED | LOW |
|------|-------------|---------|---|------|-----|-----|
| admin.rs | 87 | 31 | 36% | 13 | 8 | 10 |
| composition.rs | 107 | 22 | 21% | 16 | 1 | 5 |
| raft.rs | 83 | 19 | 23% | 7 | 5 | 7 |
| chunk.rs | 102 | 11 | 11% | 11 | 1 | 0 |
| log.rs | 156 | 9 | 6% | 3 | 4 | 2 |
| client.rs | 105 | 8 | 8% | 8 | 0 | 0 |
| view.rs | 172 | 7 | 4% | 1 | 4 | 2 |
| gateway.rs | 131 | 6 | 5% | 2 | 2 | 2 |
| operational.rs | 268 | 4 | 1% | 1 | 2 | 1 |
| crypto.rs | 61 | 4 | 7% | 3 | 1 | 0 |
| auth.rs | 49 | 3 | 6% | 3 | 0 | 0 |
| protocol.rs | 464 | 2 | <1% | 1 | 1 | 0 |
| **Total** | **1785** | **126** | **7%** | **69** | **29** | **29** |

## Root Causes

1. **No-op Given/When steps** (~55): Empty function bodies, parameters
   ignored with underscore prefix. These are precondition/action steps
   that do nothing, so subsequent Then steps assert on stale state.

2. **Comment-only Then steps** (~22): Steps containing only a comment
   like `// structural precondition` or `// TODO: wire audit`. These
   are effectively `assert!(true)`.

3. **Flag-only assertions** (~15): `assert!(w.last_error.is_some())`
   without checking error content, or `assert!(result.is_ok())`
   without checking the result value.

4. **Tautological assertions** (~10): `assert!(count >= 0)` on unsigned
   types, or asserting on data that was set up in the Given step rather
   than produced by the When step.

5. **Hardcoded shard/tenant names** (~8): Steps reference "shard-alpha"
   or "default" instead of the name from the scenario text.

## Accepted (not fixing)

Many no-op Given steps are legitimate **structural preconditions** for
in-memory testing. These would only become meaningful with real
multi-node infrastructure (e.g., "Given 3 storage nodes" is a no-op
in in-memory harness but correct — the in-memory store simulates a
single-node cluster).

Categories accepted as-is:
- Multi-node topology setup (raft.rs: 3 nodes, partitions, failures)
- Network transport setup (TLS, heartbeats)
- Device hardware simulation (device failures, SMART wear)
- Advisory hint forwarding (no-op by design per I-WA1/I-WA2)
- Audit infrastructure wiring (deferred to P-phase persistence)

## Fix Priority

### P0: Comment-only Then steps (22 in composition.rs)

These are Then steps that contain only a comment and no assertion.
They silently pass without testing anything. Fix by adding real
assertions against domain state.

### P1: Flag-only Then assertions (15 across admin.rs, log.rs)

These check `w.last_error.is_some()` but don't verify the error
content matches the scenario expectation. Fix by adding error
message content assertions.

### P2: Tautological assertions (10 across admin.rs, raft.rs)

These assert on constants or Given-step data. Fix by asserting on
actual When-step output.

### Deferred: No-op When/Given steps for distributed scenarios

These require real multi-node infrastructure to test meaningfully.
Will be addressed in Phase I2 (multi-node Raft).
