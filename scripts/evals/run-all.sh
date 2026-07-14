#!/bin/sh
# Run every deferred host-gated eval wrapper (Pipeline Activation & Wiring, Phase 7), skipping
# whichever ones this host doesn't support. Never aborts early on a skip or failure — it always
# attempts all five and prints one summary line per script plus a final tally, so a partial host
# (e.g. Zed installed but no local model) still gets everything it CAN run.
#
# Usage: sh scripts/evals/run-all.sh
set -u
cd "$(dirname "$0")"
. ./lib.sh
cd_repo_root

scripts="p0-capability-spike.sh p9-coding-matrix.sh p12-zed-interop.sh p13-browser-smoke.sh p14-automation-matrix.sh"

ran=0
skipped=0
failed=0

for s in $scripts; do
    echo
    echo "############################################################"
    echo "# $s"
    echo "############################################################"
    sh "scripts/evals/$s"
    status=$?
    if [ "$status" -eq 0 ]; then
        echo "RESULT: $s -> ran (exit 0)"
        ran=$((ran + 1))
    elif [ "$status" -eq "$SKIP_EXIT_CODE" ]; then
        echo "RESULT: $s -> SKIPPED (prerequisite absent — see message above)"
        skipped=$((skipped + 1))
    else
        echo "RESULT: $s -> FAILED (exit $status)"
        failed=$((failed + 1))
    fi
done

echo
echo "== Summary: $ran ran, $skipped skipped, $failed failed (of 5) =="
[ "$failed" -eq 0 ]
