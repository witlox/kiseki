# Kiseki — root Makefile
#
# Three test tiers (cascading; each higher tier includes the lower):
#
#   make test-fast        Tier 1 — DEFAULT. Fast unit tests
#                         (kiseki-acceptance excluded; tests marked
#                         `#[ignore = "slow:…"]` skipped) plus the
#                         BDD `@smoke` subset (KISEKI_BDD_FAST=1).
#                         Target: a few minutes wall on a dev box.
#                         Run between every code edit; runs as the
#                         pre-commit gate.
#
#   make test-slow        Tier 2 — adds the `#[ignore = "slow:…"]`
#                         unit tests + every BDD scenario the smoke
#                         filter skipped. Re-runs Tier 1 first so a
#                         single `make test-slow` covers both. Run
#                         pre-PR.
#
#   make test-full        Tier 3 — adds Python e2e via docker compose.
#                         Slowest; weekly / pre-merge / CI release
#                         lane. Runs `make test-slow` first.
#
# `make` (no target) = `make verify` = fmt-check + clippy + Tier 1
# + arch-check. The pre-commit standard.
#
# Tooling:
#   - cargo-nextest is required (`cargo install cargo-nextest --locked`).
#   - Slow unit tests: annotate with `#[ignore = "slow: <reason>"]`.
#     Plain `cargo test` and Tier 1 skip them; Tier 2 picks them up
#     via `cargo nextest run --run-ignored=only`.
#   - BDD smoke selection: the existing `KISEKI_BDD_FAST=1` runtime
#     branch in `crates/kiseki-acceptance/tests/acceptance.rs`. No
#     new tags needed.

.PHONY: all verify verify-full \
        test test-fast test-slow test-full e2e \
        rust-fmt rust-fmt-check rust-clippy rust-deny rust-build \
        check fmt build clean help arch-check \
        check-tools

SHELL := /bin/bash

# --- Rust toolchain commands ---
CARGO        ?= cargo

# Tier 1 — fast unit (workspace minus acceptance via default-members
# AND profile filter for safety). No ignored tests. The
# kiseki-chunk-cluster gRPC+TLS round-trip tests need their OWN
# process so the rustls `CryptoProvider::install_default()` they call
# wins uncontested — workspace-parallel load lets another test's tonic
# client install first, after which the TLS handshake codec_path
# rejects with `received corrupt message of type InvalidContentType`
# and the round-trip times out (observed: 360 s × 2 = entire Tier-1
# wall before this split). Two invocations is the cheapest fix; the
# CI workflow does the same. Same shape for Tier 2's
# `--run-ignored=only` lane below.
NEXTEST_FAST_UNIT_MAIN     ?= $(CARGO) nextest run --profile fast --workspace --exclude kiseki-acceptance --exclude kiseki-chunk-cluster --locked
NEXTEST_FAST_UNIT_TLS_PEER ?= $(CARGO) nextest run --profile fast -p kiseki-chunk-cluster --locked
# Tier 1 — BDD @smoke. KISEKI_BDD_FAST=1 flips the cucumber runner
# branch in acceptance.rs to skip @slow + non-smoke @integration.
# `cargo test`, not nextest: the cucumber binary is `harness = false`,
# so nextest's libtest-style enumeration (`--list --format terse`)
# fails on cucumber-rs's clap parser. cargo test invokes the binary
# directly with no libtest pre-flight, which is what cucumber expects.
NEXTEST_FAST_BDD   ?= KISEKI_BDD_FAST=1 $(CARGO) test --locked -p kiseki-acceptance --test acceptance
# Tier 2 — only the slow-marked unit tests (Tier 1 already ran the
# fast ones; this fills in the rest). Same TLS-peer split.
NEXTEST_SLOW_UNIT_MAIN     ?= $(CARGO) nextest run --profile slow --run-ignored=only --workspace --exclude kiseki-acceptance --exclude kiseki-chunk-cluster --locked
NEXTEST_SLOW_UNIT_TLS_PEER ?= $(CARGO) nextest run --profile slow --run-ignored=only -p kiseki-chunk-cluster --locked
# Tier 2 — full BDD (no env var → no @smoke / @slow filtering).
# Same `cargo test` rationale as Tier 1.
NEXTEST_SLOW_BDD   ?= $(CARGO) test --locked -p kiseki-acceptance --test acceptance

# Plain build (default-members → no acceptance).
CARGO_BUILD  ?= $(CARGO) build --all-targets --locked
# Clippy at the same scope CI runs (full workspace including
# acceptance — clippy is cheap, value is high).
CARGO_CLIPPY ?= $(CARGO) clippy --workspace --all-targets --locked -- -D warnings
CARGO_FMT    ?= $(CARGO) fmt --all

all: verify ## Default: pre-commit (fmt-check + clippy + Tier 1 tests + arch-check)

help: ## Show this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ---------------------------------------------------------------------
# Tool checks
# ---------------------------------------------------------------------

check-tools: ## Verify required tools are installed
	@command -v $(CARGO) >/dev/null || { echo "cargo not found in PATH"; exit 1; }
	@$(CARGO) nextest --version >/dev/null 2>&1 || { \
		echo "cargo-nextest not installed."; \
		echo "Install: cargo install cargo-nextest --locked"; \
		exit 1; \
	}

# ---------------------------------------------------------------------
# Rust — formatting + lints
# ---------------------------------------------------------------------

rust-fmt: ## Apply rustfmt to all crates
	$(CARGO_FMT)

rust-fmt-check: ## Check rustfmt without modifying files
	$(CARGO_FMT) -- --check

rust-clippy: ## cargo clippy with -D warnings (workspace, including kiseki-acceptance)
	$(CARGO_CLIPPY)

rust-deny: ## cargo-deny (licenses, advisories, bans)
	@if command -v cargo-deny >/dev/null; then \
		$(CARGO) deny check; \
	else \
		echo "cargo-deny not installed; skipping (install: cargo install cargo-deny)"; \
	fi

rust-build: ## cargo build (default-members — no acceptance)
	$(CARGO_BUILD)

# ---------------------------------------------------------------------
# Rust — test tiers
# ---------------------------------------------------------------------

test-fast: check-tools ## Tier 1: fast unit + BDD @smoke (the default)
	$(NEXTEST_FAST_UNIT_MAIN)
	$(NEXTEST_FAST_UNIT_TLS_PEER)
	$(NEXTEST_FAST_BDD)

test-slow: test-fast ## Tier 2: Tier 1 + slow-marked unit + full BDD
	$(NEXTEST_SLOW_UNIT_MAIN)
	$(NEXTEST_SLOW_UNIT_TLS_PEER)
	$(NEXTEST_SLOW_BDD)

test-full: test-slow e2e ## Tier 3: Tier 2 + Python e2e via docker compose

test: test-fast ## Alias for `test-fast` — the pre-commit standard

# ---------------------------------------------------------------------
# Architecture enforcement (ADV-3)
# ---------------------------------------------------------------------

arch-check: ## Verify cross-crate boundaries (kiseki-control isolation + ADR-042 §1.8 enforcement)
	@# kiseki-control isolation: forbid data-path crate names appearing
	@# as a dep ENTRY (`<name> = ` at start of line), not in comments
	@# or other prose. Anchored on `^` + `\s*` to match Cargo dep keys.
	@! grep -E '^\s*kiseki-(log|chunk|composition|view|gateway|client|keymanager|crypto|raft|transport|server|audit|advisory)\s*=' \
	    crates/kiseki-control/Cargo.toml \
	    || { echo "VIOLATION: kiseki-control depends on a data-path crate"; exit 1; }
	@# ADR-042 §1.8: ServerImpl is binding-agnostic — no
	@# `tonic::Request` / `tonic::Response` / `tonic::Streaming`, no
	@# TCP-framed `ConnectionContext`, no cxi `AttestationContext`
	@# may appear in the handler module. The grpc adapter (sibling
	@# module) is allowed to use tonic; ServerImpl reads request-
	@# source identity ONLY through `&dyn RequestPrincipal`. A future
	@# contributor adding a binding-specific shortcut method to
	@# ServerImpl fails CI before review.
	@#
	@# `tonic::Status` is the per-call error type returned by handler
	@# methods today and stays allowed (the bigger NativeError-mapping
	@# refactor is its own follow-up). All other `tonic::*` symbols
	@# are forbidden.
	@# Skip comment-only lines (`^\s*//`) so module-level docs that
	@# REFERENCE the forbidden symbols (e.g., explaining the rule)
	@# don't trip the grep — only real code references count.
	@violations=$$(grep -nE 'tonic::(Request|Response|Streaming)\b|tcp_framed::ConnectionContext|cxi::AttestationContext' crates/kiseki-gateway/src/native/server.rs | grep -vE '^[0-9]+:[[:space:]]*//' || true); \
	    if [ -n "$$violations" ]; then \
	        echo "VIOLATION (ADR-042 §1.8): kiseki-gateway::native::server references a binding-specific request type:"; \
	        echo "$$violations"; \
	        echo "ServerImpl must read request-source identity ONLY through &dyn RequestPrincipal. Move binding-specific code into kiseki-gateway::native::<binding>::adapter."; \
	        exit 1; \
	    fi

# ---------------------------------------------------------------------
# Aggregate targets
# ---------------------------------------------------------------------

fmt: rust-fmt ## Apply all formatters

check: rust-fmt-check rust-clippy test-fast arch-check ## Pre-commit (Tier 1 + lint + arch)

verify: check ## Alias for `check` — the pre-commit standard

verify-full: rust-fmt-check rust-clippy rust-deny test-full arch-check ## CI release-equivalent (Tier 3 + deny)

build: rust-build ## Build all artefacts

e2e: ## Python e2e tests via docker compose (Tier 3 component)
	/usr/local/bin/docker compose up --build -d
	.venv/bin/pytest tests/e2e/ -m e2e -v || { /usr/local/bin/docker compose down; exit 1; }
	/usr/local/bin/docker compose down

clean: ## Remove build artefacts
	$(CARGO) clean
