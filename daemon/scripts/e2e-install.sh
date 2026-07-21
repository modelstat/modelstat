#!/usr/bin/env bash
# CI installer-logic lane (plan §5 M7). Drives scripts/install.sh through its
# offline, service-free path against a LOCAL `file://` archive and asserts the
# security- and correctness-critical logic:
#   1. sha256 verify + stage BOTH binaries,
#   2. the staged bin dir is actually reachable by NAME afterwards (PATH, §3.3),
#   3. idempotent re-run,
#   4. a tampered archive is REFUSED (no partial install).
#
# Fully hermetic — the MODELSTAT_INSTALL_* hooks keep it offline and touch no
# services, and every write is confined to a throwaway MODELSTAT_HOME **and a
# throwaway HOME** (the PATH step writes shell startup files, which must never be
# the runner's own). The live curl|sh + real-service path is validated separately
# on a real machine (M7 AC), because service start/stop isn't reliable on CI
# runners.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DAEMON_DIR="$(cd "$HERE/.." && pwd)"
cd "$DAEMON_DIR"

VER="9.9.9-test"
pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; exit 1; }

# Mirror install.sh's uname→triple logic so the archive name matches.
case "$(uname -s)" in
  Darwin) OS=apple-darwin ;;
  Linux)  OS=unknown-linux-gnu ;;
  *) fail "this logic lane runs on macOS/Linux (Windows uses install.ps1)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  arm64|aarch64) ARCH=aarch64 ;;
  *) fail "unsupported arch $(uname -m)" ;;
esac
ARCHIVE="modelstat-${VER}-${ARCH}-${OS}.tar.gz"

echo "building the collector…"
cargo build -q -p modelstat-cli
COLLECTOR="target/debug/modelstat"
[ -x "$COLLECTOR" ] || fail "collector binary not built at $COLLECTOR"

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
REL="$WORK/release"; STAGE="$WORK/stage"; mkdir -p "$REL" "$STAGE"

# A real collector (install.sh runs `modelstat _setup-runtime`) + a stand-in
# engine (only copied by the stage step, never executed by it).
cp "$COLLECTOR" "$STAGE/modelstat"
printf '#!/bin/sh\necho "modelstat-summarizer %s"\n' "$VER" > "$STAGE/modelstat-summarizer"
chmod +x "$STAGE/modelstat" "$STAGE/modelstat-summarizer"
tar -czf "$REL/$ARCHIVE" -C "$STAGE" .

# SHA256SUMS in the exact `<sum>␣␣<name>` shape install.sh greps for.
( cd "$REL" && { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$ARCHIVE"; else shasum -a 256 "$ARCHIVE"; fi; } > SHA256SUMS )

run_install() { # $1 = throwaway MODELSTAT_HOME, $2 = throwaway HOME
  mkdir -p "$2"
  # SHELL is pinned so the PATH step picks the same startup file on macOS and
  # Linux (bash differs per OS by design — see path_env::rc_path).
  MODELSTAT_HOME="$1" \
  HOME="$2" \
  SHELL=/bin/zsh \
  MODELSTAT_INSTALL_BASE_URL="file://$REL" \
  MODELSTAT_INSTALL_STAGE_ONLY=1 \
    sh scripts/install.sh --version "$VER" --no-auto-update --yes --no-browser
}

# 1. Happy path — verify sha256 + stage BOTH binaries.
H1="$WORK/home1"; U1="$WORK/user1"
run_install "$H1" "$U1" >/dev/null 2>&1 || fail "install (happy path) exited non-zero"
[ -f "$H1/bin/modelstat" ] || fail "collector was not staged"
[ -f "$H1/bin/modelstat-summarizer" ] || fail "engine was not staged"
pass "verified sha256 + staged both binaries"

# 2. PATH (§3.3) — staging is only half an install. The startup file must source
# our snippet, and sourcing it must make `modelstat` resolvable BY NAME; a user
# who has to type the full path was never really installed.
[ -f "$U1/.zshrc" ] || fail "no shell startup file was written"
grep -q "$H1/env" "$U1/.zshrc" || fail ".zshrc does not source the modelstat env snippet"
[ -f "$H1/env" ] || fail "the env snippet was not written"
RESOLVED="$(env -i HOME="$U1" PATH=/usr/bin:/bin sh -c '. "$1/env"; command -v modelstat' _ "$H1" || true)"
[ "$RESOLVED" = "$H1/bin/modelstat" ] \
  || fail "sourcing the env snippet did not put the staged binary on PATH (got '${RESOLVED:-nothing}')"
pass "wired PATH — \`modelstat\` resolves to the staged binary"

# 3. Idempotent re-run — installing again over the same home still succeeds, and
# does not stack a second copy of our block in the startup file.
run_install "$H1" "$U1" >/dev/null 2>&1 || fail "idempotent re-run exited non-zero"
[ -f "$H1/bin/modelstat" ] || fail "collector missing after re-run"
BLOCKS="$(grep -c 'puts the modelstat CLI on your PATH' "$U1/.zshrc")"
[ "$BLOCKS" = "1" ] || fail "re-run duplicated the PATH block in .zshrc (found $BLOCKS)"
pass "idempotent re-run (one PATH block, not two)"

# 4. Tamper — a corrupted archive MUST be refused, leaving nothing staged.
printf 'corrupted' >> "$REL/$ARCHIVE" # sha no longer matches SHA256SUMS
H2="$WORK/home2"; U2="$WORK/user2"
if run_install "$H2" "$U2" >/dev/null 2>&1; then
  fail "installer accepted a tampered archive (sha mismatch not caught!)"
fi
[ ! -f "$H2/bin/modelstat" ] || fail "tampered install left a staged binary"
[ ! -f "$U2/.zshrc" ] || fail "tampered install still touched the shell startup file"
pass "rejected a tampered archive (checksum mismatch → no install)"

echo "  all installer-logic checks passed"
