# Fidelity Index — Kiseki

**Checkpoint**: 2026-04-19
**Auditor**: auditor role
**Scope**: All 13 Rust crates + 5 Go packages

## Per-Context Confidence

| Bounded Context | Crate(s) | Confidence | Scenarios (tested/total) | Key gap |
|----------------|----------|------------|--------------------------|---------|
| **Common/Time** | kiseki-common, kiseki-proto | **HIGH** | 19/— | — |
| **Cryptography** | kiseki-crypto | **MEDIUM** | 4/17 | Key lifecycle orchestration untested |
| **Key Management** | kiseki-keymanager | **MEDIUM** | 5/17 | No Raft HA (I-K12) |
| **Log** | kiseki-log | **LOW** | 7/21 | No Raft consensus (I-L2) |
| **Transport/Auth** | kiseki-transport | **LOW** | 3/16 | OrgId placeholder, no revocation |
| **Chunk Storage** | kiseki-chunk | **LOW** | 6/25 | No EC, no placement engine |
| **Audit** | kiseki-audit | **LOW** | 5/33 | No persistence, no safety valve |
| **Composition** | kiseki-composition | **MEDIUM** | 7/21 | No refcount integration |
| **View** | kiseki-view | **LOW** | 7/23 | No stream processor |
| **Protocol Gateway** | kiseki-gateway | **LOW** | 0/23 | Trait only, no protocol impl |
| **Native Client** | kiseki-client | **LOW** | 4/26 | No FUSE, no transport |
| **Advisory** | kiseki-advisory | **LOW** | 7/51 | No gRPC service, no telemetry |
| **Control Plane** | control/pkg/* | **MEDIUM** | 14/32 | No gRPC server |

## Aggregate

- **HIGH**: 1 context (Common/Time)
- **MEDIUM**: 4 contexts (Crypto, Key Manager, Composition, Control Plane)
- **LOW**: 8 contexts

## ADR Enforcement

- **ENFORCED**: 8/21 (38%)
- **DOCUMENTED**: 8/21 (38%)
- **UNENFORCED**: 5/21 (24%)

## Scenario Coverage

- **Total scenarios**: 288
- **Tested**: 65 (23%)
- **Gap**: 223 (77%)

## Critical Unenforced Invariants

1. **I-L2** — Raft durability (no consensus implementation)
2. **I-K12** — Key manager HA (no Raft replication)
3. **I-A1** — Audit durability (no persistence)

## Recommendation

The trait-level implementation is sound — domain types match specs,
property tests cover primitives, adversarial reviews found and fixed
real bugs. The 77% scenario gap is structural: it closes with Raft
(WI-2), gRPC (WI-3), and server runtime (WI-4).

**Priority**: WI-2a (trait prep) → WI-2b (keymanager Raft) → WI-2c
(log Raft) to close the two CRITICAL invariant gaps.
