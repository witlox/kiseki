# Test & build runtimes — estimates

Operational reference for "how long does this take?" so we can pick the
right verification step before committing.

## BDD acceptance suite

| Suite | Cmd | Scenarios | Wall time | Notes |
|---|---|---:|---:|---|
| Fast | `cargo test -p kiseki-acceptance --test acceptance` | 181 | **~1m 45s** (measured 2026-04-26) | Default; runs on every CI push. |
| Slow | `cargo test -p kiseki-acceptance --test acceptance --features slow-tests` | 241 | **~1m 02s** (measured 2026-04-26) | Faster than the fast suite because (a) ~23 scenarios short-circuit on `todo!()` panics (Phase 14f scope) and (b) the `@slow` tag is **pre-Phase-13f legacy**. See "Per-feature measurements" below. **First attempt hung at "Quorum loss blocks writes"** because `RaftTestCluster::write_delta` had no timeout; openraft's `client_write` blocks indefinitely without quorum. Killed at ~90 min wall. Fix landed (5 s `tokio::time::timeout` around `client_write` in `crates/kiseki-log/src/raft/test_cluster.rs`). |

### Per-feature measurements (2026-04-26, Linux, slow-tests on)

Drive a subset by passing `KISEKI_FEATURE_FILTER=<substring>` to the
test binary directly (the runner picks it up via env var, see
`tests/acceptance.rs::main`). Equivalent for scenario-name filtering:
`KISEKI_SCENARIO_FILTER=<substring>`.

| Feature | Real backend | Scenarios | Wall | Per scenario |
|---|---|---:|---:|---:|
| `persistence` | redb on tmpfs | 14 | 3.9 s | ~280 ms |
| `protocol-gateway` | loopback TCP via NFS/S3 listeners | 14 | 2.9 s | ~205 ms |
| `multi-node-raft` | in-process openraft + mpsc transport | 30 | 9.7 s | ~323 ms |
| `log` | MemShardStore + redb (mixed) | 17 | 3.3 s | ~197 ms |
| `block-storage` | FileBackedDevice on tmpfs | 27 | 4.7 s | ~176 ms |

**Conclusion**: nothing here is meaningfully "slow" on Linux. The
`@slow` tag is therefore semantic noise on this OS. It should be
either retired entirely, or repurposed to mean "scenarios that need
an external service (real KMS, real S3, real cloud)" — which is what
Phase 14a's stubbed cloud-KMS backends and Phase 14d's S3 backup
backend would tag.

Historical context: macOS suffered ×10-100 here because of (i)
osxfs/virtiofs fsync overhead amplifying every redb commit, and
(ii) macOS timer coalescing stretching tokio's `time::sleep` past
the 150-300 ms election window so Raft scenarios constantly retried.
On Linux with `epoll` and native filesystems both costs collapse.

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
