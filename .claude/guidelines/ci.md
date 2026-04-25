# CI/CD Guidelines

## Pipeline Structure (three-stage)

Every project follows: **Build → Validate → Test**

### Build Stage

- Compile all targets
- Create versioned artifacts (binaries, Docker images)
- Upload artifacts with retention period (7-30 days)
- Path-based triggers: only run when relevant code changes

### Validate Stage

- Formatting check (rustfmt)
- Static analysis (clippy)
- Module hygiene (Cargo.lock consistency)
- Linting (clippy with deny warnings)
- Security scanning (cargo-deny advisories)
- Vulnerability checking (cargo-deny / CodeQL)

### Test Stage

- Unit tests with race/thread-safety detection
- Integration tests (Docker-based via testcontainers)
- Acceptance tests (BDD/Gherkin)
- Coverage collection per test type → merge → upload to Codecov
- Coverage threshold enforcement (50% minimum, 80% target)
- Separate coverage flags for unit/integration/acceptance

## Triggers

- Push to `main` or `develop`
- PRs against `main` or `develop`
- Path exclusions: `docs/**`, `*.md`, `LICENSE`
- Per-crate path filters for workspace projects

## Caching

- Rust: `Swatinem/rust-cache@v2`

## Additional Workflows

- **Dependabot auto-merge** — automated dependency updates for patch/minor
- **Vulnerability fix** — auto-create PRs for security advisories
- **License compliance** — FOSSA scanning
- **CodeQL** — weekly security analysis

## Docker Builds

- Multi-stage: builder (full SDK) → runtime (minimal Alpine)
- Non-root user in runtime image
- Strip debug symbols (release profile for Rust)
- Version injection at build time
- No privileged mode in CI

## Release

- Version from tag or calver (e.g., 2026.1.0)
- Auto-publish to registries on main when version bumps
- Artifacts attached to GitHub releases
