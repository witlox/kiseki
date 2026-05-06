# Workflow Router

Correctness over velocity. Every shortcut becomes debt.

Role definitions in `.claude/roles/`. Read the relevant role file when
activating a mode. These are behavioral constraints.

## Role → files to load

| Role | Load these files |
|------|-----------------|
| analyst | `roles/analyst.md` |
| architect | `roles/architect.md`, `coding/rust.md` |
| adversary | `roles/adversary.md`, `coding/rust.md` (impl review) |
| implementer | `roles/implementer.md`, `coding/rust.md`, `coding/python.md`, `guidelines/engineering.md` |
| auditor | `roles/auditor.md` |
| integrator | `roles/integrator.md`, `guidelines/ci.md` |

Standards: `.claude/guidelines/`. Coding: `.claude/coding/`.

## Pre-commit

Three test tiers, cascading. Each higher tier includes the lower:

| Tier | What | Make target | When |
|---|---|---|---|
| **1 (fast — default)** | fast unit tests (workspace minus `kiseki-acceptance`, `#[ignore = "slow:…"]` skipped) + BDD `@smoke` | `make test-fast` (alias `make test`) | between every code edit; pre-commit |
| **2 (slow)** | Tier 1 + slow-marked unit tests (`--run-ignored=only`) + full BDD suite | `make test-slow` | pre-PR |
| **3 (full)** | Tier 2 + Python e2e via docker compose | `make test-full` | pre-merge / nightly / pre-release |

`make` (no target) = `make verify` = fmt-check + clippy + Tier 1 + arch-check. Run before every commit.

GitHub Actions mirrors the tiers:
- `ci.yml` runs Tier 1 on PR + push (merge-blocking).
- `bdd.yml` runs Tier 2 on push-to-main + nightly (advisory).
- `release.yml` runs Tier 3 weekly + on `workflow_dispatch` (gates the release).

To mark a unit test as slow so Tier 1 skips it:
```rust
#[test]
#[ignore = "slow: <reason — what makes this expensive>"]
fn my_expensive_test() { ... }
```

## Automatic commands

| Command | When |
|---|---|
| `/project:status` | First message of every new session |
| `/project:verify` | Before every commit |
| `/project:spec-check` | After completing a build phase or spec change |
| `/project:e2e` | After cross-context boundary changes |

## Mode detection

### Step 1: Project state

1. `specs/fidelity/INDEX.md` with checkpoint? → Baselined
2. `specs/fidelity/SWEEP.md` IN PROGRESS? → Resume sweep
3. Source code exists and tested? → Brownfield with baseline
4. Near-empty? → Pure greenfield

### Step 2: User intent → role

| Intent | Mode | Role |
|--------|------|------|
| status | ASSESS | Read indexes |
| sweep / baseline | SWEEP | auditor |
| adversary sweep | ADV-SWEEP | adversary |
| audit [X] | AUDIT | auditor |
| implement / add | FEATURE | implementer |
| fix / bug / error | BUGFIX | implementer |
| design / spec | DESIGN | analyst or architect |
| review / find flaws | REVIEW | adversary |
| integrate | INTEGRATE | integrator |
| continue / next | RESUME | Read sweep state |
| Unclear | ASK | |

### Step 3: Before acting, one line

```
Mode: [MODE]. Project: [state]. Role: [role]. Reason: [why].
```

## Protocols

**Feature**: analyst → spec | architect → interfaces | adversary → gate 1 | implementer → BDD+code | auditor → gate 2 | adversary → findings | integrator (if cross-feature).

Gate 2: auditor verifies step depth. See `roles/auditor.md`.

**Bugfix**: diagnose → failing test first → fix → audit depth → update index.

**Design**: new domain → analyst | arch change → architect | ADR → write it. Adversary reviews before implementation.

**Sweep**: fidelity (auditor) and adversary in parallel. LOW areas get higher adversary priority.

## Entry point

Project state — phase, scope, counts — lives in the root `CLAUDE.md`.
This file is the workflow router only; load the role file for the
mode you're entering and read the project state from `CLAUDE.md`.

## Escalation paths

Implementer → Architect (interface) or Analyst (spec).
Adversary → Architect (structural) or Analyst (gap).
Auditor → Implementer (shallow tests) or Architect (contract divergence).
Integrator → Architect (cross-cutting).
All go to `specs/escalations/`.
