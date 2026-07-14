#!/bin/sh
# P9 — Golden coding baseline MATRIX (Sub-Agent + Skill Architecture, Phase 9).
#
# The CI-runnable, zero-network tier of P9 (scripted-LLM goldens) already runs in
# `cargo test --workspace` via crates/haily-core/tests/coding_goldens.rs — this script is ONLY
# the model-host-gated baseline matrix (local x {Normal, Deep} x escalation {off, on}) driven by
# `haily eval coding` (crates/haily-app/src/eval.rs), a thin wrapper, no new scoring logic here.
#
# Usage: sh scripts/evals/p9-coding-matrix.sh
# Env:   HAILY_EVAL_MODEL      (required — model name recorded on every eval_runs row)
#        HAILY_EVAL_DEPTH      (default: normal; also try "deep" for the matrix' other arm)
#        HAILY_EVAL_ESCALATE   ("1" enables the P3 tier-escalation matrix arm; default off)
#        HAILY_EVAL_FIXTURES   (default: evals/fixtures)
#        HAILY_CARGO_FEATURES  (e.g. "llama" for a local GGUF host; unset is fine for a cloud model)
set -eu
cd "$(dirname "$0")"
. ./lib.sh
cd_repo_root

if [ -z "${HAILY_EVAL_MODEL:-}" ]; then
    skip "HAILY_EVAL_MODEL is not set. The P9 baseline matrix needs a configured local/cloud \
model host, which this environment does not provide. Set HAILY_EVAL_MODEL=<model-name> and \
configure the LLM router (llm.llama_model_path for local GGUF, or llm.cloud_base_url/cloud_model \
+ a provider key for cloud), then re-run this script. The CI-runnable scripted-LLM goldens still \
run offline via: cargo test -p haily-core --test coding_goldens. See \
docs/runbooks/pipeline-evals.md#p9 for the full matrix protocol."
fi

depth="${HAILY_EVAL_DEPTH:-normal}"
fixtures="${HAILY_EVAL_FIXTURES:-evals/fixtures}"

set -- eval coding --depth "$depth" --fixtures "$fixtures"
if [ "${HAILY_EVAL_ESCALATE:-0}" = "1" ]; then
    set -- "$@" --escalate
fi

echo "Running: haily eval coding --depth $depth --fixtures $fixtures" \
     "$([ "${HAILY_EVAL_ESCALATE:-0}" = "1" ] && echo "--escalate")"
run_haily_cli "$@"
