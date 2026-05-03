# Kiseki

Distributed storage system for HPC/AI workloads. 20 production Rust
crates (+ 1 BDD-test crate), 40 ADRs, 140 invariants.

BDD acceptance: 316 scenarios (CI: 315 pass, 1 occasionally flaky on
multi-node-raft D-10 cross-stream). Fidelity fix landed —
@integration steps now drive real multi-node clusters via
ClusterHarness against spawned `kiseki-server` binaries; in-memory
mocks retired for the cross-node paths.

Workspace tests: ~1650 unit + integration via cargo nextest. CI
splits Unit Tests into two invocations (workspace minus
kiseki-chunk-cluster, then chunk-cluster alone) to dodge a
process-wide rustls CryptoProvider clash in the gRPC TLS
round-trip tests. See `.config/nextest.toml`.

E2e tests: 18 Python test files via docker compose (real server, real
protocols — these are the ground truth).

GCP perf cluster: 3 Terraform profiles (default/transport/gpu) in
`infra/gcp/`. **`transport` requires europe-west1**
(c3-standard-88-lssd is not available in west6 default). Last run
2026-05-03 surfaced a fabric write quorum-loss bug — cross-node
PutFragment averaging 2 s on a 28 Gbps wire. Suspected cause:
`build_fabric_channel` in `kiseki-server::runtime` missing
`tcp_nodelay(true)` on the tonic Channel. See
`docs/performance/README.md`.

Phase 16 (cross-node chunk replication) complete in code. Phase 17
follow-ups landed: ADR-040 persistent CompositionStore, per-shard
leader endpoint, delta hydration with `name_inserts` / `name_removes`
on followers.

May 2026 perf-fix sweep (commits b0f048d, 56ec297, e058ded, 59cab58):
local single-node matrix shows NFSv4 GET went from 24 op/s · p99 30 s
to 27 291 op/s · p99 4 ms, pNFS GET fixed from 100 % errors to 16 549
op/s, and S3 GET 5.6× — see `docs/performance/README.md`.

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
