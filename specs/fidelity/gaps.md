# Cross-Cutting Gaps

## Unenforced invariants (highest risk)

| Invariant | Claimed | Actual | Risk |
|-----------|---------|--------|------|
| I-L2 | Durable on majority before ack | No Raft — single-node only | **CRITICAL** |
| I-K12 | System key manager HA | No Raft — single-node only | **CRITICAL** |
| I-A1 | Audit append-only, immutable, same durability as Log | No Raft, no persistence | **HIGH** |
| I-K2 | No plaintext on wire | TLS config exists but no integration test proving encrypted wire | **HIGH** |
| I-C3 | Placement per affinity policy | Pool tracking exists, no placement engine | **MEDIUM** |
| I-C4 | EC per pool | Enum exists, no encoding/decoding | **MEDIUM** |
| I-V1 | View rebuildable from shards | No rebuild test (only discard) | **MEDIUM** |
| I-V3 | Cross-view consistency per protocol | `check_staleness` exists but no cross-view test | **MEDIUM** |
| I-Auth2 | Optional tenant IdP | Not implemented | **LOW** |
| I-Auth3 | SPIFFE SVID | Not implemented | **LOW** |

## Dead specs (feature scenarios with zero test coverage)

- `workflow-advisory.feature`: 44/51 scenarios untested
- `protocol-gateway.feature`: 23/23 scenarios untested
- `native-client.feature`: 22/26 scenarios untested
- `operational.feature`: 28/33 scenarios untested

## Orphan tests (tests with no spec traceability)

None found — all tests reference spec invariants or feature scenarios.

## Coverage gaps by bounded context

| Context | Total scenarios | Tested | Gap |
|---------|----------------|--------|-----|
| Log | 21 | 7 | 14 (67%) |
| Chunk Storage | 25 | 6 | 19 (76%) |
| Composition | 21 | 7 | 14 (67%) |
| View Materialization | 23 | 7 | 16 (70%) |
| Protocol Gateway | 23 | 0 | 23 (100%) |
| Native Client | 26 | 4 | 22 (85%) |
| Key Management | 17 | 5* | 12 (71%) |
| Control Plane | 32 | 14 | 18 (56%) |
| Authentication | 16 | 3 | 13 (81%) |
| Operational | 33 | 5 | 28 (85%) |
| Workflow Advisory | 51 | 7 | 44 (86%) |
| **Total** | **288** | **65** | **223 (77%)** |

*Combined across kiseki-crypto (26 tests) and kiseki-keymanager (9 tests),
mapped to 5 of 17 feature scenarios.

## Assessment

The project has solid foundational coverage: domain types match the
ubiquitous language, trait boundaries match the API contracts, and
property tests cover the load-bearing primitives (HLC, AEAD, HKDF).

The 77% scenario gap is structural, not accidental — the in-memory
reference implementations prove trait semantics but cannot test Raft
consensus, protocol wire formats, FUSE mounts, or cross-process
isolation. These gaps close with WI-2 (Raft), WI-3 (gRPC), and WI-4
(server runtime).

**Highest-risk gaps**: I-L2 (Raft durability) and I-K12 (key manager HA)
are the two invariants that the system's correctness depends on but
that have zero enforcement. These are addressed by WI-2.
