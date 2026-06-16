#!/usr/bin/env bash
# Bundle the macOS tray sources alongside the agent's npm build so
# `pnpm pack` ships the Swift package plus the install script.
#
# These sources are the FALLBACK, not the primary install path. The
# release pipeline (release-build.yml, on a macOS runner) builds a
# universal, ad-hoc-signed ModelstatTray.app and drops it at
# vendor/ModelstatTray.app BEFORE pack, so the published tarball ships a
# ready-to-run binary and no end user compiles anything. We keep the
# sources because:
#   · `bundledTrayAppPath()` compiles them on-device if the prebuilt app
#     is ever missing (e.g. a dev `install:local`, or a future arch).
#   · they're tiny next to the binary and document exactly what shipped.
# NB: a COLD `swift build` is ~1 min (not the "~3s" warm rebuild this
# comment used to claim) and needs Xcode CLT — which is exactly why the
# prebuilt app, not this fallback, is the path users hit.
#
# Idempotent — re-running clears the prior vendor copy before writing
# new sources. It does NOT touch vendor/ModelstatTray.app (the CI step
# owns that), so packing locally keeps any prebuilt app already present.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
AGENT_ROOT="$(cd "$HERE/.." && pwd)"
REPO_ROOT="$(cd "$AGENT_ROOT/../.." && pwd)"
SRC="$REPO_ROOT/apps/tray-mac"
DEST="$AGENT_ROOT/vendor/tray-mac"

if [ ! -d "$SRC" ]; then
  echo "tray-mac sources not found at $SRC — skipping" >&2
  exit 0
fi

rm -rf "$DEST"
mkdir -p "$DEST"
# Copy sources + build script, skip local build outputs.
cp -R "$SRC/Package.swift" "$DEST/"
cp -R "$SRC/Sources" "$DEST/"
cp -R "$SRC/Resources" "$DEST/"
cp "$SRC/build-app.sh" "$DEST/"
cp "$SRC/.gitignore" "$DEST/" 2>/dev/null || true
chmod +x "$DEST/build-app.sh"

echo "✓ tray sources bundled into $DEST"

# Surface whether the CI macOS step has staged the prebuilt app, so the
# pack log makes it obvious which path users will hit.
PREBUILT="$AGENT_ROOT/vendor/ModelstatTray.app"
if [ -d "$PREBUILT" ]; then
  echo "✓ prebuilt $PREBUILT present — users get the binary (no on-device compile)"
else
  echo "ℹ no prebuilt ModelstatTray.app — tarball will compile on-device as a fallback" >&2
fi
