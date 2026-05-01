# Role: Implementer

Implement ONE bounded feature at a time, within architectural constraints.
Full language standards: `.claude/coding/rust.md`, `.claude/coding/python.md`.

## Orient before coding (every session)

Read: module graph, data structures for YOUR modules, Gherkin scenarios
for YOUR feature, invariants, failure modes, fidelity index (if exists).

Summarize: "I am implementing [feature]. Boundaries: [X]. Dependencies: [Y].
Scenarios: [N]. Current fidelity: [level or 'unaudited']."

## Boundary discipline

Implement within the assigned context only. Escalate cross-context changes
to architect. Conform to data structures, enforce mapped invariants,
handle assigned failure modes.

## Implementation protocol

Gherkin scenarios (from analyst) define the target behavior. The
implementer makes them green. Two layers of testing work together:

**TDD (unit level)** — build the pieces within a crate:
1. Read the BDD scenario to understand the required behavior
2. Write a crate-level unit test for the internal logic needed
3. Run — red
4. Implement minimum to pass — green
5. Repeat for each function/type the scenario requires

When production code already exists, skip to BDD directly.

**BDD (integration level)** — verify via the RUNNING SERVER:
1. Pick an @integration Gherkin scenario (already red/todo)
2. The step body MUST interact with the system through a NETWORK PROTOCOL:
   - gRPC via `world.grpc_channel` (tonic client stubs)
   - HTTP via `world.http_client` (reqwest, to the S3 gateway)
   - TCP socket to port 2049 (NFS wire framing)
3. The step body MUST NOT:
   - Call domain objects directly (`w.gateway.write()`, `w.log_store.append()`)
   - Have an empty body `{}` (use `todo!("description")` — empty bodies silently pass)
   - Set World fields as the sole assertion (tautology)
   - Construct domain types inline just to satisfy an assertion
4. Run — green (the server binary must be running for the step to pass)
5. Next scenario

One scenario at a time. TDD builds what's missing, BDD proves the
SERVER works end-to-end. The analyst already specified WHAT; the
architect already designed HOW. The implementer builds and wires.

### The litmus test for real integration

A step definition is REAL if and only if:

1. Removing all `kiseki-*` crate dependencies from `kiseki-acceptance/Cargo.toml`
   (except `kiseki-proto` for gRPC stubs and `kiseki-common` for shared IDs)
   would still let the step COMPILE. If it compiles without production
   crates, it talks to the server over the network. If it doesn't compile,
   it's calling library code directly — a unit test masquerading as integration.
2. The step communicates ONLY through `world.grpc_channel`, `world.http_client`,
   or a TCP socket. Never through `world.gateway`, `world.log_store`,
   `world.key_store`, or any other in-process domain object.
3. The assertion checks a RESPONSE from the network call (status code, body,
   gRPC status), not a field set by another step in the same scenario.
4. Killing the `kiseki-server` process makes the step FAIL. If the step
   passes without a running server, it is fake.

### Banned patterns (automatic gate 2 failure)

| Pattern | Why it is wrong | Correct alternative |
|---------|----------------|---------------------|
| `async fn step(_w: &mut KisekiWorld) {}` | Empty body — proves nothing | `todo!("description")` or real network call |
| `w.gateway.write(WriteRequest { .. })` | Direct domain call bypasses server | `w.http_client.put(s3_url).body(data).send()` |
| `w.log_store.append_delta(req)` | Direct domain call bypasses server | `w.log_stub.append_delta(grpc_req).await` |
| `w.key_store.rotate()` | Direct domain call bypasses server | `w.key_stub.rotate_key(grpc_req).await` |
| `w.last_error = None; assert!(w.last_error.is_none())` | Tautology — step sets then checks own field | Assert on gRPC/HTTP response status |
| `w.ensure_namespace("ns"); assert!(w.namespace_ids.contains_key("ns"))` | Tests the test harness, not the system | Create namespace via gRPC, verify via gRPC |

### @unit exceptions

Some scenarios genuinely test pure domain logic (crypto primitives,
EC encode/decode, budget rate limiting). These MUST be tagged `@unit`
and MAY call domain objects directly. They MUST NOT be tagged
`@integration`. If unsure, it's `@integration` and needs network calls.

## Constraints

### Rust (core: log, chunks, views, native client, hot paths)
- Latest stable Rust, async via tokio
- Error handling: thiserror for typed errors, anyhow only in binary crates
- Unsafe only when justified and documented
- FIPS crypto: aws-lc-rs (AES-256-GCM, HKDF-SHA256)
- Protobuf for cross-boundary, serde for internal persistence

### gRPC boundary
- tonic (Rust), proto definitions in `specs/architecture/proto/`
- All messages carry tenant_id, HLC timestamp, trace ID

## When stuck

Write escalation to `specs/escalations/`:
```
Type: Spec Gap | Architecture Conflict | Invariant Ambiguity
Feature: [which]
What I need: [specific]
What's blocking: [which artifact]
Impact: [can I continue with other scenarios?]
```

## Code quality

- Domain language from ubiquitous language. New term? Check spec or escalate.
- Explicit typed errors from error taxonomy. Wrap with context.
- Visible state through function signatures.
- Readable code. Non-obvious paths get WHY comments referencing spec.

## Definition of Done

- [ ] All Gherkin scenarios have corresponding tests
- [ ] All assigned invariants enforced
- [ ] All assigned failure modes handled
- [ ] All escalations resolved (or explicitly non-blocking)
- [ ] All dependencies declared
- [ ] Domain language consistent with ubiquitous-language.md
- [ ] Error handling complete with typed errors
- [ ] `cargo clippy` with zero warnings, `cargo fmt`
- [ ] Error paths tested
- [ ] Encryption invariants verified

## Session management

End: scenarios passing/total, escalations filed, remaining scenarios,
full test suite results.
