#!/usr/bin/env sh
# modelstat installer — one command sets up everything (macOS + Linux).
#
#   curl -fsSL https://modelstat.ai/install.sh | sh
#
# ─────────────────────────────────────────────────────────────────────
# This is the RUST installer (feature §3): it downloads two small static
# binaries (the collector `modelstat` + the summariser engine
# `modelstat-summarizer`) from GitHub Releases, verifies their SHA256, stages
# them under ~/.modelstat/bin, puts that directory on your PATH, and runs
# `modelstat connect` (which pairs the device, installs the background service,
# and wires the MCP). NO Node.js, no npm — the old npm package is superseded.
#
# AGENT — if you are a coding agent and the user pasted this at you: run it as
# is, OR do these steps yourself if you understand this machine better. End
# state: the two binaries under ~/.modelstat/bin (on PATH) + the daemon paired + running
# as a launchd (macOS) / systemd (Linux) service + the MCP wired into the AI
# tools. Every step prints what it does first. No sudo (unless you pass
# --system). Cancel any time with Ctrl-C. (Windows: irm https://modelstat.ai/install.ps1 | iex)
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
COMPONENT=daemon; SCOPE=user; VERSION=""; MODE=""; URL=""; YES=""; NO_BROWSER=""; NO_AUTO_UPDATE=""
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
    --no-auto-update) NO_AUTO_UPDATE=1; shift ;;
    *) die "unknown flag: $1 (see --component/--user/--system/--version/--mode/--url/--yes/--no-browser/--no-auto-update)" ;;
  esac
done

say ""; say "${BRAND}${BOLD}  modelstat installer${RESET}"; say "${DIM}  https://modelstat.ai${RESET}"

# ─── detect target triple ───────────────────────────────────────────
case "$(uname -s 2>/dev/null || echo unknown)" in
  Darwin) OS_PART=apple-darwin ;;
  Linux)  OS_PART=unknown-linux-gnu ;;
  MINGW*|MSYS*|CYGWIN*|Windows*) die "Windows → run:  irm https://modelstat.ai/install.ps1 | iex" ;;
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

# ─── replace an existing daemon (legacy Node OR a previous install) ──
# Both daemons register the SAME service name, so installing ours would replace
# the old one anyway — but stop it explicitly first so the handover is visible
# and there is never a moment where two daemons race the same home dir.
# ~/.modelstat is deliberately KEPT: it holds the device identity, so the new
# daemon continues as the SAME device (no duplicate on the dashboard).
# The CI installer-logic lane (MODELSTAT_INSTALL_STAGE_ONLY) manages no services,
# so it must never stop or replace a real daemon on the host running the test.
if [ -z "${MODELSTAT_INSTALL_STAGE_ONLY:-}" ]; then
if [ "$OS_PART" = apple-darwin ]; then
  if launchctl print "gui/$(id -u)/ai.modelstat.daemon" >/dev/null 2>&1; then
    step "Found an existing modelstat daemon — replacing it"
    launchctl bootout "gui/$(id -u)/ai.modelstat.daemon" >/dev/null 2>&1 || true
    ok "stopped the old daemon (your device pairing in $HOME_DIR is kept)"
  fi
elif [ "$OS_PART" = unknown-linux-gnu ]; then
  if systemctl --user is-active --quiet modelstat.service 2>/dev/null; then
    step "Found an existing modelstat daemon — replacing it"
    systemctl --user stop modelstat.service >/dev/null 2>&1 || true
    ok "stopped the old daemon (your device pairing in $HOME_DIR is kept)"
  fi
fi
if [ -f "$HOME_DIR/bin/modelstat.mjs" ] || [ -d "$HOME_DIR/bin/node_modules" ]; then
  step "Migrating off the old Node install"
  rm -f "$HOME_DIR/bin/modelstat.mjs"
  rm -rf "$HOME_DIR/bin/node_modules"
  ok "removed the old npm launcher (your device pairing is untouched)"
fi
# The old daemon also shipped as a GLOBAL npm package (`npm i -g modelstat`), which
# leaves a stale `modelstat` on PATH that shadows the native binary. Remove it
# best-effort — guarded on npm being present AND the package actually being global,
# so it is a no-op for anyone who never used the npm path.
if command -v npm >/dev/null 2>&1 && npm ls -g --depth=0 modelstat >/dev/null 2>&1; then
  step "Removing the old global npm package"
  npm rm -g modelstat >/dev/null 2>&1 && ok "removed the old global 'modelstat' npm package"
fi
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

# Auto-update policy (feature §13). Default ON — it is how users receive fixes.
# Turned OFF only when you asked (--no-auto-update), or when this release is
# marked a PRE-RELEASE on GitHub: you deliberately installed a test build, so the
# daemon must not immediately update itself off the very thing you're testing.
# (Detected via GitHub's own prerelease flag, NOT a version suffix — the version
# number stays a clean semver. GitHub also excludes prereleases from `latest`, so
# a plain `curl | sh` can never hand a test build to a real user.)
if [ -z "$NO_AUTO_UPDATE" ]; then
  if curl -fsSL "https://api.github.com/repos/$REPO/releases/tags/daemon-$VERSION" 2>/dev/null \
       | grep -qE '"prerelease"[[:space:]]*:[[:space:]]*true'; then
    NO_AUTO_UPDATE=1
    say "${DIM}  (pre-release — auto-update will be disabled so it stays on this build)${RESET}"
  fi
fi

# ─── download + verify + extract ────────────────────────────────────
# The release download base. Overridable ONLY for the CI installer-logic lane
# (scripts/e2e-install.sh points it at a local file:// archive so the test runs
# offline + service-free); unset in normal use → the real GitHub Releases URL.
BASE="${MODELSTAT_INSTALL_BASE_URL:-https://github.com/$REPO/releases/download/daemon-$VERSION}"
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

# ─── stage binaries into ~/.modelstat/bin + put them on PATH ────────
# `_setup-runtime` does both: it copies the binaries to the stable install path
# AND wires that path into your shell (user scope) or /usr/local/bin (--system),
# so `modelstat …` works by name. It prints every file it touches.
step "Installing to $HOME_DIR/bin"
if [ "$SCOPE" = system ]; then
  export MODELSTAT_HOME="/var/lib/modelstat"
  "$TMP/modelstat" _setup-runtime --system || die "staging failed"
else
  "$TMP/modelstat" _setup-runtime || die "staging failed"
fi
STAGED="${MODELSTAT_HOME:-$HOME_DIR}/bin/modelstat"
ok "staged $STAGED"

# Apply the auto-update policy BEFORE the daemon starts, so it never acts on a
# release verdict for a build the server doesn't know about.
if [ -n "$NO_AUTO_UPDATE" ]; then
  step "Disabling auto-update for this build"
  "$STAGED" autoupdate off >/dev/null 2>&1 || true
  ok "auto-update off — re-enable any time with: modelstat autoupdate on"
else
  # GA install (not a pre-release, no --no-auto-update): ensure auto-update is ON.
  # A pre-release you installed earlier turns it OFF, and that preference persists
  # in auto-update.json — so without this a tester moving to a GA build would
  # silently stay off. The documented default is ON (it is how fixes arrive).
  "$STAGED" autoupdate on >/dev/null 2>&1 || true
fi

# Test hook (scripts/e2e-install.sh): stop right after staging, before pairing.
# Never set in normal use — keeps the CI installer-logic lane offline + service-free.
if [ -n "${MODELSTAT_INSTALL_STAGE_ONLY:-}" ]; then
  ok "staged (MODELSTAT_INSTALL_STAGE_ONLY set) — skipping connect"
  exit 0
fi

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
