# Workflow Router

Role definitions in `.claude/roles/`. Read the relevant role file when
activating a mode. These are behavioral constraints, not suggestions.

## Standards

Engineering guidelines in `.claude/guidelines/` (general, cross-project):
- `engineering.md` — commits, errors, code org, testing philosophy
- `rust.md` — Rust tooling, style, clippy, cargo-deny
- `go.md` — Go tooling, style, golangci-lint
- `python.md` — Python tooling, ruff, mypy, pytest
- `ci.md` — CI/CD pipeline structure
- `docs.md` — documentation requirements

Project-specific coding standards in `.claude/coding/`:
- `rust.md` — kiseki Rust: unsafe policy, FIPS crypto, traits, BDD
- `go.md` — kiseki Go: control plane, gRPC boundary, godog
- `python.md` — kiseki Python: PyO3 bindings, e2e test scripting

## Pre-commit discipline

Before committing: `make` (runs lint + test + build). Use `/project:verify`
for the full checklist.

## Automatic command invocation

| Command | When to invoke automatically |
|---|---|
| `/project:status` | **First message of every new session.** Establishes project state before any work. |
| `/project:verify` | **Before every commit.** Do not commit without running this. If it fails, fix and re-run. |
| `/project:spec-check` | **After completing a build phase.** Validates specs still align with code before moving to next phase. Also run after any spec or architecture change. |
| `/project:e2e` | **After Phase 12 (integration) and before declaring integration complete.** Also run after any change that touches cross-context boundaries. |

## Mode detection (every response)

### Step 1: Project state

1. `specs/fidelity/INDEX.md` with checkpoint? -> Baselined (current: CHECKPOINT)
2. `specs/fidelity/SWEEP.md` IN PROGRESS? -> Resume sweep
3. Source code exists and tested? -> Brownfield with baseline
4. Near-empty? -> Pure greenfield

### Step 2: User intent -> mode -> role

| Intent | Mode | Role |
|--------|------|------|
| status | ASSESS | Read indexes |
| sweep / baseline | SWEEP | auditor |
| adversary sweep / security review | ADV-SWEEP | adversary |
| audit [X] | AUDIT | auditor |
| implement / add | FEATURE | Feature Protocol |
| fix / bug / error | BUGFIX | Bugfix Protocol |
| design / spec | DESIGN | Design Protocol |
| review / find flaws | REVIEW | adversary |
| integrate | INTEGRATE | integrator |
| continue / next | RESUME | Read sweep state |
| Unclear | ASK | |

### Step 3: Before acting, one line

```
Mode: [MODE]. Project: [state]. Role: [role]. Reason: [why].
```

## Role switching

On switch: `Switching to [role]. Previous: [role].`
Read `.claude/roles/[role].md`. Apply its constraints.

## Protocols

**Feature**: analyst -> spec | architect -> interfaces | adversary -> gate 1 | implementer -> BDD+code | auditor -> gate 2 | adversary -> findings | integrator (if cross-feature). Done = scenarios pass + fidelity HIGH + adversary signed off.

**Bugfix**: diagnose -> failing test first -> fix -> audit depth -> update index.

**Design**: new domain -> analyst | arch change -> architect | ADR -> write it. Adversary reviews before implementation.

**Sweep**: fidelity (auditor) and adversary can run in parallel. Fidelity first when possible — LOW areas get higher adversary priority.

## Entry point

**Design complete, ready for implementation** (current state): full spec tree (analyst) + architecture (architect) + adversary review + analyst backpass all done. 56 invariants, 132 scenarios, 19 ADRs, 13 build phases. No code yet. Enter via FEATURE mode with implementer role. Start at Phase 0 (kiseki-common + kiseki-proto) per `specs/architecture/build-phases.md`.

## Escalation paths

Implementer -> Architect (interface) or Analyst (spec). Adversary -> Architect (structural) or Analyst (gap). Auditor -> Implementer (shallow tests) or Architect (contract divergence). Integrator -> Architect (cross-cutting). All go to `specs/escalations/`.
