# Documentation Maintenance

## Required Documentation Per Project

1. **README.md** — purpose, quick-start, architecture overview, license
2. **CONTRIBUTING.md** — dev setup, coding standards, PR process, testing requirements
3. **CLAUDE.md** — project context for AI assistants: architecture, conventions, build commands
4. **.claude/CLAUDE.md** — workflow router: phase detection, role routing, escalation paths

## Spec Documents (specs/)

- `ubiquitous-language.md` — domain glossary, kept in sync with code
- `domain-model.md` — entities, value objects, aggregates
- `invariants.md` — numbered list of system invariants
- `features/*.feature` — Gherkin behavioral specs
- `fidelity/INDEX.md` — test depth per invariant (THOROUGH/MODERATE/NONE)
- `cross-context/` — integration points between bounded contexts
- `failure-modes.md` — known failure scenarios and handling

## Architecture Decision Records

- Stored in `specs/architecture/adr/`
- Record context, decision, and consequences
- Append-only: supersede old ADRs with new ones, don't edit
- Number sequentially (001, 002, ...)

## Inline Documentation

- Doc comments explain WHY, not WHAT
- All public items documented (rustdoc `///`)
- Module-level docs (`//!` in Rust)
- Code comments only where logic isn't self-evident

## Keeping Docs Current

- README updated as part of any PR that changes setup, build, or test
- ADRs written when architectural decisions are made (not retroactively)
- Fidelity index updated after every test sweep
- Spec documents updated when domain model evolves
- Stale docs are worse than no docs — delete rather than leave misleading
