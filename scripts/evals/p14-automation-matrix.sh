#!/bin/sh
# P14 — Per-candidate-model automation/connector matrix (Sub-Agent + Skill Architecture, Phase 14).
#
# HONEST GAP (documented, not patched here — the phase-07 constraint is against adding harness
# entrypoints, not against saying so plainly): unlike P9, there is no CLI wiring for a model-
# DRIVEN automation run. `run_automation_eval`
# (crates/haily-core/src/pipeline/automation_eval/mod.rs) only replays a fixture's SCRIPTED
# `steps` — no LLM is plumbed in to GENERATE tool calls yet (see evals/mock_saas/README.md's
# "per-candidate-MODEL matrix (DEFERRED)" section). There is therefore nothing this script can
# skip on a missing host — the missing piece is the harness wiring itself, not a reachable host.
#
# What this script DOES run: the CI-tier scripted-step goldens
# (crates/haily-core/tests/automation_goldens.rs) — zero-network, always available — as the
# closest real proxy, with an explicit note every time about what's still missing.
#
# Usage: sh scripts/evals/p14-automation-matrix.sh
set -eu
cd "$(dirname "$0")"
. ./lib.sh
cd_repo_root

if [ -n "${HAILY_EVAL_MODEL:-}" ]; then
    echo "NOTE: HAILY_EVAL_MODEL is set, but no CLI entrypoint yet drives the automation eval" \
         "with a real model generating tool calls (only the scripted-step CI tier below exists)." \
         "See docs/runbooks/pipeline-evals.md#p14 — tracked as a follow-up, not fabricated here." >&2
else
    echo "NOTE: the per-candidate-model automation matrix has no CLI entrypoint at all yet — this" \
         "is a genuine harness-wiring gap (not a missing host), tracked in" \
         "docs/runbooks/pipeline-evals.md#p14. Running the CI-runnable scripted tier as the" \
         "closest available proxy." >&2
fi

echo "Running: cargo test -p haily-core --test automation_goldens"
cargo test -p haily-core --test automation_goldens
