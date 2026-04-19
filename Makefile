# Kiseki — root Makefile
#
# Targets here mirror the pre-commit discipline documented in
# `.claude/CLAUDE.md`: `make` runs fmt + lint + test + build across the
# full workspace. `make verify` is the strict CI-equivalent used by the
# `/project:verify` check.

.PHONY: all verify rust-fmt rust-fmt-check rust-clippy rust-test rust-deny \
        go-fmt go-fmt-check go-vet go-test go-proto \
        check fmt test build clean help

SHELL := /bin/bash

# --- Rust toolchain commands ---
CARGO        ?= cargo
CARGO_TEST   ?= $(CARGO) test --workspace --all-targets
CARGO_BUILD  ?= $(CARGO) build --workspace --all-targets
CARGO_CLIPPY ?= $(CARGO) clippy --workspace --all-targets -- -D warnings
CARGO_FMT    ?= $(CARGO) fmt --all

# --- Go toolchain commands ---
GO       ?= go
GO_DIR   := control

# --- Protobuf ---
PROTO_ROOT := specs/architecture/proto
PROTO_FILES := $(wildcard $(PROTO_ROOT)/kiseki/v1/*.proto)

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
# Go
# ---------------------------------------------------------------------

go-fmt: ## gofmt -s -w
	cd $(GO_DIR) && gofmt -s -w .

go-fmt-check: ## gofmt -s -d (fails if any diff)
	cd $(GO_DIR) && test -z "$$(gofmt -s -d .)" || { echo "gofmt diffs detected"; gofmt -s -d .; exit 1; }

go-vet: ## go vet ./...
	cd $(GO_DIR) && $(GO) vet ./...

go-test: ## go test ./... with race detector
	cd $(GO_DIR) && $(GO) test -race ./...

go-build: ## build Go binaries under control/bin/
	cd $(GO_DIR) && mkdir -p bin && $(GO) build -o bin/kiseki-control ./cmd/kiseki-control && $(GO) build -o bin/kiseki-cli ./cmd/kiseki-cli

go-lint: ## golangci-lint (if installed)
	@if command -v golangci-lint >/dev/null; then \
		cd $(GO_DIR) && golangci-lint run ./...; \
	else \
		echo "golangci-lint not installed; skipping (install: https://golangci-lint.run/)"; \
	fi

# ---------------------------------------------------------------------
# Protobuf code-gen (Go side)
#
# Rust side is generated automatically by `cargo build` via the
# kiseki-proto `build.rs`. Go side needs explicit codegen before the
# Phase 11 control plane consumes the wire types. Requires `protoc`,
# `protoc-gen-go`, and `protoc-gen-go-grpc` on PATH.
# ---------------------------------------------------------------------

go-proto: ## Generate Go protobuf/gRPC code from specs/architecture/proto/
	@if ! command -v protoc >/dev/null; then \
		echo "protoc not installed (apt: protobuf-compiler)"; exit 1; \
	fi
	@if ! command -v protoc-gen-go >/dev/null; then \
		echo "protoc-gen-go not installed (go install google.golang.org/protobuf/cmd/protoc-gen-go@latest)"; exit 1; \
	fi
	@if ! command -v protoc-gen-go-grpc >/dev/null; then \
		echo "protoc-gen-go-grpc not installed (go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@latest)"; exit 1; \
	fi
	protoc \
		--proto_path=$(PROTO_ROOT) \
		--go_out=$(GO_DIR)/proto --go_opt=paths=source_relative \
		--go-grpc_out=$(GO_DIR)/proto --go-grpc_opt=paths=source_relative \
		$(PROTO_FILES)

# ---------------------------------------------------------------------
# Aggregate targets
# ---------------------------------------------------------------------

fmt: rust-fmt go-fmt ## Apply all formatters

check: rust-fmt-check rust-clippy rust-test go-fmt-check go-vet go-test ## Standard pre-commit check

test: rust-test go-test ## Run all tests

build: rust-build go-build ## Build all artefacts

verify: rust-fmt-check rust-clippy rust-deny rust-test go-fmt-check go-vet go-lint go-test ## CI-equivalent strict verification

clean: ## Remove build artefacts
	$(CARGO) clean
	rm -rf $(GO_DIR)/bin
