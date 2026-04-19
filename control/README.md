# `control` — Kiseki Control Plane (Go)

Control plane per `specs/architecture/build-phases.md#Phase-11`.

## Scope

Tenancy, IAM, policy, flavor matching, federation, audit export, advisory
policy, fabric discovery. Go 1.24+, standard library preferred. This
module NEVER touches the data path — data-path code is Rust only.

## Protobuf

Canonical `.proto` files live in `specs/architecture/proto/kiseki/v1/`.
Go code is generated into `control/proto/kiseki/v1/` via `make go-proto`
(see workspace `Makefile`). The Rust side regenerates at `cargo build`
time via the `kiseki-proto` build script.

## Binaries

- `cmd/kiseki-control` — API server (mTLS-authed, management network).
- `cmd/kiseki-cli`     — admin CLI.

Both are Phase 0 scaffolds; real behaviour lands in Phase 11.
