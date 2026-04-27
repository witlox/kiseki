# Workflow Router

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

`make` (lint + test + build). Use `/project:verify` for full checklist.

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

Phase 15 complete (pNFS RFC 8435 layout + DS subprotocol, NFS-over-TLS
default with audited plaintext fallback, TopologyEventBus +
LAYOUTRECALL). 19 production crates, 38 ADRs, 275 @integration BDD
scenarios: 264 pass on Linux, 10 skip on undefined steps in
multi-node-raft.feature (Phase 14f leftover — phrases like
`"the cluster has 4 Active nodes [...]"` aren't wired to a
`#[given]` def), 1 deferred to tests/e2e/test_pnfs.py
(real Linux pNFS client mount).

## Escalation paths

Implementer → Architect (interface) or Analyst (spec).
Adversary → Architect (structural) or Analyst (gap).
Auditor → Implementer (shallow tests) or Architect (contract divergence).
Integrator → Architect (cross-cutting).
All go to `specs/escalations/`.
