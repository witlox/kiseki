# Kiseki — root Makefile
#
# Targets here mirror the pre-commit discipline documented in
# `.claude/CLAUDE.md`: `make` runs fmt + lint + test + build across the
# full workspace. `make verify` is the strict CI-equivalent used by the
# `/project:verify` check.

.PHONY: all verify rust-fmt rust-fmt-check rust-clippy rust-test rust-deny \
        check fmt test build clean help arch-check e2e

SHELL := /bin/bash

# --- Rust toolchain commands ---
CARGO        ?= cargo
CARGO_TEST   ?= $(CARGO) test --workspace --all-targets
CARGO_BUILD  ?= $(CARGO) build --workspace --all-targets
# -D warnings promotes all warnings to errors.
# -A overrides for test-common patterns (unwrap, expect, panic, missing docs on macro-generated types).
CARGO_CLIPPY ?= $(CARGO) clippy --workspace --all-targets -- -D warnings \
	-A clippy::unwrap-used -A clippy::expect-used -A clippy::panic -A missing-docs
CARGO_FMT    ?= $(CARGO) fmt --all

all: check ## Default: fmt + lint + test

help: ## Show this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ---------------------------------------------------------------------
# Rust
# ---------------------------------------------------------------------

rust-fmt: ## Apply rustfmt to all crates
	$(CARGO_FMT)

rust-fmt-check: ## Check rustfmt without modifying files
	$(CARGO_FMT) -- --check

rust-clippy: ## cargo clippy with -D warnings
	$(CARGO_CLIPPY)

rust-test: ## cargo test --workspace
	$(CARGO_TEST)

rust-deny: ## cargo-deny (licenses, advisories, bans)
	@if command -v cargo-deny >/dev/null; then \
		$(CARGO) deny check; \
	else \
		echo "cargo-deny not installed; skipping (install: cargo install cargo-deny)"; \
	fi

rust-build: ## cargo build workspace
	$(CARGO_BUILD)

# ---------------------------------------------------------------------
# Architecture enforcement (ADV-3)
# ---------------------------------------------------------------------

arch-check: ## Verify kiseki-control depends only on allowed crates
	@! grep -E 'kiseki-(log|chunk|composition|view|gateway|client|keymanager|crypto|raft|transport|server|audit|advisory)' \
	    crates/kiseki-control/Cargo.toml \
	    || { echo "VIOLATION: kiseki-control depends on a data-path crate"; exit 1; }

# ---------------------------------------------------------------------
# Aggregate targets
# ---------------------------------------------------------------------

fmt: rust-fmt ## Apply all formatters

check: rust-fmt-check rust-clippy rust-test ## Standard pre-commit check

test: rust-test ## Run all tests

build: rust-build ## Build all artefacts

verify: rust-fmt-check rust-clippy rust-deny rust-test arch-check ## CI-equivalent strict verification

e2e: ## Run Python e2e tests (requires docker compose)
	/usr/local/bin/docker compose up --build -d
	.venv/bin/pytest tests/e2e/ -m e2e -v || { /usr/local/bin/docker compose down; exit 1; }
	/usr/local/bin/docker compose down

clean: ## Remove build artefacts
	$(CARGO) clean
