#!/usr/bin/env bash
# Odoo CI bootstrap for the Safe Operator Harness golden tests (phase 5).
#
# Runs AFTER `docker compose -f docker/odoo-ci-compose.yml up -d`. It:
#   1. Polls an AUTHENTICATED RPC readiness probe — a `common.authenticate` that SUCCEEDS.
#      `pg_isready` (the compose healthcheck) only proves Postgres is up; it says NOTHING
#      about whether Odoo has finished installing base/contacts/crm and can serve `object`
#      calls. We poll a real login so the tests never race a half-initialized server.
#   2. Creates a scoped, non-admin service user (never uses the admin password for the tests).
#   3. Generates ONE API key for that user via `res.users.apikeys.generate()` (returned once).
#   4. Masks the key in CI logs (::add-mask::) and NEVER echoes the login/generate response.
#   5. Exports HAILY_ODOO_URL + HAILY_ODOO_API_KEY (+ db/uid) to the GitHub Actions env so the
#      env-gated golden tests pick them up. With HAILY_ODOO_URL unset the tests SKIP.
#
# Runnable on Git-Bash (Windows) and Linux (CI). Requires: curl, python3 (for JSON parsing —
# avoids a jq dependency on minimal runners).
set -euo pipefail

ODOO_URL="${ODOO_URL:-http://127.0.0.1:8069}"
ODOO_DB="${ODOO_DB:-haily_ci}"
ADMIN_LOGIN="${ODOO_ADMIN_LOGIN:-admin}"
# The default is the throwaway sandbox constant "admin" (a fresh, ephemeral CI DB — not a
# real credential). If an operator overrides it with a REAL password, mask it from CI logs
# defensively so a downstream echo/set -x can never leak it. The default is left unmasked so
# the readiness-probe logs stay legible.
ADMIN_PASSWORD="${ODOO_ADMIN_PASSWORD:-admin}"
if [ "${ADMIN_PASSWORD}" != "admin" ]; then
  echo "::add-mask::${ADMIN_PASSWORD}"
fi
SERVICE_LOGIN="haily-ci-service"
# The service user's password is only used to authenticate the one apikey generate call; the
# tests themselves authenticate with the generated API key, never this password.
SERVICE_PASSWORD="$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 24)"

# POST a JSON-RPC call to $1 (service), $2 (method), $3 (args JSON array). Prints the raw
# response body on stdout. Callers that receive a secret MUST NOT echo the body.
rpc() {
  local service="$1" method="$2" args="$3"
  curl -sS --max-time 15 -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"call\",\"params\":{\"service\":\"${service}\",\"method\":\"${method}\",\"args\":${args}}}" \
    "${ODOO_URL}/jsonrpc"
}

# --- 1. Authenticated-RPC readiness probe ------------------------------------------------
# Poll `common.authenticate(db, admin, pw, {})` until it returns a numeric uid (success). A
# transport error or a non-numeric result means Odoo is not ready yet.
echo "Waiting for Odoo authenticated-RPC readiness at ${ODOO_URL} ..."
ADMIN_UID=""
for attempt in $(seq 1 60); do
  # NOTE: the response may contain session detail — parse it, never print it.
  resp="$(rpc common authenticate "[\"${ODOO_DB}\",\"${ADMIN_LOGIN}\",\"${ADMIN_PASSWORD}\",{}]" || true)"
  ADMIN_UID="$(printf '%s' "$resp" | python3 -c 'import sys,json;
try:
    r=json.load(sys.stdin).get("result")
    print(r if isinstance(r,int) else "")
except Exception:
    print("")' 2>/dev/null || true)"
  if [ -n "$ADMIN_UID" ]; then
    echo "Odoo ready (authenticated as uid=${ADMIN_UID}) after ${attempt} attempt(s)."
    break
  fi
  sleep 3
done
if [ -z "$ADMIN_UID" ]; then
  echo "ERROR: Odoo did not become RPC-ready in time." >&2
  exit 1
fi

# execute_kw as admin: object.execute_kw(db, uid, pw, model, method, args[, kwargs]).
admin_kw() {
  local model="$1" method="$2" args="$3" kwargs="${4:-{}}"
  rpc object execute_kw \
    "[\"${ODOO_DB}\",${ADMIN_UID},\"${ADMIN_PASSWORD}\",\"${model}\",\"${method}\",${args},${kwargs}]"
}

# --- 2. Create (or find) the scoped service user -----------------------------------------
# Idempotent: reuse the user if a prior run created it. Group membership is deliberately
# minimal (base.group_user + sales) — NOT an admin. Sets a password so apikeys.generate can
# authenticate as this user below.
echo "Provisioning scoped service user '${SERVICE_LOGIN}' ..."
existing="$(admin_kw res.users search "[[[\"login\",\"=\",\"${SERVICE_LOGIN}\"]]]" || true)"
SERVICE_UID="$(printf '%s' "$existing" | python3 -c 'import sys,json;
try:
    r=json.load(sys.stdin).get("result") or []
    print(r[0] if r else "")
except Exception:
    print("")' 2>/dev/null || true)"

if [ -z "$SERVICE_UID" ]; then
  create_resp="$(admin_kw res.users create \
    "[{\"name\":\"Haily CI Service\",\"login\":\"${SERVICE_LOGIN}\",\"password\":\"${SERVICE_PASSWORD}\"}]" || true)"
  SERVICE_UID="$(printf '%s' "$create_resp" | python3 -c 'import sys,json;
try:
    print(json.load(sys.stdin).get("result",""))
except Exception:
    print("")' 2>/dev/null || true)"
else
  # Ensure a known password on the reused account so the generate-key login below works.
  admin_kw res.users write "[[${SERVICE_UID}],{\"password\":\"${SERVICE_PASSWORD}\"}]" >/dev/null || true
fi
if [ -z "$SERVICE_UID" ]; then
  echo "ERROR: failed to provision the service user." >&2
  exit 1
fi
echo "Service user uid=${SERVICE_UID}."

# --- 3. Generate an API key for the service user -----------------------------------------
# res.users.apikeys.generate(scope, name, expiration_date) runs in the CONTEXT of the calling
# user, so we authenticate AS the service user (its own uid + password) to mint ITS key. The
# key is returned exactly once — capture it silently.
SERVICE_UID_AUTH="$(rpc common authenticate "[\"${ODOO_DB}\",\"${SERVICE_LOGIN}\",\"${SERVICE_PASSWORD}\",{}]" \
  | python3 -c 'import sys,json;
try:
    r=json.load(sys.stdin).get("result")
    print(r if isinstance(r,int) else "")
except Exception:
    print("")' 2>/dev/null || true)"
if [ -z "$SERVICE_UID_AUTH" ]; then
  echo "ERROR: service user could not authenticate for key generation." >&2
  exit 1
fi

gen_resp="$(rpc object execute_kw \
  "[\"${ODOO_DB}\",${SERVICE_UID_AUTH},\"${SERVICE_PASSWORD}\",\"res.users.apikeys\",\"generate\",[\"rpc\",\"haily-ci\",false]]" || true)"
API_KEY="$(printf '%s' "$gen_resp" | python3 -c 'import sys,json;
try:
    print(json.load(sys.stdin).get("result",""))
except Exception:
    print("")' 2>/dev/null || true)"
# NEVER print gen_resp — it contains the key. Fail if empty.
if [ -z "$API_KEY" ]; then
  echo "ERROR: API key generation returned no key." >&2
  exit 1
fi

# --- 4. Mask + 5. export to the test env -------------------------------------------------
# ::add-mask:: scrubs the key from all subsequent CI log output (m2). Emit it BEFORE any
# further use so an accidental echo downstream is still redacted.
echo "::add-mask::${API_KEY}"
echo "Generated scoped API key for '${SERVICE_LOGIN}' (masked)."

if [ -n "${GITHUB_ENV:-}" ]; then
  {
    echo "HAILY_ODOO_URL=${ODOO_URL}"
    echo "HAILY_ODOO_DB=${ODOO_DB}"
    echo "HAILY_ODOO_UID=${SERVICE_UID_AUTH}"
    echo "HAILY_ODOO_API_KEY=${API_KEY}"
  } >> "$GITHUB_ENV"
  echo "Exported HAILY_ODOO_URL/DB/UID/API_KEY to the CI env."
else
  # Local run: print export lines the caller can `eval`. The key line is masked above, but
  # for a local (non-CI) run the operator explicitly opts into seeing it.
  echo "# eval the following to run the golden tests locally:"
  echo "export HAILY_ODOO_URL=${ODOO_URL}"
  echo "export HAILY_ODOO_DB=${ODOO_DB}"
  echo "export HAILY_ODOO_UID=${SERVICE_UID_AUTH}"
  echo "export HAILY_ODOO_API_KEY=<generated-key-not-shown-in-ci>"
fi
