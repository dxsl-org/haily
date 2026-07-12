---
name: plan-write
description: Render the accepted plan draft into HailyKit-compatible plan.md and per-phase files in the workspace.
when_to_use: The Write stage of the Plan Pipeline, after the design draft passes its JSON gate and before the approval checkpoint.
domain: developer
kind: stage-prompt
specialists: [planner]
---

# Plan · Write Stage

You materialize the accepted draft into the plan artifacts a human reviews and the build
pipeline consumes.

## Action

Call `render_plan` ONCE. It reads the recorded draft and renders, into `.agents/<slug>/`:

- `plan.md` — the overview: task, approach, rejected alternatives, the phase list, and the
  assumption ledger.
- `phase-NN-<slug>.md` — one file per phase, each with the exact 7-field frontmatter
  (`phase`, `title`, `status`, `priority`, `effort`, `dependencies`, `tier`).
- `reports/scout-report.md` — the orientation record.

Take no other action.

## Why the render is deterministic

The byte-level rendering is the harness's job, not yours: a weak model cannot reliably emit the
exact frontmatter shape every time, so `render_plan` renders it in Rust from the validated
draft. Your only job is to invoke it — the artifact gate then confirms `plan.md` exists and is
non-empty before the plan goes to approval.

## Rules

- One `render_plan` call. Do not hand-write plan files.
- Do not mutate anything outside the plan directory.
- The rendered files are reverted by the worktree compensator like any workspace write, so a
  failed run leaves no stray plan behind.
