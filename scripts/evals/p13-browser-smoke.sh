#!/bin/sh
# P13 — Stealth browser live smoke (Sub-Agent + Skill Architecture, Phase 13).
#
# Per crates/haily-tools/src/browser/manager.rs's Decision 7 (locked): the live CDP smoke
# (render a JS page, verify stealth against a fingerprint test page, with and without
# CloakBrowser) is a MANUAL step — the driver is verified only to compile under the `browser`
# feature. This script does the two things that ARE automatable — (1) prereq-check for a
# drivable browser binary, (2) run the existing `browser`-feature test suite (unit + wiring
# tests, zero-network, already exist) — then hands off to the runbook for the manual live steps.
# No new smoke harness is added here (that would be new mechanism, out of scope for this phase).
#
# Usage: sh scripts/evals/p13-browser-smoke.sh
# Env:   HAILY_BROWSER_PATH  (explicit browser binary override)
#        HAILY_CDP_URL       (remote/external CDP endpoint — counts as "present")
set -eu
cd "$(dirname "$0")"
. ./lib.sh
cd_repo_root

if ! browser_binary_present; then
    skip "No drivable browser found (checked HAILY_CDP_URL, HAILY_BROWSER_PATH, then the \
CloakBrowser/Chrome/Chromium candidate paths crates/haily-tools/src/browser/stealth.rs uses). \
Install Chrome/Chromium (or CloakBrowser), or set HAILY_BROWSER_PATH / HAILY_CDP_URL, then \
re-run this script. See docs/runbooks/pipeline-evals.md#p13."
fi

echo "Browser prerequisite satisfied — running the browser-feature test suite" \
     "(compiles the live CDP driver, exercises the zero-network stealth/risk-tier logic)…"
cargo test -p haily-tools --features browser

cat <<'EOF'

The automatable part is done. The LIVE CDP smoke itself (render a real page, compare fingerprint
output with and without CloakBrowser) has no code-level entrypoint — it is a locked manual step
(see crates/haily-tools/src/browser/manager.rs's Decision 7 doc comment). Follow
docs/runbooks/pipeline-evals.md#p13 for the manual walk-through: launch a coding/automation run
with a browser_navigate/browser_interact step, or drive the tools directly via a REPL session,
and compare against a fingerprint test page (e.g. https://bot.sannysoft.com or similar) with and
without HAILY_BROWSER_PATH pointed at CloakBrowser.
EOF
