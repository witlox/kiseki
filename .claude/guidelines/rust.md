# Rust Guidelines

## Version & Tooling

- Edition 2024 for new crates, 2021 acceptable for existing
- MSRV: 1.85+
- Format: `rustfmt` with `rustfmt.toml` (max_width = 100, edition = 2024)
- Lint: `clippy` with pedantic + nursery at warn level
- License/advisory: `cargo-deny` with `deny.toml`
- Test runner: `cargo-nextest` (parallel execution)
- Coverage: `cargo-llvm-cov` → Codecov
- Build system: `just` (justfile) or `make` (Makefile)

## Workspace Structure

- Workspace in root `Cargo.toml` with crates under `crates/`
- Workspace-level dependency versions, lints, and metadata
- Per-crate CI workflows triggered by path filters

## Style & Safety

- `unsafe_code = "deny"` at workspace level by default
- Explicit `#[allow(unsafe_code)]` per-crate where justified and documented (FFI, mlock, etc.)
- No `unwrap()` in library code (allowed in tests via `allow-unwrap-in-tests`)
- Error handling: `thiserror` for library error types, `anyhow` in CLI/binary crates
- Async runtime: `tokio` (multi-threaded)
- All public types and functions have doc comments (`///`)
- Module-level docs with `//!`

## Clippy Configuration

```toml
[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
nursery = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
must_use_candidate = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"

[workspace.lints.rust]
unsafe_code = "deny"
```

## Testing

- Unit tests: `#[cfg(test)]` modules in-file; `#[test]` for sync, `#[tokio::test]` for async
- BDD: `cucumber` crate for Gherkin scenarios
- Property-based: `proptest` for invariant testing
- Benchmarks: `criterion` for performance-critical paths
- Integration tests: `tests/` directory, may use `testcontainers`
- Slow tests: mark with `#[ignore]`, run with `--include-ignored`
- Use `tempfile` crate for temp directories in tests

## Cargo-Deny (deny.toml)

- Advisory DB: unmaintained = error, yanked = warn
- Allowed licenses: Apache-2.0, MIT, BSD-2/3-Clause, ISC, Unicode-DFS-2016, MPL-2.0, Zlib, OpenSSL, BlueOak-1.0.0
- Sources: only crates.io (no unknown git)
- Multiple versions: warn

## Build System (justfile)

```
just check       # cargo check --workspace
just fmt         # cargo fmt --all
just fmt-check   # cargo fmt --all -- --check
just lint        # cargo clippy --workspace --all-targets --all-features -- -D warnings
just test        # cargo nextest run
just test-all    # nextest with --run-ignored all
just deny        # cargo deny check
just audit       # cargo deny check advisories
just all         # fmt-check + lint + test + deny
```

## Patterns

- Traits at module boundaries where multiple implementations exist
- Concrete types within a component
- Protobuf for cross-language APIs (gRPC)
- Feature flags for optional integrations; default features minimal
- All features tested in CI (`--all-features`)
