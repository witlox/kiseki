# Kiseki

Distributed storage system for HPC/AI workloads. Design complete,
ready for implementation.

## Language

- Core: Rust (12 crates in workspace)
- Control plane: Go
- Boundary: gRPC / protobuf (4 service definitions)
- Client bindings: Rust native + C FFI, Python (PyO3), C++ wrapper
- Crypto: FIPS 140-2/3 validated (aws-lc-rs, AES-256-GCM, HKDF-SHA256)

## Workflow

Diamond workflow via `.claude/CLAUDE.md`. Role definitions in `.claude/roles/`.
Read the workflow router before acting.

## Spec documents (read order for new sessions)

1. `specs/ubiquitous-language.md` — domain terms (read first, always)
2. `specs/domain-model.md` — 8 bounded contexts and relationships
3. `specs/invariants.md` — 56 invariants (the rules)
4. `specs/architecture/module-graph.md` — crate/package structure
5. `specs/architecture/api-contracts.md` — per-context interfaces
6. `specs/architecture/enforcement-map.md` — invariant → code location
7. `specs/architecture/build-phases.md` — implementation order
8. `specs/architecture/error-taxonomy.md` — typed errors
9. `specs/features/*.feature` — 132 Gherkin scenarios (the tests)
10. `specs/architecture/adr/*.md` — 19 architecture decision records

## Background documents (reference as needed)

- `docs/analysis/design-conversation.md` — original design conversation
- `docs/prior-art/deltafs-mochi-evaluation.md` — DeltaFS + Mochi comparison
- `specs/SEED.md` — original analyst seed
- `specs/assumptions.md` — 50+ tracked assumptions
- `specs/failure-modes.md` — 20 failure modes (P0-P3)
- `specs/adversarial-findings.md` — analyst adversarial findings
- `specs/findings/architecture-review.md` — architect adversarial findings
- `specs/cross-context/interactions.md` — data paths and failure cascades

## Pre-commit

Run `make` before committing (once a Makefile exists).
Rust: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`
Go: `go fmt ./... && go vet ./... && go test ./...`
