# Go Guidelines

## Version & Tooling

- Go 1.25+ (use latest stable)
- Module path: `github.com/witlox/<project>`
- Format: `gofmt` + `goimports` (enforced in CI)
- Lint: `golangci-lint` with project `.golangci.yml`
- Security: `gosec`, `govulncheck`
- Pre-commit: Lefthook (installed via `make setup`)

## Style

- Follow Effective Go + Uber Go Style Guide
- `context.Context` threaded through all I/O and async operations (first parameter)
- No package-level globals; inject dependencies
- Errors: wrap with `fmt.Errorf("operation: %w", err)`, use custom error types at boundaries
- No `panic()` in library code

## Testing

- Unit tests: table-driven with `t.Run()` subtests; use `testify` assertions
- BDD acceptance tests: `godog` (Gherkin scenarios) in `tests/acceptance/`
- Integration tests: `testcontainers` for Docker-based dependencies
- Race detection: `go test -race` in CI
- Coverage: 50% minimum enforced, 80%+ target for new code
- Test modes: `-short` flag for fast feedback; full mode with Docker

## Build System (Makefile)

```makefile
all:              lint + test + build (default target)
build:            go build ./...
test:             unit + acceptance tests
test-unit:        unit tests only (-short)
test-acceptance:  godog BDD tests
test-integration: integration tests (requires Docker)
test-race:        tests with race detector
lint:             go vet + golangci-lint
fmt:              gofmt + goimports
coverage:         generate coverage report
coverage-check:   enforce threshold
setup:            install tools + git hooks
```

## Linting Configuration (.golangci.yml)

- Timeout: 5m
- Core linters: errcheck, govet, staticcheck, unused, ineffassign
- Quality: gocritic, revive, misspell, errorlint, bodyclose
- Security: gosec, noctx
- Test files: relaxed rules (exclude gosec, errcheck, wrapcheck)
- cmd/ and scripts/: relaxed rules

## Patterns

- Concrete implementations preferred; interfaces only where multiple backends exist
- Leaf packages import only stdlib — enforce import direction
- No code generation unless truly needed; prefer runtime loading
