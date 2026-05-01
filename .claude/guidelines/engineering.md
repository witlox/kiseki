# General Engineering Guidelines

## Commits & Branching

- Conventional commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `chore:`, `ci:`
- One logical change per commit; reference issue numbers where applicable

## Error Handling

- Wrap errors with context (what operation failed and why)
- Typed error types in library code; validate at system boundaries
- Trust internal code; validate external input

## Code Organization

- Imports grouped: stdlib → external → internal
- One responsibility per file; keep files under 500 lines
- Pass dependencies explicitly, no globals

## Testing

**TDD** — crate-level unit tests drive internal logic. Red → green
within a single crate.

**BDD** — Gherkin scenarios verify the assembled system. @integration
steps talk to a running `kiseki-server` via `world.server()` (gRPC/HTTP).
@unit steps may call domain objects for pure logic (crypto, EC math).

The `world/` module a step touches reveals its tier: production crate
imports = @unit. Only `kiseki-proto` imports = @integration.

**Organization**: unit tests co-located with source. BDD in
`crates/kiseki-acceptance/`. E2e in `tests/e2e/` (Python, docker compose).

## Architecture Decision Records

ADRs in `specs/architecture/adr/`. Record context, decision, consequences.
Append-only — supersede, don't edit.

## Workflow Phases

1. Analyst — domain model, invariants, Gherkin scenarios
2. Architect — interfaces, contracts, ADRs
3. Adversary — challenge completeness, gate 1
4. Implementer — TDD for units, BDD for integration
5. Auditor — depth classification, gate 2
6. Integrator — cross-context validation
