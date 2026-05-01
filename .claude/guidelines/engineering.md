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
- Implementer makes them green by running a REAL `kiseki-server` binary
  and making REAL network calls (gRPC, HTTP, TCP) in step definitions
- @integration scenarios exercise cross-context behavior through the
  server's actual protocol endpoints — never through in-process mocks
- Fidelity tracking: each scenario rated by depth (see `roles/auditor.md`)

### BDD Step Fidelity

@integration BDD steps MUST exercise the system through network protocols.
The `KisekiWorld` struct holds a running `kiseki-server` process and
network clients (gRPC channel, HTTP client). Step definitions use ONLY
these clients to interact with the system.

**Forbidden in @integration steps:**
- Importing production crate types other than `kiseki-proto` (gRPC stubs)
  and `kiseki-common` (shared IDs/types for request construction)
- Calling any method on an in-process domain object (`gateway`, `log_store`,
  `key_store`, `comp_store`, `chunk_store`, `view_store`)
- Empty step bodies `{}` (use `todo!("description")` — empty bodies silently pass)
- Setting World fields as the sole means of passing data between steps

**The test for whether a step is real:**
If you deleted the `kiseki-server` binary, would the step fail?
If yes: real. If no: fake.

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
- BDD @integration steps use network clients (gRPC/HTTP), never in-process mocks
- In-memory implementations are acceptable for @unit crate tests only
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
