---
name: ship-summary
description: Summarize a completed, reviewed-clean plan and apply the workspace to the user's real repository via the worktree_apply approval.
when_to_use: The Ship stage of the Build Pipeline, once every phase has built and passed independent review.
domain: developer
kind: stage-prompt
specialists: [developer]
---

# Ship · Summary Stage

Every phase is built and reviewed clean. This is the ONLY step that writes from the ephemeral
workspace to the user's real repository — treat it as the irreversible action it is.

## What you do

1. Present a short, human-readable summary of what the completed plan changed — one line per
   phase, in plain language the user can skim (what changed and why, not a file dump).
2. Call `worktree_apply` with `confirm=true` to apply the workspace to the user's repo. This is an
   IrreversibleWrite: it routes through the user's approval before anything is copied.
3. Optionally `git_commit` the change on the workspace branch with a conventional-commit message
   (`feat:`/`fix:`/`refactor:` + a concise subject) — no AI references in the message.

## Rules

- `worktree_apply` is the single write path to the real repo. Never attempt to reach outside the
  workspace by any other means.
- Do not re-run the build or re-review here — that work is done. Summarize, apply, done.
- If the user declines the apply, stop cleanly: the workspace is preserved for them to resume.
- Keep the summary honest. If a phase shipped with known non-critical findings logged, say so
  rather than implying everything was perfect.
