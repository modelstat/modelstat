#!/usr/bin/env bash
# Build ModelstatTray.app from the Swift package.
#
# Output:
#   build/ModelstatTray.app           (ad-hoc-signed bundle, ready to run/ship)
#
# Modes (env):
#   TRAY_BUILD_CONFIG=release|debug   build config       (default: release)
#   TRAY_UNIVERSAL=1                  arm64 + x86_64 fat  (default: host arch)
#                                     — universal needs FULL Xcode (xcbuild);
#                                       host-arch builds on Command Line Tools
#                                       alone, which is the on-device fallback.
#
# Signing: we ad-hoc sign (`codesign -s -`). That is enough to RUN — arm64
# requires at least an ad-hoc signature, and an app delivered inside the npm
# tarball is not quarantined, so Gatekeeper's notarization gate never fires.
# To distribute a DOWNLOADED build (a DMG off the website) without a Gatekeeper
# prompt, re-sign with a Developer ID cert + Hardened Runtime and notarize:
#   codesign --force --options runtime --timestamp \
#            --sign "Developer ID Application: …" build/ModelstatTray.app
#   xcrun notarytool submit … && xcrun stapler staple build/ModelstatTray.app
# That is purely additive and slots into CI once the Apple account exists.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

APP_NAME="ModelstatTray"
BUNDLE="build/${APP_NAME}.app"
CONFIG="${TRAY_BUILD_CONFIG:-release}"

BUILD_ARGS=(-c "$CONFIG")
if [[ "${TRAY_UNIVERSAL:-}" == "1" ]]; then
  BUILD_ARGS+=(--arch arm64 --arch x86_64)
fi

echo "▶ swift build ${BUILD_ARGS[*]}"
swift build "${BUILD_ARGS[@]}"

# Locate the produced binary. Universal builds land under
# .build/apple/Products/<Config>/; single-arch under .build/<config>/ (a
# triple-prefixed dir that SwiftPM also symlinks as .build/<config>).
BIN=""
for cand in \
  ".build/apple/Products/Release/modelstat-tray" \
  ".build/apple/Products/Debug/modelstat-tray" \
  ".build/${CONFIG}/modelstat-tray" \
  ".build/release/modelstat-tray" \
  ".build/debug/modelstat-tray"; do
  if [[ -f "$cand" ]]; then BIN="$cand"; break; fi
done
if [[ -z "$BIN" ]]; then
  echo "✗ could not find a built modelstat-tray binary under .build/" >&2
  exit 1
fi
echo "▶ binary: $BIN"
lipo -archs "$BIN" 2>/dev/null | sed 's/^/  arches: /' || true

# Assemble the bundle fresh (leave .build/ so incremental compiles stay fast).
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "$BIN" "$BUNDLE/Contents/MacOS/modelstat-tray"
chmod +x "$BUNDLE/Contents/MacOS/modelstat-tray"
cp "Resources/Info.plist" "$BUNDLE/Contents/Info.plist"

# Embedded PkgInfo file — AppKit used to be picky about this. Costs four
# bytes to include and silences a startup warning on older macOS.
printf "APPL????" > "$BUNDLE/Contents/PkgInfo"

# Ad-hoc sign the assembled bundle (seals it; required to launch on arm64).
echo "▶ codesign --force --sign - (ad-hoc)"
codesign --force --sign - "$BUNDLE"

echo
echo "✓ Built $BUNDLE"
codesign -dv "$BUNDLE" 2>&1 | grep -iE "signature|identifier" | sed 's/^/  /' || true
echo "  run:  open '$BUNDLE'"
echo "  install: cp -R '$BUNDLE' /Applications/"
