#!/usr/bin/env sh
# modelstat installer — one command sets up everything (macOS + Linux).
#
#   curl -fsSL https://install.modelstat.ai | sh
#
# ─────────────────────────────────────────────────────────────────────
# This is the RUST installer (feature §3): it downloads two small static
# binaries (the collector `modelstat` + the summariser engine
# `modelstat-summarizer`) from GitHub Releases, verifies their SHA256, stages
# them under ~/.modelstat/bin, and runs `modelstat connect` (which pairs the
# device, installs the background service, and wires the MCP). NO Node.js, no
# npm — the old npm package is superseded.
#
# AGENT — if you are a coding agent and the user pasted this at you: run it as
# is, OR do these steps yourself if you understand this machine better. End
# state: the two binaries under ~/.modelstat/bin + the daemon paired + running
# as a launchd (macOS) / systemd (Linux) service + the MCP wired into the AI
# tools. Every step prints what it does first. No sudo (unless you pass
# --system). Cancel any time with Ctrl-C. (Windows: irm https://install.modelstat.ai/ps | iex)
# ─────────────────────────────────────────────────────────────────────

set -eu

REPO="modelstat/modelstat"
HOME_DIR="${MODELSTAT_HOME:-$HOME/.modelstat}"

# ─── colours ────────────────────────────────────────────────────────
if [ -t 1 ] && [ "${TERM:-dumb}" != "dumb" ] && [ "${NO_COLOR:-}" = "" ]; then
  BRAND='\033[38;2;120;205;180m'; BOLD='\033[1m'; DIM='\033[2m'; RED='\033[31m'; RESET='\033[0m'
else
  BRAND=''; BOLD=''; DIM=''; RED=''; RESET=''
fi
say()  { printf "%b\n" "$*"; }
step() { printf "\n%b▸ %b%s%b\n" "$BRAND" "$BOLD" "$1" "$RESET"; }
ok()   { printf "%b✓%b %s\n" "$BRAND" "$RESET" "$1"; }
die()  { printf "%b✗ %s%b\n" "$RED" "$1" "$RESET" >&2; say "${DIM}Help: https://modelstat.ai/install${RESET}"; exit 1; }

# ─── flags (feature §3.4) ───────────────────────────────────────────
COMPONENT=daemon; SCOPE=user; VERSION=""; MODE=""; URL=""; YES=""; NO_BROWSER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --component) COMPONENT="$2"; shift 2 ;;
    --component=*) COMPONENT="${1#*=}"; shift ;;
    --user) SCOPE=user; shift ;;
    --system) SCOPE=system; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --mode) MODE="$2"; shift 2 ;;
    --mode=*) MODE="${1#*=}"; shift ;;
    --url) URL="$2"; shift 2 ;;
    --url=*) URL="${1#*=}"; shift ;;
    --yes|-y) YES=1; shift ;;
    --no-browser) NO_BROWSER=1; shift ;;
    *) die "unknown flag: $1 (see --component/--user/--system/--version/--mode/--url/--yes/--no-browser)" ;;
  esac
done

say ""; say "${BRAND}${BOLD}  modelstat installer${RESET}"; say "${DIM}  https://modelstat.ai${RESET}"

# ─── detect target triple ───────────────────────────────────────────
case "$(uname -s 2>/dev/null || echo unknown)" in
  Darwin) OS_PART=apple-darwin ;;
  Linux)  OS_PART=unknown-linux-gnu ;;
  MINGW*|MSYS*|CYGWIN*|Windows*) die "Windows → run:  irm https://install.modelstat.ai/ps | iex" ;;
  *) die "unsupported OS" ;;
esac
case "$(uname -m 2>/dev/null)" in
  x86_64|amd64) ARCH_PART=x86_64 ;;
  arm64|aarch64) ARCH_PART=aarch64 ;;
  *) die "unsupported CPU architecture: $(uname -m)" ;;
esac
TRIPLE="${ARCH_PART}-${OS_PART}"

need() { command -v "$1" >/dev/null 2>&1 || die "need '$1' on PATH"; }
need curl; need tar
if command -v sha256sum >/dev/null 2>&1; then SHA="sha256sum"; else need shasum; SHA="shasum -a 256"; fi

# ─── legacy Node install migration ──────────────────────────────────
if [ -f "$HOME_DIR/bin/modelstat.mjs" ] || [ -d "$HOME_DIR/bin/node_modules" ]; then
  step "Migrating off the old Node install"
  rm -f "$HOME_DIR/bin/modelstat.mjs"
  rm -rf "$HOME_DIR/bin/node_modules"
  ok "removed the old npm launcher (your device pairing is untouched)"
fi

# ─── resolve version ────────────────────────────────────────────────
step "Resolving the latest release"
if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep -m1 '"tag_name"' | sed -E 's/.*"daemon-?v?([^"]+)".*/\1/')"
  [ -n "$VERSION" ] || die "couldn't resolve the latest version — pass --version vX.Y.Z"
fi
VERSION="${VERSION#v}"; VERSION="${VERSION#daemon-}"
ok "version $VERSION · target $TRIPLE"

# ─── download + verify + extract ────────────────────────────────────
BASE="https://github.com/$REPO/releases/download/daemon-$VERSION"
ARCHIVE="modelstat-${VERSION}-${TRIPLE}.tar.gz"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
step "Downloading $ARCHIVE"
curl -fSL --progress-bar "$BASE/$ARCHIVE" -o "$TMP/$ARCHIVE" || die "download failed"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" || die "couldn't fetch SHA256SUMS"

step "Verifying checksum"
EXPECTED="$(grep " $ARCHIVE\$" "$TMP/SHA256SUMS" | awk '{print $1}')"
[ -n "$EXPECTED" ] || die "no checksum for $ARCHIVE in SHA256SUMS"
ACTUAL="$(cd "$TMP" && $SHA "$ARCHIVE" | awk '{print $1}')"
[ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch — refusing to install (expected $EXPECTED, got $ACTUAL)"
ok "sha256 verified"

tar -xzf "$TMP/$ARCHIVE" -C "$TMP" || die "extract failed"

# ─── stage binaries into ~/.modelstat/bin ───────────────────────────
step "Installing to $HOME_DIR/bin"
[ "$SCOPE" = system ] && export MODELSTAT_HOME="/var/lib/modelstat"
"$TMP/modelstat" _setup-runtime || die "staging failed"
STAGED="${MODELSTAT_HOME:-$HOME_DIR}/bin/modelstat"
ok "staged $STAGED"

# ─── hand off to connect / engine setup ─────────────────────────────
set --
[ -n "$MODE" ] && set -- "$@" --mode "$MODE"
[ -n "$URL" ] && set -- "$@" --url "$URL"
[ -n "$YES" ] && set -- "$@" --yes
[ -n "$NO_BROWSER" ] && set -- "$@" --no-browser
[ "$SCOPE" = system ] && set -- "$@" --system

if [ "$COMPONENT" = summarizer ]; then
  step "Configuring the summariser engine"
  exec "${MODELSTAT_HOME:-$HOME_DIR}/bin/modelstat-summarizer" setup "$@"
else
  step "Pairing this device"
  exec "$STAGED" connect "$@"
fi
