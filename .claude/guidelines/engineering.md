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

### TDD Discipline

- Write a failing test before writing implementation code
- Tests describe behavior, not implementation details
- Test names should read as specifications: `test_allocator_rejects_overcommit`
- Coverage thresholds enforced in CI (minimum 50%, target 80%+ for new code)

### BDD / Specification-Driven

- Behavioral specifications written in Gherkin (.feature files) BEFORE implementation
- Acceptance tests validate business-level behavior end-to-end
- Domain model, invariants, and ubiquitous language documented in specs/
- Fidelity tracking: each invariant/feature rated THOROUGH, MODERATE, or NONE for test depth

### Test Organization

- Unit tests: co-located with source (in-module for Rust, `_test.go` for Go)
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

1. **Analyst** — domain model, invariants, ubiquitous language, behavioral specs
2. **Architect** — architecture specs, component boundaries, ADRs
3. **Adversary** — challenge completeness, find blind spots, failure modes
4. **Implementer** — code against specs, TDD per component
5. **Auditor** — fidelity index, confidence levels, coverage gaps
6. **Integrator** — cross-component integration, end-to-end validation
