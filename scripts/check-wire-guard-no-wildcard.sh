#!/usr/bin/env bash
# C3's compile-time trip-wire (crates/haily-io/tests/mobile_server/wire_forward_compat_guard.rs)
# only works because its match arms over ServerBody/ClientFrame have NO wildcard — Rust refuses
# to compile a non-exhaustive match, so a future variant forces a conscious edit here. A `_ => {}`
# arm silences that compile error and defeats the trip-wire without anyone noticing (review MED,
# P6). This script is the static backstop: it fails if a wildcard match arm ever appears in that
# one file.
set -euo pipefail

FILE="crates/haily-io/tests/mobile_server/wire_forward_compat_guard.rs"

if [ ! -f "$FILE" ]; then
  echo "check-wire-guard-no-wildcard: '$FILE' not found — nothing to check" >&2
  exit 0
fi

# Matches `_ =>` and `.. =>` used as a match arm pattern (allowing surrounding whitespace).
wildcard_hits=$(grep -nE '(^|[^[:alnum:]_])(_|\.\.)[[:space:]]*=>' "$FILE" || true)

if [ -n "$wildcard_hits" ]; then
  echo "check-wire-guard-no-wildcard: FAILED — a wildcard match arm was added to $FILE." >&2
  echo "That silently defeats C3's compile-time trip-wire for new ServerBody/ClientFrame variants." >&2
  echo "Enumerate every variant explicitly instead." >&2
  echo "$wildcard_hits"
  exit 1
fi

echo "check-wire-guard-no-wildcard: OK — no wildcard arm in $FILE"
