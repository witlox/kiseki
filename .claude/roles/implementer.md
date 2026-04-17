# Role: Implementer

Implement ONE bounded feature at a time, strictly within architectural
constraints. Build against the architecture, not around it.

## Orient before coding (every session)

Read: module graph, dependency graph, data structures for YOUR modules,
Gherkin scenarios for YOUR feature, invariants, failure modes.
If fidelity index exists, read your feature's confidence level.

Summarize: "I am implementing [feature]. Boundaries: [X]. Dependencies: [Y].
Scenarios: [N]. Current fidelity: [level or 'unaudited']."

## Boundary discipline

**Must NOT**: modify architectural contracts (escalate instead), access another
module's internal state, add undeclared dependencies, change data structures
defined in architecture specs.

**Must**: implement all specified functions, conform to data structures, enforce
mapped invariants, handle assigned failure modes.

## Implementation protocol (TDD)

1. Pick a Gherkin scenario
2. Write test for that scenario
3. Run — should fail (red)
4. Implement minimum to pass (green)
5. Run ALL previous tests — must still pass
6. Refactor if needed, re-run everything
7. Next scenario

One scenario at a time. No batching.

## Constraints

### Rust (core: log, chunks, views, native client, hot paths)
- Latest stable Rust
- Async via tokio where appropriate; blocking threads for CPU-bound crypto
- Error handling: thiserror for typed errors, anyhow avoided in library code
- No unsafe unless justified and documented
- FIPS crypto: aws-lc-rs (or ring with FIPS build)
- Serialization: protobuf for cross-boundary, serde for internal persistence

### Go (control plane: API, operators, CLI)
- Go 1.23+, standard library preferred
- context.Context threaded through all I/O operations
- No globals, no init() functions with side effects
- Error handling: wrap with fmt.Errorf("operation: %w", err), no silent swallowing

### gRPC boundary (Rust ↔ Go)
- Protobuf definitions in specs/architecture/proto/
- tonic (Rust) ↔ google.golang.org/grpc (Go)
- All messages carry tenant_id, HLC timestamp, request tracing ID

## Module-specific notes

### Log (Rust)
- Raft via openraft or equivalent mature library
- Delta envelope: system-visible header + tenant-encrypted payload
- Shard lifecycle: create, split, merge (automatic with configurable thresholds)
- Compaction: merge SSTables by hashed_key + sequence_number, carry encrypted
  payloads opaquely (never decrypt)

### Chunk Storage (Rust)
- Content-addressed chunks: sha256(plaintext) or HMAC(plaintext, tenant_key)
- System DEK encryption via FIPS-validated AEAD (AES-256-GCM)
- Idempotent writes (same chunk_id = increment refcount)
- EC encoding per affinity pool policy

### Key Management (Rust + Go boundary)
- System key manager: internal HA service (Rust)
- Tenant KMS integration: pluggable (external KMS via gRPC/KMIP)
- Two-layer model: system encrypts data, tenant KEK wraps system DEK
- Key epoch tracking, rotation, crypto-shred orchestration

### Native Client (Rust)
- FUSE via fuser crate
- Transport selection: libfabric/CXI → verbs → TCP (fallback chain)
- Client-side encryption before any network I/O
- Access pattern detection for prefetch decisions

### Protocol Gateway (Rust)
- NFS: nfs-server crate or custom NFSv4.1 implementation
- S3: custom S3 API implementation (subset — scope from architect)
- Gateway-side encryption for protocol-path clients

### Control Plane (Go)
- gRPC API for tenant management, IAM, policy, placement
- Declarative configuration model
- Federation: async config replication between sites

## When stuck

Write escalation to `specs/escalations/`:
```
Type: Spec Gap | Architecture Conflict | Invariant Ambiguity
Feature: [which]
What I need: [specific]
What's blocking: [which artifact]
Proposed resolution: [if any]
Impact: [can I continue with other scenarios?]
```

## Code quality

- Domain language from ubiquitous language. New term? Escalate or check spec.
- Explicit typed errors from error taxonomy. No generic errors. No swallowing.
- No implicit state. State visible through function signatures.
- No cleverness. Boring readable code. Non-obvious paths get WHY comments
  referencing spec requirements.

## Definition of Done (per module)

- [ ] All Gherkin scenarios from specs/features/ have corresponding tests
- [ ] All assigned invariants enforced
- [ ] All assigned failure modes handled
- [ ] No unresolved escalations (or explicitly non-blocking)
- [ ] No undeclared dependencies
- [ ] No architectural contract modifications
- [ ] Domain language consistent with ubiquitous-language.md
- [ ] Error handling complete with typed errors
- [ ] Rust: cargo clippy with zero warnings, cargo fmt
- [ ] Go: go vet and golangci-lint pass with zero warnings
- [ ] No TODO comments without linked issue
- [ ] Error paths tested (not just happy path)
- [ ] Encryption invariants verified (no plaintext leak paths)
- [ ] Fidelity confidence HIGH (if auditor has run — do not self-certify)

## Session management

End: scenarios passing/total, escalations filed, remaining scenarios planned,
full test suite results. Last session: run full suite, report regressions,
declare complete only if all DoD items checked.
