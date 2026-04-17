# Kiseki — Rust Coding Standards

Extends `.claude/guidelines/rust.md` with project-specific conventions.

## Workspace

- 12 crates under `crates/` (see `specs/architecture/module-graph.md`)
- Shared dependencies pinned at workspace level
- `kiseki-common` is the leaf crate — imports only stdlib + uuid + zeroize
- `kiseki-proto` is generated code — do not hand-edit

## Unsafe Code Policy

`unsafe_code = "deny"` at workspace level. Explicit per-crate allowances:

| Crate | Unsafe allowed | Justification |
|---|---|---|
| `kiseki-transport` | Yes | libfabric-sys FFI bindings for Slingshot/CXI |
| `kiseki-crypto` | Yes | mlock/madvise for key material memory protection |
| `kiseki-client` | Yes | fuser FUSE bindings (if needed beyond safe wrapper) |
| All others | No | |

Every `#[allow(unsafe_code)]` must have a `// SAFETY:` comment explaining
the invariants being upheld.

## FIPS Crypto

- All symmetric encryption: AES-256-GCM via `aws-lc-rs` (FIPS-validated)
- Key derivation: HKDF-SHA256 via `aws-lc-rs`
- Chunk ID: SHA-256 (default) or HMAC-SHA256 (tenant opt-out)
- Key material: wrapped in `zeroize::Zeroizing<T>`, mlock'd pages
- No `ring` in production paths (use only if aws-lc-rs unavailable)
- No custom crypto implementations

## Traits at Boundaries

Every bounded context exposes a trait (e.g., `LogOps`, `ChunkOps`,
`CryptoOps`). Within a crate, use concrete types.

```rust
// Good: trait at crate boundary
pub trait ChunkOps {
    fn write_chunk(&self, req: WriteChunkRequest) -> Result<WriteChunkResponse, KisekiError>;
}

// Good: concrete type within crate
struct ChunkStore { pool: AffinityPool, crypto: Arc<dyn CryptoOps> }
```

## Error Handling

- `thiserror` for all error types (see `specs/architecture/error-taxonomy.md`)
- Every error is categorized: Retriable, Permanent, Security
- Wrap with context: `.map_err(|e| KisekiError::from(e).with_context("chunk write"))`
- No `anyhow` in library crates; `anyhow` only in binary crates (`kiseki-server`, etc.)

## Async

- `tokio` multi-threaded runtime for I/O
- CPU-bound crypto operations on `tokio::task::spawn_blocking`
- No blocking I/O on async threads
- `#[tokio::test]` for async tests

## Protobuf / gRPC

- `tonic` for gRPC server/client
- `prost` for protobuf codegen
- Proto definitions in `specs/architecture/proto/kiseki/v1/`
- Generated code in `kiseki-proto` crate
- All gRPC messages carry: `tenant_id`, `DeltaTimestamp`, trace ID

## BDD

- `cucumber` crate for Gherkin scenario execution
- Feature files in `specs/features/`
- Step definitions in `tests/acceptance/`
- One step definition file per feature file

## Domain Language

- All type names match `specs/ubiquitous-language.md` exactly
- New domain terms: check spec first, escalate if not found
- No abbreviations in public APIs (write `SequenceNumber`, not `SeqNum`)
