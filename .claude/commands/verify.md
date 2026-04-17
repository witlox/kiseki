Pre-commit verification. Run this before every commit claim.

1. Format: `go fmt ./...`
2. Vet: `go vet ./...` — must be 0 errors
3. Lint: `golangci-lint run ./...` — must be 0 errors (if golangci-lint is installed)
4. Build: `make build` — must succeed
5. Unit tests: `make test-unit` — all must pass
6. Acceptance tests: `make test-acceptance` — check for pending vs failing
7. Scenario coverage: `make verify-scenarios` — report uncovered scenarios
8. Report: show pass/fail counts for each step

If ANY step fails (except pending acceptance steps), do NOT commit. Fix first, then re-run /project:verify.
