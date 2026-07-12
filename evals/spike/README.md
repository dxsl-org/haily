# Phase 0 — Track B Capability + Undo Spike (THROWAWAY)

> This directory is **throwaway spike scaffolding**, not production code and not a CI suite.
> Its only job is to produce the go/no-go numbers for `reports/phase-00-spike-report.md`.
> Delete it once P0's verdict is recorded (the permanent coding eval is P9, `evals/fixtures/`).

## What the spike must answer

Two load-bearing premises of the whole sub-agent/skill plan are unvalidated:

1. **Undo soundness** — does the chosen compensator (`git checkout -- . && git clean -fd`)
   fully revert a workspace *including untracked build artifacts* (`target/`, `node_modules/`)
   to a bit-identical tree? → **Track B-undo** (`undo/`), scriptable, no model needed.
2. **Weak-model capability** — can the configured local model produce parseable-AND-coherent
   coding output within a bounded tool-call budget, **with the full ACI in place** (lint-on-edit,
   hash-anchored edits, repo-map orientation)? ACI value is inversely proportional to model
   capability (SWE-agent: +10.7pp), so measuring bare prompts would understate the shipped
   config. → **Track B-capability** (`fixtures/`), requires the local model host.

## Go / No-Go decision rule (records into the spike report)

- **GO for P1–P9 as designed** iff, across the fixtures, the local model reaches a passing gate
  (build/type-check + tests green, no out-of-workspace writes, journal complete) on **≥ 2 of 3**
  fixtures within **≤ 25 tool calls each**, AND the undo spike reverts bit-identically.
- **CONDITIONAL GO (cloud tier for coding)** iff undo is sound but the local model clears
  **≤ 1 of 3** — record "coding pipeline requires a stronger (cloud) model tier", proceed with
  the harness but default the coding router tier to cloud.
- **NO-GO / redesign** iff undo is unsound (Track A/undo must be fixed before any P1 exec ships).

The recommended default stage tool-call budget is `min(25, observed coherence ceiling)`.

## Track B-undo — run now (no model)

```powershell
pwsh evals/spike/undo/run_undo_spike.ps1     # Windows
```
```bash
bash evals/spike/undo/run_undo_spike.sh       # macOS/Linux
```
Asserts: after a build that produces `target/`, a tracked-file edit, and planted untracked
files, `git checkout -- . && git clean -fd` yields a tree hash bit-identical to the clean commit.

## Track B-capability — run on the local-model host (manual)

The full plan→build→verify pipeline does not exist yet (that is P4–P6), so this track is a
**manual protocol** until then:

1. Provision the sandbox: set `HAILY_WSL_DISTRO=<managed-distro>` (see Track A) so exec is
   enforcing; otherwise `NullSandbox` forces first-exec approval (fine for a manual run).
2. For each `fixtures/<task>/`: copy it to a scratch git repo, give the model the `task.yaml`
   `description` + the ACI tool surface, cap at the `max_tool_calls` budget.
3. Score with the language's own gate (`task.yaml.gate`) — NOT an LLM judge:
   ```
   fixtures/rust-fix-compile   →  cargo test
   fixtures/python-fix-test    →  pytest
   fixtures/ts-add-feature     →  npm test   (tsc + node assert)
   ```
4. Record per fixture: passed? tool-call count, tool-call count at first incoherence, egress
   (local/cloud). Enter the numbers into `reports/phase-00-spike-report.md`.

Each fixture is guaranteed to **build/type-check clean before its defect is introduced** — the
defect is the task. The gate is deterministic and language-native (proves the language-agnostic
gate premise beyond Rust).
