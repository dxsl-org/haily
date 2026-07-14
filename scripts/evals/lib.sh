#!/bin/sh
# Shared helpers for scripts/evals/*.sh (Pipeline Activation & Wiring, Phase 7).
#
# Contract every wrapper follows: check a real prerequisite (model host / Chromium / Zed
# reachable), then either invoke the existing harness verbatim (no re-implemented scoring) or
# print an EXPLICIT skip line and exit non-zero-but-distinguishable. Never a silent skip, never a
# misleading pass. POSIX sh only (Git Bash on Windows) — no bashisms.

# Distinguishes "prerequisite absent, intentionally not run" from a real failure (exit 1+) or
# success (exit 0). 77 mirrors the autotools/automake SKIP convention.
SKIP_EXIT_CODE=77

# Move the caller to the repo root so every relative path (evals/fixtures, cargo -p, …) resolves
# the same way regardless of the caller's cwd.
cd_repo_root() {
    root="$(git rev-parse --show-toplevel 2>/dev/null)"
    if [ -z "$root" ]; then
        echo "evals: not inside a git checkout — cannot locate the repo root" >&2
        exit 1
    fi
    cd "$root" || exit 1
}

# Print a clear, unambiguous skip message (never truncated, never swallowed) and exit
# $SKIP_EXIT_CODE. Every wrapper calls this instead of silently returning 0 when its
# prerequisite is absent.
skip() {
    echo "SKIP: $1" >&2
    exit "$SKIP_EXIT_CODE"
}

# Best-effort check for a browser binary the P13 stealth tool can drive, mirroring the discovery
# order in crates/haily-tools/src/browser/stealth.rs::find_browser_binary (env override, then
# CloakBrowser, then Chrome/Chromium). This is a PREREQ CHECK only — no harness logic duplicated.
browser_binary_present() {
    if [ -n "${HAILY_CDP_URL:-}" ]; then
        return 0
    fi
    if [ -n "${HAILY_BROWSER_PATH:-}" ] && [ -e "${HAILY_BROWSER_PATH}" ]; then
        return 0
    fi
    for candidate in \
        "/c/Program Files/CloakBrowser/Application/cloakbrowser.exe" \
        "/c/Program Files/Google/Chrome/Application/chrome.exe" \
        "/c/Program Files (x86)/Google/Chrome/Application/chrome.exe" \
        "/Applications/CloakBrowser.app/Contents/MacOS/CloakBrowser" \
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
        "/Applications/Chromium.app/Contents/MacOS/Chromium" \
        "/usr/bin/cloakbrowser" \
        "/usr/bin/google-chrome-stable" \
        "/usr/bin/google-chrome" \
        "/usr/bin/chromium-browser" \
        "/usr/bin/chromium"; do
        [ -e "$candidate" ] && return 0
    done
    for cmd in google-chrome google-chrome-stable chromium chromium-browser; do
        command -v "$cmd" >/dev/null 2>&1 && return 0
    done
    return 1
}

# cargo run wrapper that only appends --features when HAILY_CARGO_FEATURES is set, so a caller
# needing no llama/GPU feature (e.g. a cloud-model HAILY_EVAL_MODEL) is not forced into one.
run_haily_cli() {
    if [ -n "${HAILY_CARGO_FEATURES:-}" ]; then
        cargo run -p haily-cli --bin haily-cli --features "$HAILY_CARGO_FEATURES" -- "$@"
    else
        cargo run -p haily-cli --bin haily-cli -- "$@"
    fi
}
