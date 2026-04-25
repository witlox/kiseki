# General Engineering Guidelines

## Commits & Branching

- Conventional commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `chore:`, `ci:`
- Branch naming: `feature/`, `fix/`, `docs/`, `refactor/`, `test/`
- One logical change per commit; reference issue numbers where applicable

## Error Handling

- Never swallow errors silently
- Wrap errors with context (what operation failed and why)
- Use typed/custom error types in library code; richer error types at boundaries
- Validate at system boundaries (user input, external APIs); trust internal code

## Code Organization

- Imports grouped: stdlib → external → internal (blank line between groups)
- Public items before private items in a file
- One component/responsibility per file; keep files under 500 lines where practical
- No globals; pass dependencies explicitly (context, config, clients)

## Code Quality

- Pre-commit hooks enforce formatting, linting, and basic tests before every commit
- No hardcoded secrets, tokens, or credentials in source
- Sanitize sensitive data in logs
- Keep dependencies updated; run vulnerability scanning in CI

## Testing Philosophy

### TDD — build the pieces (unit level)

- Crate-level unit tests for internal logic (algorithms, data structures,
  validation functions)
- Red-green within a single crate: write test → implement → pass
- When no production code exists yet, TDD drives the initial implementation
  based on the analyst's specs and architect's interfaces
- Test names read as specifications: `test_allocator_rejects_overcommit`

### BDD — verify integration (system level)

- Gherkin scenarios written by analyst BEFORE implementation
- Implementer makes them green by wiring production code through real
  integrated paths (gateway→composition→log, real backends)
- @integration scenarios exercise cross-context behavior end-to-end
- Fidelity tracking: each scenario rated by depth (see `roles/auditor.md`)

### Test Organization

- Unit tests: co-located with source (in-module for Rust)
- Integration tests: `tests/integration/` — require external services (Docker, DB)
- Acceptance tests: `tests/acceptance/` or `tests/e2e/` — BDD/Gherkin scenarios
- Test helpers and fixtures: `tests/testutil/` — shared mocks, in-memory implementations
- Slow/integration tests marked (build tags, `#[ignore]`) so fast feedback loop stays fast

### Test Patterns

- Table-driven tests with named cases
- Arrange-Act-Assert structure
- Mock external dependencies at boundaries, not internal logic
- Use in-memory implementations over mocks where possible (more realistic)
- Test edge cases and error paths, not just happy paths

## Architecture Decision Records

- ADRs stored in `specs/architecture/adr/` (or `docs/decisions/`)
- Record the context, decision, and consequences
- ADRs are append-only (supersede, don't edit)

## Workflow Phases (spec-driven development)

1. **Analyst** — domain model, invariants, ubiquitous language, Gherkin scenarios
2. **Architect** — interfaces, contracts, ADRs (the design, not the code)
3. **Adversary** — challenge completeness, find flaws, gate 1
4. **Implementer** — TDD for unit logic, BDD for integration wiring
5. **Auditor** — depth classification, fidelity index, gate 2
6. **Integrator** — cross-context validation, end-to-end paths
