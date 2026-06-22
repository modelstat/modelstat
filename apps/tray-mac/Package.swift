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
// Wired into the macOS install path in apps/daemon/src/service.ts: the
// installer stages this bundle (installTrayApp) and opens it (openTrayApp).
// The launchd service runs the headless daemon — a GUI app exits
// 78/EX_CONFIG under launchd — while the tray runs in the GUI session as a
// Login Item (ensureLoginItem in main.swift) and spawns `modelstat start`
// as a child, with the singleton lock keeping it to one live daemon.
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
