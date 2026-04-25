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

### BDD test tiers

Scenarios are tagged `@unit` or `@integration`:

| Tier | Backend | Speed | When to run |
|------|---------|-------|-------------|
| `@unit` | `MemShardStore`, `InMemoryGateway` | Fast (<5s total) | Every CI run |
| `@integration` | `PersistentShardStore` or `RaftShardStore`, real routing | Slower (~30s+) | Pre-release, `/project:e2e` |

**Rules:**

- Scenarios that test distributed behavior (Raft consensus, replication,
  leader election, failover, multi-node placement, shard routing, node
  drain, persistence/crash-recovery) MUST be `@integration`. Testing
  these against `MemShardStore` produces false greens.
- Scenarios that test algorithmic correctness (sequence monotonicity,
  watermark GC, EXDEV rejection, compaction dedup, key derivation,
  encryption roundtrip) may be `@unit`.
- A step function MUST NOT have an empty body. If the behavior cannot
  be implemented yet, use `todo!("reason")` so the test fails visibly.
  Green-but-empty is worse than red-and-known.
- Tautological assertions (`x.is_none() || x.is_some()`, `assert!(true)`,
  `usize >= 0`) are bugs. Every assertion must be falsifiable.
- Error scenarios must trigger real failure paths, not set
  `last_error = Some("message")` directly.

### Auditor gate for BDD depth

The auditor classifies every step function:

| Depth | Definition | Acceptable for |
|-------|-----------|----------------|
| STUB | Empty body or comment-only | Nothing — must be `todo!()` |
| SHALLOW | Checks a flag/boolean without exercising real code | `@unit` non-critical paths only |
| MOCK | Exercises real logic against in-memory backends | `@unit` scenarios |
| THOROUGH | Exercises real code with real backends and meaningful assertions | `@integration` scenarios |

Auditor gate 2 fails if any `@integration` scenario has steps below THOROUGH.

## Domain Language

- All type names match `specs/ubiquitous-language.md` exactly
- New domain terms: check spec first, escalate if not found
- No abbreviations in public APIs (write `SequenceNumber`, not `SeqNum`)
