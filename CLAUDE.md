# Kiseki

Distributed storage system for HPC/AI workloads. 20 production Rust
crates (+ 1 BDD-test crate), 41 ADRs, 140 invariants.

BDD acceptance: 321 scenarios. CI green: 320 pass + 1 `@flaky`
(retried 2× via cucumber `retry_filter`). Two scenarios currently
tagged `@flaky`: D-10 cross-stream and 6-node EC PUT under cross-
singleton compose pressure. Fidelity fix landed — @integration
steps drive real multi-node clusters via ClusterHarness against
spawned `kiseki-server` binaries; in-memory mocks retired for the
cross-node paths.

Workspace tests: ~1650 unit + integration via cargo nextest. CI
splits Unit Tests into two invocations (workspace minus
kiseki-chunk-cluster, then chunk-cluster alone) to dodge a
process-wide rustls CryptoProvider clash in the gRPC TLS
round-trip tests. See `.config/nextest.toml`.

E2e tests: 31 tests across 18 Python test files via docker compose
(real server, real protocols — these are the ground truth). 2026-05-05
run: 28 / 31 pass + 2 skipped + 3 ReadTimeout pressure flakes
flagged for follow-up (S3 PUT, S3 HEAD, FUSE remote-HTTP cross-
protocol).

GCP perf cluster: 3 Terraform profiles (default/transport/gpu) in
`infra/gcp/`. **`transport` requires europe-west1**
(c3-standard-88-lssd is not available in west6 default). 2026-05-02
fabric write quorum-loss root-caused to H2 flow-control window
default (commit `f362060` bumped to 16 MiB stream / 32 MiB
connection). TCP_NODELAY on the fabric Channel was confirmed
default-on in tonic 0.14.5. Re-run on real hardware pending. See
`docs/performance/README.md`.

Phase 16 (cross-node chunk replication) complete in code. Phase 17
follow-ups landed: ADR-040 persistent CompositionStore, per-shard
leader endpoint, delta hydration with `name_inserts` / `name_removes`
on followers. ADR-041 (multiplexed Raft transport — single port per
node) + ADR-033 §4 (cluster-wide split apply hook) landed
2026-05-04 — see `specs/implementation/post-2026-05-03-sweep.md`.

May 2026 perf-fix sweep: local single-node matrix shows NFSv4 GET
went from 24 op/s · p99 30 s to 27 291 op/s · p99 4 ms, pNFS GET
fixed from 100 % errors to 16 549 op/s, and S3 GET 5.6×. FUSE p99
collapsed from 160 ms (RwLock-across-gateway-call + per-write
composition fsync) to ~4 ms (3-phase write-lock + composition
group commit). Group-commit fsync(2) correctness preserved via
`gateway.fsync_pending()` hook chain — see
`docs/operations/durability.md` for per-knob loss windows. Target
deployment is 10–100+ nodes where R-3 / EC-4+2 + scrub recover any
per-node loss window.

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
