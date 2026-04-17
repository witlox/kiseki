# Kiseki

Distributed storage system for HPC/AI workloads. Pure greenfield — design
exploration phase.

## Language

- Core: Rust
- Control plane: Go
- Boundary: gRPC / protobuf
- Client bindings: Rust native + C FFI, Python (PyO3), C++ wrapper

## Workflow

Diamond workflow via `.claude/CLAUDE.md`. Role definitions in `.claude/roles/`.
Read the workflow router before acting.

## Key documents

- `docs/analysis/design-conversation.md` — distilled 16-turn design conversation
- `docs/prior-art/deltafs-mochi-evaluation.md` — DeltaFS + Mochi comparison
- `specs/SEED.md` — candidate terms, invariants, question bank (unvalidated)
- `specs/` — analyst outputs land here

## Pre-commit

Run `make` before committing (once a Makefile exists). No code yet.
