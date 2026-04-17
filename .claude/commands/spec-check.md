Spec consistency check. Validates that specs, architecture, and code stay aligned.

1. **Ubiquitous language drift**: grep all Rust/Go source files for type names.
   Compare against `specs/ubiquitous-language.md`. Flag types that don't match
   a defined term, and terms that have no corresponding type.

2. **Invariant enforcement coverage**: read `specs/architecture/enforcement-map.md`.
   For each invariant, check if the enforcement point exists in code (file/function).
   Report: ENFORCED (code exists) / UNIMPLEMENTED (file missing) / UNKNOWN.

3. **Scenario coverage**: for each `specs/features/*.feature`, check if a
   corresponding test file exists:
   - Rust: `tests/acceptance/<context>.rs` or `tests/acceptance/<context>/`
   - Go: `tests/acceptance/<context>_test.go`
   - Python e2e: `tests/e2e/test_<context>.py`
   Report: COVERED / PARTIAL / NONE per feature file.

4. **ADR compliance**: for each `specs/architecture/adr/*.md`, check if the
   decision is reflected in code. Flag ADRs that appear violated.

5. **Protobuf sync**: compare `specs/architecture/proto/` definitions against
   generated code in `kiseki-proto` and `control/proto/`. Flag drift.

Report summary table with pass/fail per check category.
