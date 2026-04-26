# Test & build runtimes — estimates

Operational reference for "how long does this take?" so we can pick the
right verification step before committing.

## BDD acceptance suite

| Suite | Cmd | Scenarios | Wall time | Notes |
|---|---|---:|---:|---|
| Fast | `cargo test -p kiseki-acceptance --test acceptance` | 181 | **~1m 45s** (measured 2026-04-26) | Default; runs on every CI push. |
| Slow | `cargo test -p kiseki-acceptance --test acceptance --features slow-tests` | 241 | **~1m 02s** (measured 2026-04-26) | Faster than the fast suite because (a) ~23 scenarios short-circuit on `todo!()` panics (Phase 14f scope) and (b) the `@slow` tag is **pre-Phase-13f legacy** — before ADR-037 introduced the in-process `RaftTestCluster` (mpsc transport + `MemLogStore`), Raft scenarios needed real networking + persistent storage and were genuinely 1-2 s each. Now they're tens of ms. Going forward the tag should mean "exercises real disk fsync or real TCP/TLS" — the persistence subsections wired in Phase 14b are the natural new owners. macOS used to be ×10-100 slower than Linux for this suite because of osxfs/virtiofs fsync cost + macOS timer-coalescing stretching `tokio::time::sleep` past the 150-300 ms election window; on Linux with `epoll` and native fs, both costs collapse. **First attempt hung at "Quorum loss blocks writes"** because `RaftTestCluster::write_delta` had no timeout; openraft's `client_write` blocks indefinitely without quorum. Killed at ~90 min wall. Fix landed (5 s `tokio::time::timeout` around `client_write` in `crates/kiseki-log/src/raft/test_cluster.rs`). |

The slow suite spins up real `RaftTestCluster` instances (multi-node
openraft, channel-based transport), fault-injection scenarios with
~150-300ms election timeouts, and persistence scenarios that open and
close redb databases per-scenario. Per-scenario cost ~1-2s; the spread
is largely Raft election timing.

## Container builds

| Image | Cmd | Wall time | Size | Notes |
|---|---|---:|---:|---|
| `kiseki-server` | `docker build -f Dockerfile.server -t kiseki-server:local .` | ~5 min (cold) | 991 MB | Two-stage: rust:slim builder (cmake/clang/protobuf/golang/perl/nasm) → rust:slim runtime (ca-certificates + binary). FIPS default disabled in the Docker build — the FIPS delocator needs a certified toolchain not available in generic Docker; production FIPS uses a dedicated certified build env. |

## Workspace tests (excl. acceptance)

| Cmd | Wall time | Notes |
|---|---:|---|
| `cargo test --workspace --exclude kiseki-acceptance --locked` | ~30-45s | All unit + integration tests across 12 production crates. Cached redb dependency. |

## Lint + fmt

| Cmd | Wall time | Notes |
|---|---:|---|
| `cargo fmt --all -- --check` | <5s | |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | ~60s warm / ~3 min cold | CI-equivalent invocation (matches `make rust-clippy` after `844f5aa`). |

## CI

| Workflow | Trigger | Wall time | Path |
|---|---|---:|---|
| `ci.yml` (fast suite) | every push to `main` | ~7-10 min | fmt → clippy → deny → feature-check → unit + BDD-fast → coverage |
| `release.yml` (full suite + e2e) | `workflow_dispatch` / weekly Mon 06:00 UTC | ~hours | preflight (incl. `--features slow-tests`) → server/client per-arch → docker → crates → e2e single+3-node → publish |

Update this file as soon as a measurement diverges from the estimate
by more than 50 %.
