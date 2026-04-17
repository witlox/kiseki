# Kiseki

Distributed storage system for HPC/AI workloads. Analyst phase complete,
entering architect phase.

## Language

- Core: Rust
- Control plane: Go
- Boundary: gRPC / protobuf
- Client bindings: Rust native + C FFI, Python (PyO3), C++ wrapper
- Crypto: FIPS 140-2/3 validated (aws-lc-rs or equivalent)

## Workflow

Diamond workflow via `.claude/CLAUDE.md`. Role definitions in `.claude/roles/`.
Read the workflow router before acting.

## Key documents

- `docs/analysis/design-conversation.md` — distilled 16-turn design conversation
- `docs/prior-art/deltafs-mochi-evaluation.md` — DeltaFS + Mochi comparison
- `specs/SEED.md` — original analyst seed (candidate terms, question bank)
- `specs/ubiquitous-language.md` — 45+ confirmed terms
- `specs/domain-model.md` — 8 bounded contexts with relationships
- `specs/invariants.md` — 51 invariants across 12 categories
- `specs/assumptions.md` — 50+ tracked assumptions
- `specs/failure-modes.md` — 20 failure modes (P0-P3)
- `specs/adversarial-findings.md` — 17 findings (3 closed, 14 for architect)
- `specs/cross-context/interactions.md` — data paths, contracts, cascades
- `specs/features/*.feature` — 132 Gherkin scenarios across 8 contexts

## Pre-commit

Run `make` before committing (once a Makefile exists). No code yet.
