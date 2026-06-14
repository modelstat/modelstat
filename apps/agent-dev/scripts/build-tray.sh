#!/usr/bin/env bash
# Bundle the macOS tray sources alongside the agent's npm build so
# `pnpm pack` ships the Swift package plus the install script.
#
# We intentionally ship SOURCES, not a pre-built .app:
#   · a .app needs codesigning to launch without Gatekeeper warnings,
#     and signing requires the team's Developer ID cert which CI has
#     but an npm publish pipeline shouldn't.
#   · the Swift build is ~3 seconds on an M-series laptop and every
#     install.sh candidate (macOS) already has — or can install —
#     `xcode-select` in one command.
#   · sources cross-compile: the same tarball works on arm64 + x86_64.
#
# Idempotent — re-running clears the prior vendor copy before writing
# new sources.

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
