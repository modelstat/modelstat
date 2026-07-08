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
BIN=""
# Try SwiftPM first. The `if` keeps `set -e` from aborting when `swift build`
# fails so we can fall back below. Locate the produced binary on success:
# universal builds land under .build/apple/Products/<Config>/; single-arch under
# .build/<config>/ (a triple-prefixed dir SwiftPM also symlinks as .build/<config>).
if swift build "${BUILD_ARGS[@]}"; then
  for cand in \
    ".build/apple/Products/Release/modelstat-tray" \
    ".build/apple/Products/Debug/modelstat-tray" \
    ".build/${CONFIG}/modelstat-tray" \
    ".build/release/modelstat-tray" \
    ".build/debug/modelstat-tray"; do
    if [[ -f "$cand" ]]; then BIN="$cand"; break; fi
  done
fi

# Fallback: SwiftPM couldn't build. The usual cause is a toolchain whose
# Package.swift MANIFEST ABI is broken (e.g. a beta Swift where the older
# `swift-tools-version` manifest symbol is missing) — NOT a problem with the app
# itself, which is a single dependency-free main.swift. So compile the sources
# directly with swiftc. This keeps the tray buildable on the exact toolchains
# where `swift build` can't even parse the manifest; the SwiftPM path above
# (incl. the universal CI build) is untouched. Host-arch only — a universal fat
# binary needs SwiftPM/xcbuild, which is the CI path.
if [[ -z "$BIN" ]]; then
  echo "▶ SwiftPM build unavailable — compiling main.swift directly with swiftc"
  if [[ "${TRAY_UNIVERSAL:-}" == "1" ]]; then
    echo "  note: universal (arm64+x86_64) needs SwiftPM/Xcode; the swiftc fallback builds host-arch only." >&2
  fi
  mkdir -p .build/fallback
  BIN=".build/fallback/modelstat-tray"
  swiftc -O -framework AppKit -framework Foundation Sources/ModelstatTray/*.swift -o "$BIN"
fi

if [[ -z "$BIN" || ! -f "$BIN" ]]; then
  echo "✗ could not produce a modelstat-tray binary (SwiftPM and swiftc both failed)" >&2
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
