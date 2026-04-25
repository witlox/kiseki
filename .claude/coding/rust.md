# Kiseki — Rust Coding Standards

Extends `.claude/guidelines/rust.md` with project-specific conventions.
Loaded for: implementer, adversary (implementation review).

## Workspace

- 12 crates under `crates/` (see `specs/architecture/module-graph.md`)
- Shared dependencies pinned at workspace level
- `kiseki-common` is the leaf crate — imports only stdlib + uuid + zeroize
- `kiseki-proto` is generated from .proto files; hand-edits are overwritten

## Unsafe Code Policy

`unsafe_code = "deny"` at workspace level. Explicit per-crate allowances:

| Crate | Unsafe allowed | Justification |
|---|---|---|
| `kiseki-transport` | Yes | libfabric-sys FFI bindings for Slingshot/CXI |
| `kiseki-crypto` | Yes | mlock/madvise for key material memory protection |
| `kiseki-client` | Yes | fuser FUSE bindings (if needed beyond safe wrapper) |

Every `#[allow(unsafe_code)]` requires a `// SAFETY:` comment.

## FIPS Crypto

- Symmetric encryption: AES-256-GCM via `aws-lc-rs` (FIPS-validated)
- Key derivation: HKDF-SHA256 via `aws-lc-rs`
- Chunk ID: SHA-256 (default) or HMAC-SHA256 (tenant opt-out)
- Key material: wrapped in `zeroize::Zeroizing<T>`, mlock'd pages

## Traits at Boundaries

Every bounded context exposes a trait (e.g., `LogOps`, `ChunkOps`,
`CryptoOps`). Within a crate, use concrete types.

## Error Handling

- `thiserror` for all error types (see `specs/architecture/error-taxonomy.md`)
- Every error categorized: Retriable, Permanent, Security
- Wrap with context: `.map_err(|e| KisekiError::from(e).with_context(...))`
- `anyhow` only in binary crates (`kiseki-server`, etc.)

## Async

- `tokio` multi-threaded runtime for I/O
- CPU-bound crypto on `tokio::task::spawn_blocking`
- `#[tokio::test]` for async tests

## Protobuf / gRPC

- `tonic` + `prost`, proto files in `specs/architecture/proto/kiseki/v1/`
- Generated code in `kiseki-proto` crate
- All gRPC messages carry: `tenant_id`, `DeltaTimestamp`, trace ID

## BDD

- `cucumber` crate for Gherkin, feature files in `specs/features/`
- Step definitions in `tests/acceptance/`, one file per feature
- @integration exercises real integrated code paths (gateway→composition→log)
- Depth requirements: see `.claude/roles/auditor.md`

## Domain Language

- All type names match `specs/ubiquitous-language.md` exactly
- New domain terms: check spec first, escalate if not found
- Full names in public APIs (`SequenceNumber`, not `SeqNum`)
