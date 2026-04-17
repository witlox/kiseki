Pre-commit verification. Run this before every commit claim.

1. Format (Rust): `cargo fmt --check` — must pass
2. Format (Go): `go fmt ./...` — must pass (when Go code exists)
3. Lint (Rust): `cargo clippy -- -D warnings` — must be 0 warnings
4. Lint (Go): `golangci-lint run ./...` — must be 0 errors (when Go code exists)
5. Build (Rust): `cargo build` — must succeed
6. Build (Go): `go build ./...` — must succeed (when Go code exists)
7. Unit tests (Rust): `cargo test` — all must pass
8. Unit tests (Go): `go test ./...` — all must pass (when Go code exists)
9. Scenario coverage: check Gherkin scenarios against test implementations
10. Report: show pass/fail counts for each step

If ANY step fails, do NOT commit. Fix first, then re-run /project:verify.
