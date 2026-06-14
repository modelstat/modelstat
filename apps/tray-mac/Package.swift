// swift-tools-version:5.9
//
// ModelstatTray — a tiny menu-bar app for macOS.
//
// No third-party deps. AppKit provides NSStatusItem for the menu-bar
// icon; Foundation is enough for subprocess + JSON. The whole thing
// is a single main.swift so release builds are fast and the bundle
// we ship in the DMG is small (~1 MB).
//
// Build:
//   swift build -c release             (executable at .build/release/modelstat-tray)
//   ./build-app.sh                     (wraps the binary in ModelstatTray.app)
//
// Wired into the macOS install path in apps/agent-dev/src/service.ts —
// the launchd plist launches THIS instead of the headless daemon; the
// tray then spawns `modelstat start` as a child so there's still only
// one process managing the pipeline.
import PackageDescription

let package = Package(
  name: "ModelstatTray",
  platforms: [.macOS(.v12)],
  products: [
    .executable(name: "modelstat-tray", targets: ["ModelstatTray"])
  ],
  targets: [
    .executableTarget(
      name: "ModelstatTray",
      path: "Sources/ModelstatTray"
    )
  ]
)
