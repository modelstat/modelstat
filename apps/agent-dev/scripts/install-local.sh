#!/usr/bin/env bash
# Build + pack + install the @modelstat/agent CLI globally from the
# local tarball. Works on macOS + Linux, with pnpm / bun / npm.
#
# Uninstall any previous global copy first to avoid two `modelstat`
# binaries ending up on $PATH (nvm bin + pnpm bin + bun bin can coexist
# and the one that wins depends on shell PATH order — confusing).
set -euo pipefail

cd "$(dirname "$0")/.."
pnpm run build

TARBALL_DIR=/tmp
# `npm pack` names the tarball after the package's `name` field. The
# package was renamed `@modelstat/agent` → `modelstat`, so the glob
# now matches `modelstat-VERSION.tgz`. The negative lookahead-style
# trick — `modelstat-[0-9]*.tgz` — keeps us from accidentally matching
# `modelstat-public-*.tgz` or similar siblings if a future repo dumps
# tarballs into /tmp at the same time.
rm -f "$TARBALL_DIR"/modelstat-[0-9]*.tgz
npm pack --pack-destination "$TARBALL_DIR" >/dev/null
TARBALL=$(ls "$TARBALL_DIR"/modelstat-[0-9]*.tgz | head -1)
echo "▶ packed: $TARBALL"

# Uninstall any previous global copies (ignore errors — each PM will
# simply no-op if it didn't install it).
echo "▶ removing any existing global modelstat installs (both package names)"
for name in modelstat @modelstat/agent; do
  command -v pnpm >/dev/null 2>&1 && pnpm remove -g "$name" >/dev/null 2>&1 || true
  command -v bun  >/dev/null 2>&1 && bun  remove -g "$name" >/dev/null 2>&1 || true
  command -v npm  >/dev/null 2>&1 && npm  uninstall -g "$name" >/dev/null 2>&1 || true
done

# Pick the installer:
#   PNPM_HOME is set AND on PATH          → pnpm
#   else bun on PATH                      → bun
#   else npm (always available with node) → npm
INSTALLER=npm
if command -v pnpm >/dev/null 2>&1 && [[ -n "${PNPM_HOME:-}" ]] && [[ ":$PATH:" == *":$PNPM_HOME:"* ]]; then
  INSTALLER=pnpm
elif command -v bun >/dev/null 2>&1; then
  INSTALLER=bun
fi

echo "▶ installing with $INSTALLER: $TARBALL"
case "$INSTALLER" in
  pnpm) pnpm install -g "$TARBALL" ;;
  bun)  bun install -g "$TARBALL"  ;;
  npm)  npm install -g "$TARBALL"  ;;
esac

rm -f "$TARBALL"

echo
echo "✓ modelstat installed via $INSTALLER"
if command -v modelstat >/dev/null 2>&1; then
  echo "  path:  $(command -v modelstat)"
  MODELSTAT_VERSION=$(node -e "console.log(require(require('path').dirname(require('fs').realpathSync(process.argv[1])) + '/../package.json').version)" "$(command -v modelstat)" 2>/dev/null || echo "?")
  echo "  version: $MODELSTAT_VERSION"
fi
echo
echo "  Next: modelstat"
