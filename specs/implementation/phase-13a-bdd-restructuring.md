# Phase 13a: BDD Restructuring (completed 2026-04-25)

## Goal

Move all 401 @unit BDD scenarios to crate-level unit tests. Keep only
@integration scenarios in the acceptance suite.

## Phases

### Phase A: Remove 159 COVERED scenarios
- Crate tests already existed
- Removed from BDD, no code changes
- **Status: Done**

### Phase B: Fix 59 PARTIAL scenarios
- Fix crate test stubs with real assertions, then remove from BDD
- **22 fixed, 19 removed**
- Remaining 37 absorbed by Phase C
- **Status: Done (verified — 0 stubs remain, 14 weak/structural)**

### Phase C: Add 142 GAP-UNIT tests
- Write crate unit tests, then remove @unit BDD scenarios
- **243 crate tests added, 222 @unit scenarios removed**
- **Status: Done**

### Phase D: Add 18 GAP-INTEGRATION scenarios
- Absorbed into existing @integration scenarios with todo!() steps
- **Status: Absorbed**

### Phase E: Add 23 GAP-BOTH
- Absorbed into @integration + crate tests from Phase C
- **Status: Absorbed**

## Result

- BDD: 632 → 241 scenarios (all @integration, zero @unit)
- Crate tests: 753 → 1018 (+265)
- All @integration steps have todo!() — implementer's red-green targets

## Key files

- `specs/fidelity/bdd-depth-audit.md` — original depth analysis
- `specs/fidelity/unit-coverage-audit.md` — per-scenario coverage map
