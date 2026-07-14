#!/bin/sh
# P0 — Capability + undo spike (Sub-Agent + Skill Architecture, Phase 0).
#
# Two tracks (see evals/spike/README.md for the full Go/No-Go rule):
#   Track B-undo        — scriptable, no model needed. Always runs.
#   Track B-capability   — needs a configured local/cloud model host. The README's original
#                          "manual protocol" predates the plan->build->verify pipeline this repo
#                          later built (P4-P6); this wrapper reuses that NOW-EXISTING P9 harness
#                          (`haily eval coding`) pointed at the original P0 spike fixtures
#                          (evals/spike/fixtures/) instead of re-deriving a separate runner.
#
# Usage: sh scripts/evals/p0-capability-spike.sh
# Env:   HAILY_EVAL_MODEL      (required for the capability track — model name recorded on the run)
#        HAILY_EVAL_DEPTH      (default: normal)
#        HAILY_CARGO_FEATURES  (e.g. "llama" for a local GGUF host; unset is fine for a cloud model)
set -eu
cd "$(dirname "$0")"
. ./lib.sh
cd_repo_root

echo "== P0 Track B-undo (scriptable, no model) =="
if ! sh evals/spike/undo/run_undo_spike.sh; then
    echo "P0: undo spike FAILED — this is a NO-GO / redesign signal per evals/spike/README.md" \
         "(Track A/undo must be fixed before any P1 exec ships)." >&2
    exit 1
fi

echo
echo "== P0 Track B-capability (requires a configured local/cloud model host) =="
if [ -z "${HAILY_EVAL_MODEL:-}" ]; then
    skip "HAILY_EVAL_MODEL is not set. Track B-capability needs a configured local/cloud model \
(same prerequisite as P9's matrix — see docs/runbooks/pipeline-evals.md#p9). Set \
HAILY_EVAL_MODEL=<model-name>, configure the LLM router (llm.llama_model_path preference for a \
local GGUF host, or llm.cloud_base_url/cloud_model + a provider key for a cloud host), then \
re-run this script. The undo track above already ran and its result stands regardless."
fi

echo "Running the P9 harness against the ORIGINAL P0 spike fixtures (evals/spike/fixtures/) —" \
     "reuses evals/fixtures' harness, not a re-implementation."
run_haily_cli eval coding --depth "${HAILY_EVAL_DEPTH:-normal}" --fixtures evals/spike/fixtures

echo
echo "P0: read the Go/No-Go bar in evals/spike/README.md against the per-fixture pass/fail and" \
     "tool-call counts in the report just written under .agents/reports/ (see the runbook for" \
     "how to interpret a 'local model fails the bar' result — that means 'needs a cloud tier'," \
     "not a bug)."
