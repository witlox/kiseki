Project status assessment. Run at the start of any session.

1. Read `specs/architecture/build-phases.md` — identify current phase
2. Check for source code: `ls crates/*/src/lib.rs 2>/dev/null` — which crates exist?
3. Check for Go code: `ls control/pkg/*/**.go 2>/dev/null` — which packages exist?
4. Check test state: `cargo test --no-run 2>&1 | tail -5` — does it compile?
5. Check fidelity index: `cat specs/fidelity/INDEX.md 2>/dev/null` — has auditor run?
6. Check open escalations: `ls specs/escalations/*.md 2>/dev/null`
7. Check git status: uncommitted changes? Ahead/behind remote?

Report:
- Current build phase (0-12)
- Crates implemented / total (N/12)
- Go packages implemented / total
- Test status (compiles? passes? coverage?)
- Fidelity status (sweep done? confidence levels?)
- Open escalations
- Recommended next action
