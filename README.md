# Kiseki — Handoff Package

Distributed storage system for HPC / AI workloads on Slingshot and
commodity fabrics. Intellectual-itch investigation transitioning to
structured design via diamond workflow.

---

## What is in this package

```
docs/
├── analysis/
│   └── design-conversation.md    # Distilled 16-turn design conversation
└── prior-art/
    └── deltafs-mochi-evaluation.md    # DeltaFS + Mochi comparison

specs/
└── SEED.md    # Analyst seed: candidate terms, invariants, question bank
```

This is deliberately a small package. It contains **context for the
analyst**, not **output from the analyst**. The analyst produces the
full `specs/` tree through interrogation, per the diamond workflow
role definition.

---

## What is NOT in this package (and why)

- **No `specs/domain-model.md`, `specs/invariants.md`, etc.** — These
  are the analyst's outputs, not inputs. Pre-filling them would
  undercut the interrogation step.
- **No `ADR/` decisions** — These come from the architect after the
  analyst graduates. The analyst will surface decision points; the
  architect will record them.
- **No code skeleton, no `Cargo.toml`, no build scaffolding** — Per
  the analyst role definition, no code exists until scope is pinned
  down.
- **No `.claude/` workflow config** — Assumed to be copied from
  the reference workflow (ghyll) and adapted.

---

## How to run the analyst on this

From the project root, after copying in the standard `.claude/`
workflow configuration:

```
# Mode: DESIGN. Project: pure greenfield. Role: analyst.
# Reason: new domain, no specs yet, conversation distilled.

Switching to analyst. Previous: (none).
```

The analyst should:

1. Read `docs/analysis/design-conversation.md` first. It's the
   source material.
2. Read `docs/prior-art/deltafs-mochi-evaluation.md` second. It
   contains domain insights from prior work that shape interrogation
   priorities.
3. Read `specs/SEED.md` third. It contains candidate terms,
   suspected invariants, failure modes, and a question bank —
   all explicitly marked as unvalidated hypotheses for the analyst
   to probe.
4. Begin Layer 1 interrogation per the role definition. Start with
   the existential questions (Q-E1, Q-E2, Q-E3 in SEED.md) — these
   can legitimately close or reshape the whole project.
5. Produce specs under `specs/` as interrogation proceeds.

---

## Known tensions the analyst should not paper over

These are surfaced explicitly in the design conversation. If the
analyst allows them to be implicitly resolved, the architect will
inherit bad specs. The analyst's job is to force them into the
open:

1. **Kiseki vs. DeltaFS differentiation**. Architectural overlap is
   significant. Differentiation is real (persistence, multi-tenancy,
   standard protocols, **first-class encryption**) but needs to be
   confirmed as the actual reason to build, not a post-hoc
   rationalization.

2. **Build-on-Mochi vs. build-pure-Rust**. Not settled. Shapes
   language strategy, dependency risk, and scope.

3. **Log-corruption blast radius**. Log-structured architecture has
   catastrophic failure modes for a corrupted log. The design
   conversation acknowledged this without resolving it.

4. **Cross-view / cross-protocol consistency**. Declared "view
   descriptor decides semantics" but actual semantics not specified.

5. **v1 scope**. Multiple plausible v1s were discussed; none committed.

6. **Compaction strategy**. Named as the operational-make-or-break
   concern; not designed.

7. **Tenant-service density**. Dedicated-per-tenant vs. shared-with-
   isolation was named; not resolved.

8. **Encryption as first-class citizen (late addition)**. Surfaced
   after the initial package was drafted. It is a pillar, not a
   feature. Neither DeltaFS nor Mochi provide an encryption design
   to inherit. Threat model, key hierarchy, KMS boundary,
   dedup-vs-crypto conflict, RDMA-vs-crypto conflict, and
   crypto-shred semantics are all unresolved. The fact that this
   commitment surfaced late is itself signal — the analyst should
   probe for other implicit pillars the domain expert has not yet
   named.

---

## Scope honesty

This is an intellectual-itch project. The domain expert explicitly
named it as mode (c) — design exploration. That is relevant because:

- Success does not require shipping. A design doc that concludes
  "don't build this, adopt DeltaFS + extensions instead" is a
  valid outcome.
- Budget is presumably limited. The analyst should probe whether
  the domain expert intends to build, design-only, or something
  in between. This shapes how rigorously the later stages need
  to be pursued.
- The hardware substrate (ClusterStor E1000/E1000F) is real, so
  there is an operational fallback: even a narrower system that
  repurposes the hardware reasonably is a success.

---

## Project identity

- **Name**: Kiseki (軌跡 — Japanese: locus, trajectory, trace)
  - Searched for software collisions; none found
  - Cultural overlap with "The Legend of Heroes: Kiseki" JRPG
    series exists but is not expected to impede a storage project
- **Core language (committed)**: Rust
- **Control plane language (committed)**: Go
- **Boundary**: gRPC / protobuf
- **Client bindings**: Rust native + C FFI, Python (PyO3 or
  equivalent), C++ wrapper

---

## Escalation paths from analyst

If the analyst hits:

- **Missing architectural decision** that blocks spec completion →
  defer to architect session; log in `specs/escalations/`
- **Domain expert confirms "don't build this"** → produce a
  decision record explaining why, archive the specs, close the
  project
- **Domain expert wants to pivot scope substantially** →
  update `docs/analysis/design-conversation.md` with new scope,
  revise SEED.md, restart from Layer 1

---

## Questions the analyst should anchor on

Repeating the four existential questions from SEED.md here because
they really do gate everything else:

> **Q-E1**: Given DeltaFS exists and is remarkably close
> architecturally, what does Kiseki do that DeltaFS + extensions
> could not?
>
> **Q-E2**: Is Kiseki expected to be production-grade, a research
> prototype, or somewhere in between?
>
> **Q-E3**: Is building on Mochi's Mercury/Bake/SDSKV substrate
> on the table, or ruled out?
>
> **Q-E4**: What is the threat model? Encryption is first-class,
> but "first-class encryption defending against X" is a very
> different system from "first-class encryption defending against
> Y". The threat model determines which of the downstream Q-K
> questions have hard answers vs. soft answers.

These should be the analyst's first four questions to the domain
expert. Everything else depends on the answers.
