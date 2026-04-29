# Kiseki

Distributed storage system for HPC/AI workloads. 20 production Rust
crates (+ 1 BDD-test crate), 39 ADRs, 140 invariants, 285
@integration BDD scenarios: 274 passing + 10 skipped (documentation-
grade, see `crates/kiseki-acceptance/tests/acceptance.rs`) + 1
filtered as `@e2e-deferred` (real Linux pNFS client mount, witnessed
by `tests/e2e/test_pnfs.py`).

Phase 15 (pNFS) complete end-to-end — sustained Linux 6.x flex-files
reads work against the 3-node docker compose. Layout encoding shape
revised in 15c.9 (one segment + N mirrors, see ADR-039) and validated
on the wire via tcpdump. Phase 16 (cross-node chunk replication)
also complete; cluster fabric routes correctly from gateway through
3-node fan-out at any practical chunk size.

## Language

- Core: Rust (20 production crates + kiseki-acceptance test crate)
- Boundary: gRPC / protobuf (4 service definitions)
- Client bindings: Rust native + C FFI, Python (PyO3), C++ wrapper
- Crypto: FIPS 140-2/3 validated (aws-lc-rs, AES-256-GCM, HKDF-SHA256)

## Workflow

Diamond workflow via `.claude/CLAUDE.md`. Role definitions in `.claude/roles/`.

## Spec documents (read order for new sessions)

1. `specs/ubiquitous-language.md` — domain terms (read first, always)
2. `specs/domain-model.md` — 8 bounded contexts and relationships
3. `specs/invariants.md` — 140 invariants (the rules)
4. `specs/architecture/module-graph.md` — crate/package structure
5. `specs/architecture/api-contracts.md` — per-context interfaces
6. `specs/architecture/enforcement-map.md` — invariant → code location
7. `specs/architecture/build-phases.md` — implementation order
8. `specs/architecture/error-taxonomy.md` — typed errors
9. `specs/features/*.feature` — Gherkin scenarios (the tests)
10. `specs/architecture/adr/*.md` — architecture decision records

## Background documents (reference as needed)

- `docs/analysis/design-conversation.md` — original design conversation
- `docs/prior-art/deltafs-mochi-evaluation.md` — DeltaFS + Mochi comparison
- `specs/SEED.md` — original analyst seed
- `specs/assumptions.md` — 50+ tracked assumptions
- `specs/failure-modes.md` — failure modes (P0-P3)
- `specs/adversarial-findings.md` — analyst adversarial findings
- `specs/findings/architecture-review.md` — architect adversarial findings
- `specs/cross-context/interactions.md` — data paths and failure cascades

## Pre-commit

Run `make` before committing (once a Makefile exists).
`cargo fmt --check && cargo clippy -- -D warnings && cargo test`
