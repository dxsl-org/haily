#!/bin/sh
# P12 — Live ACP/Zed interop (Sub-Agent + Skill Architecture, Phase 12).
#
# The ACP channel itself is unit-tested in-crate (crates/haily-io/src/acp/tests.rs — stdout-frame
# discipline, the request_permission<->ApprovalGate bridge, session/load replay) and already runs
# in `cargo test --workspace`. What remains host-gated is a REAL ACP-capable editor (Zed) driving
# the process live — there is no code-level entrypoint for "an editor talks to us" beyond spawning
# the binary, so this script's job is: confirm the binary builds, confirm an editor is present,
# and print the exact config an operator pastes into Zed. The live drive itself stays a documented
# manual step (docs/runbooks/pipeline-evals.md#p12) — not something this script can automate
# without Zed itself as the driver.
#
# Usage: sh scripts/evals/p12-zed-interop.sh
# Env:   HAILY_ZED_BIN  (override the "zed" binary name/path used for the presence check)
set -eu
cd "$(dirname "$0")"
. ./lib.sh
cd_repo_root

zed_bin="${HAILY_ZED_BIN:-zed}"
if ! command -v "$zed_bin" >/dev/null 2>&1; then
    skip "No ACP-capable editor found on PATH (looked for '$zed_bin'). Install Zed \
(https://zed.dev) or set HAILY_ZED_BIN to its binary path, then re-run this script. See \
docs/runbooks/pipeline-evals.md#p12 for the manual configuration steps."
fi

echo "Zed found at: $(command -v "$zed_bin")"
echo "Building the ACP binary (cargo build -p haily-cli --bin haily-cli)…"
cargo build -p haily-cli --bin haily-cli

bin_path="$(pwd)/target/debug/haily-cli"
[ "${OS:-}" = "Windows_NT" ] && bin_path="${bin_path}.exe"

cat <<EOF

Built: $bin_path

This is as far as a script can drive P12 — Zed itself must spawn and speak to the process; there
is no code-level "run the interop" command (the ACP channel's live half genuinely has no
automatable entrypoint, unlike P9/P14 which reuse a real CLI harness). Add a custom agent server
in Zed's settings.json pointing at the binary above, e.g.:

  {
    "agent_servers": {
      "Haily": {
        "command": "$bin_path",
        "args": ["acp"]
      }
    }
  }

Then open Zed's agent panel, select "Haily", and drive a coding run through the editor. Expect:
RunEvent-backed edit previews streaming in, an approval prompt on any write-tier step, and the
transcript replay on session/load. See docs/runbooks/pipeline-evals.md#p12 for what to check at
each step. This manual-drive requirement is a known follow-up, not a bug in this script.
EOF
