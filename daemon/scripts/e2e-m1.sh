#!/usr/bin/env bash
#
# M1 end-to-end (plan §5 M1 acceptance criteria), driving the REAL `modelstat`
# binary through the five scenarios:
#
#   1. fresh register        2. credential reuse     3. `--fresh` convergence
#   4. 401-revocation recovery                       5. prod-guard exit 2
#
# Server: if $DAEMON_API_URL is set, it runs against that (e.g. the core dev
# server from `docker compose -f core/rust/dev/docker-compose.yml up -d`).
# Otherwise it spawns the bundled fake device-API server (no Docker needed) —
# the same server the `e2e_m1` cargo integration test uses. Scenario 4 needs the
# fake server's revoke/claim control endpoints and is skipped against a real one.
#
# Everything runs under a throwaway MODELSTAT_HOME, so the real ~/.modelstat is
# never touched.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DAEMON_DIR="$(cd "$HERE/.." && pwd)"
cd "$DAEMON_DIR"

FAKE_PORT="${MODELSTAT_FAKE_PORT:-47591}"
USE_FAKE=0
SERVER_PID=""
HOME_TMP="$(mktemp -d)"

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$HOME_TMP"
}
trap cleanup EXIT

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; exit 1; }
strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }
device_id_of() { grep -o 'device_id=[^ ]*' | head -1 | cut -d= -f2; }

echo "building modelstat…"
cargo build -q -p modelstat-cli
BIN="$DAEMON_DIR/target/debug/modelstat"

if [ -n "${DAEMON_API_URL:-}" ]; then
  API="$DAEMON_API_URL"
  echo "using real server: $API"
else
  API="http://127.0.0.1:$FAKE_PORT"
  USE_FAKE=1
  echo "building + starting fake device-API server on $API…"
  cargo build -q -p modelstat-ingest --example fake_device_server
  "$DAEMON_DIR/target/debug/examples/fake_device_server" "127.0.0.1:$FAKE_PORT" &
  SERVER_PID=$!
  disown "$SERVER_PID" 2>/dev/null || true  # suppress the job-control "Terminated" notice on cleanup
  ready=0
  for _ in $(seq 1 50); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$FAKE_PORT") 2>/dev/null; then
      exec 3>&- 3<&-
      ready=1
      break
    fi
    sleep 0.1
  done
  [ "$ready" = 1 ] || fail "fake server did not come up on $API"
fi

export DAEMON_API_URL="$API"
export MODELSTAT_HOME="$HOME_TMP"
# NOTE: dedupe is on fingerprint.machine_id (this machine's hardware key), which
# no env var overrides — so against a *real* dev server this converges onto this
# machine's dev device row idempotently (a fresh secret, no duplicate). The
# throwaway MODELSTAT_HOME keeps the real ~/.modelstat untouched; the default
# fake server is fully isolated.

echo
echo "── 1. fresh register ──────────────────────────────────────────────"
OUT1="$("$BIN" self-register 2>&1 | strip_ansi)" || fail "self-register exited non-zero"
echo "$OUT1" | sed 's/^/    /'
if ! echo "$OUT1" | grep -q 'device_id='; then fail "no device_id in output"; fi
D1="$(echo "$OUT1" | device_id_of)"
if [ ! -f "$HOME_TMP/identity.json" ]; then fail "identity.json not written"; fi
pass "registered device_id=$D1, identity.json written"

echo
echo "── 2. credential reuse ────────────────────────────────────────────"
OUT2="$("$BIN" self-register 2>&1 | strip_ansi)" || fail "self-register (reuse) exited non-zero"
D2="$(echo "$OUT2" | device_id_of)"
if [ "$D2" != "$D1" ]; then fail "reuse changed device_id ($D1 → $D2)"; fi
if ! echo "$OUT2" | grep -q 're-registered'; then fail "reuse did not report re-registered"; fi
pass "reuse converged onto the same device $D2 (re-registered)"

echo
echo "── 3. --fresh convergence ─────────────────────────────────────────"
# What `connect --fresh` does (M6): back up + wipe identity, then re-derive.
cp "$HOME_TMP/identity.json" "$HOME_TMP/identity.json.bak-e2e"
rm -f "$HOME_TMP/identity.json"
OUT3="$("$BIN" self-register 2>&1 | strip_ansi)" || fail "self-register (--fresh) exited non-zero"
D3="$(echo "$OUT3" | device_id_of)"
if [ "$D3" != "$D1" ]; then fail "--fresh converged onto a different device ($D1 → $D3)"; fi
pass "--fresh re-derived the same uuid and converged onto $D3"

echo
echo "── 4. 401-revocation recovery ─────────────────────────────────────"
if [ "$USE_FAKE" = 1 ]; then
  curl -sf -X POST "$API/_control/revoke" -H 'content-type: application/json' \
    -d "{\"device_id\":\"$D1\"}" >/dev/null
  curl -sf -X POST "$API/_control/claim" -H 'content-type: application/json' \
    -d "{\"device_id\":\"$D1\",\"user_id\":\"user_e2e\"}" >/dev/null
  # The stored bearer is now dead. await-claim polls /devices/me → 401 →
  # machine-stable re-register (same row, fresh bearer) → sees claimed → exits 0.
  OUT4="$("$BIN" await-claim 2>&1 | strip_ansi)" || fail "await-claim exited non-zero"
  if ! echo "$OUT4" | grep -q 'claimed by user_id=user_e2e'; then
    fail "await-claim did not recover + complete: $OUT4"
  fi
  pass "recovered from a revoked bearer and completed the claim"
else
  echo "  SKIP 401 recovery (needs the fake server's control endpoints)"
fi

echo
echo "── 5. prod-guard exit 2 ───────────────────────────────────────────"
# Fresh home (no identity ⇒ derived uuid) + CI + prod default (no DAEMON_API_URL)
# ⇒ the guard must refuse with exit 2 BEFORE any network call.
HOME5="$(mktemp -d)"
set +e
env -u DAEMON_API_URL CI=1 MODELSTAT_HOME="$HOME5" MODELSTAT_ALLOW_PROD_REGISTER= \
  "$BIN" self-register </dev/null >/dev/null 2>&1
code=$?
set -e
rm -rf "$HOME5"
if [ "$code" != 2 ]; then fail "prod guard expected exit 2, got $code"; fi
pass "fresh prod register from CI refused with exit 2"

echo
printf '\033[32mALL M1 SCENARIOS PASSED\033[0m\n'
