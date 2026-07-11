# Coding Eval Fixtures (Sub-Agent + Skill Architecture P9)

Permanent, committed fixture repos for `haily eval coding`. Each is a small (<30 files) standalone
project (NOT a workspace member — `evals/` is `exclude`d from the root Cargo workspace) carrying a
**planted defect that IS the task**. Every fixture built/type-checked clean *before* its defect was
introduced; the eval scores whether the model resolves the defect so the fixture's own
language-native gate passes.

## `task.yaml` schema (P9)

A FLAT scalar set + one string list (parsed by `crate::pipeline::eval_runner::manifest`, NOT a
general-YAML lib — use single-line, optionally-quoted values, no block scalars):

```yaml
id: rust-fix-compile           # fixture id == eval_runs.task_id
language: rust                 # rust | typescript | python | go
kind: fix-compile-error        # fix-compile-error | fix-failing-test | feature-with-tests | refactor-rename
description: "One-line task."   # what the model is told to do
gate: cargo test               # deterministic pass condition (exit 0 == pass); NOT an LLM judge
max_tool_calls: 25
max_escalations: 0
timeout_seconds: 120
calibration: hard              # OPTIONAL — marks a fixture known to fail single-pass on a weak model
invariants:                    # audit/report only; structural guards enforce them
  - "no writes outside the workspace root"
  - "tests unchanged"
```

## Coverage

| Fixture | Language | Kind | Calibration |
|---------|----------|------|-------------|
| rust-fix-compile | Rust | fix-compile-error | |
| rust-fix-test | Rust | fix-failing-test | hard |
| rust-refactor-rename | Rust | refactor-rename | |
| ts-add-feature | TypeScript | feature-with-tests | |
| python-fix-test | Python | fix-failing-test | |
| python-add-feature | Python | feature-with-tests | hard |
| go-fix-compile | Go | fix-compile-error | |

Four languages exercise the language-agnostic gate / repo-map / lint beyond Rust; two
`calibration: hard` tasks prevent ceiling effects that would mask model differences.

## Running

The BASELINE MATRIX RUN (`local × {Normal, Deep} × escalation {off,on}`) needs a configured
local/cloud model host and is a **manual step** (see `docs/project-roadmap.md`). The scripted-LLM
goldens in `crates/haily-core/tests/coding_goldens.rs` exercise the pipeline behavior + eval-mode
invariants in CI with zero network.
