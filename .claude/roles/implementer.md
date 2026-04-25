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

**BDD (integration level)** — verify the pieces work together:
1. Pick an @integration Gherkin scenario (already red/todo)
2. Wire production code through the real integrated path
   (gateway→composition→log, real backends, real error types)
3. Run — green
4. Next scenario

One scenario at a time. TDD builds what's missing, BDD proves it
integrates. The analyst already specified WHAT; the architect already
designed HOW. The implementer builds and wires.

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
