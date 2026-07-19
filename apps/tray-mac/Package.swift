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
// Wired into the macOS install path by the Rust CLI (modelstat-service):
// `_setup-runtime` stages this bundle and `install_tray_autostart` installs
// a launchd agent that execs this binary directly, so the menu-bar icon
// starts at login and is restarted on crash. The daemon has its own
// separate launchd agent — its ONLY supervisor. The tray never runs the
// daemon; it converges it via `modelstat _ensure-daemon`.
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
