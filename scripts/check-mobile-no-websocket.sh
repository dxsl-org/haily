#!/usr/bin/env bash
# Mobile Thin-Client plan phase 3 (M14): the mobile WebView must be structurally incapable of
# opening its own network socket — the restrictive CSP in `src-tauri-mobile/tauri.conf.json`
# (`connect-src 'self' ipc: http://ipc.localhost`, no ws:/wss:/remote http) is the runtime
# enforcement; this script is the static one, catching a future dependency/component that
# tries to open a raw socket from JS under the mobile route group before it ever ships.
set -euo pipefail

SCOPE="src/routes/mobile"

if [ ! -d "$SCOPE" ]; then
  echo "check-mobile-no-websocket: '$SCOPE' does not exist — nothing to scan" >&2
  exit 0
fi

# `new WebSocket(` — the only way plain JS/TS opens a client socket.
websocket_hits=$(grep -rnE "new[[:space:]]+WebSocket[[:space:]]*\(" "$SCOPE" || true)
# A remote-origin fetch/EventSource under the mobile route would be an equally-real leak of the
# "WebView can't reach the network directly" invariant, even without a raw WebSocket. Quote
# alternation includes backtick so a template-literal URL (`fetch(\`https://...\`)`) is caught
# too, not just single/double-quoted string literals.
remote_fetch_hits=$(grep -rnE "(fetch|EventSource)\s*\(\s*[\"'\`]https?://" "$SCOPE" || true)
# `XMLHttpRequest` sets its target URL via a separate `.open(method, url)` call, not the
# constructor, so it can't be caught by the same "literal starts with http(s)" pattern above —
# any construction at all under this scope is suspicious enough to flag on its own.
xhr_hits=$(grep -rnE "new[[:space:]]+XMLHttpRequest[[:space:]]*\(" "$SCOPE" || true)

if [ -n "$websocket_hits" ] || [ -n "$remote_fetch_hits" ] || [ -n "$xhr_hits" ]; then
  echo "check-mobile-no-websocket: FAILED — the mobile WebView must never open its own socket (M14)." >&2
  echo "The Rust core (haily-mobile-client) owns the WS connection; Svelte only talks to it over Tauri IPC." >&2
  [ -n "$websocket_hits" ] && { echo "-- new WebSocket(...) matches:"; echo "$websocket_hits"; }
  [ -n "$remote_fetch_hits" ] && { echo "-- remote fetch/EventSource matches:"; echo "$remote_fetch_hits"; }
  [ -n "$xhr_hits" ] && { echo "-- new XMLHttpRequest(...) matches:"; echo "$xhr_hits"; }
  exit 1
fi

echo "check-mobile-no-websocket: OK — no WebSocket/remote-fetch usage found under $SCOPE"
