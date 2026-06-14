#!/usr/bin/env bash
# Build ModelstatTray.app from the Swift package.
#
# Output:
#   .build/release/modelstat-tray     (raw executable)
#   build/ModelstatTray.app           (bundle ready to drop into /Applications)
#
# Usage:
#   ./build-app.sh                    (release build, universal when running on arm64)
#   SWIFT_ARCH=x86_64 ./build-app.sh  (cross-build for Intel)
#
# We deliberately DO NOT codesign here — the installer pipeline takes
# care of that with the team's Developer ID. This script just has to
# produce a runnable bundle; `codesign` + `create-dmg` happen in CI.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

APP_NAME="ModelstatTray"
BUNDLE="build/${APP_NAME}.app"

# Clean prior bundle; leave the .build/ cache so incremental swift
# compiles stay fast.
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"

echo "▶ swift build -c release"
swift build -c release

cp ".build/release/modelstat-tray" "$BUNDLE/Contents/MacOS/modelstat-tray"
chmod +x "$BUNDLE/Contents/MacOS/modelstat-tray"
cp "Resources/Info.plist" "$BUNDLE/Contents/Info.plist"

# Embedded PkgInfo file — AppKit used to be picky about this. Costs
# four bytes to include and silences a startup warning on older macOS.
printf "APPL????" > "$BUNDLE/Contents/PkgInfo"

echo
echo "✓ Built $BUNDLE"
echo "  run:  open '$BUNDLE'"
echo "  install: cp -R '$BUNDLE' /Applications/"
