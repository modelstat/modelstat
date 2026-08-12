// ModelstatTray — macOS menu-bar app for the modelstat agent.
//
// What it does:
//   · puts a "◉" status item in the menu bar
//   · keeps the launchd-managed daemon converged via `modelstat
//     _ensure-daemon` (boot + a 30s watchdog) — the tray NEVER runs the
//     daemon itself
//   · reads ~/.modelstat/last-status.json every second for the live phase
//   · polls `modelstat status --json` every 15 s for the slow-moving data
//   · offers: Open dashboard, Copy claim URL, View pipeline, Pause/Resume,
//     Quit
//
// Shelling the CLI is deliberate: the CLI owns supervision, ingestion and
// auth; the tray is a thin renderer + nudger — one file, no dependencies
// beyond AppKit/Foundation.

import AppKit
import Darwin
import Foundation

// ── Resolve the `modelstat` CLI at the canonical runtime location every
//    installer stages the collector into (~/.modelstat/bin/modelstat) — so we
//    don't rely on shell profiles loading in launchd's env — then fall back to a
//    login-shell `which` for PATH-based (e.g. Homebrew) installs.
func locateCli() -> URL? {
  let fm = FileManager.default
  let home = NSHomeDirectory()
  let candidates = [
    "\(home)/.modelstat/bin/modelstat",
  ]
  for p in candidates {
    if fm.isExecutableFile(atPath: p) { return URL(fileURLWithPath: p) }
  }
  // Last-ditch: `which modelstat` via a login shell so PATH lookups
  // honour the user's zsh/bash config (covers Homebrew / PATH installs).
  let task = Process()
  task.launchPath = "/bin/zsh"
  task.arguments = ["-l", "-c", "which modelstat"]
  let pipe = Pipe()
  task.standardOutput = pipe
  task.standardError = Pipe()
  do { try task.run(); task.waitUntilExit() } catch { return nil }
  let out = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
  let path = out.trimmingCharacters(in: .whitespacesAndNewlines)
  return path.isEmpty ? nil : URL(fileURLWithPath: path)
}

struct AgentStats: Decodable {
  let paired: Bool?
  let claimed: Bool?
  let dashboard: String?
  let claim_code: String?
  let status: String?
  let claim_url: String?
  let agent_url: String?
  let device: DeviceInfo?
  let analyzed: AnalyzedInfo?
  /// Daemon heartbeat snapshot mirrored to ~/.modelstat/last-status.json.
  /// Present on both unclaimed and claimed responses post-0.0.30 so the
  /// tray can show real numbers even when the public device-view 404s
  /// (claimed devices). Older daemons that don't write the file leave
  /// this nil.
  let local: LocalStatus?
  /// Where sessions get summarised (cloud/local/self-hosted). Drives the
  /// "Summariser" submenu — its title, checkmarks, and enabled state. nil on
  /// older daemons that don't report it yet.
  let summarizer: SummarizerInfo?
  /// Where turns get SCRUBBED (cloud/local/self-hosted) — the redactor's
  /// submenu twin. nil on older daemons that don't report it yet.
  let redactor: SummarizerInfo?
}

/// Active summariser/redactor mode + (self-hosted only) endpoint, from
/// `status --json`. One shape serves both settings — same fields, same
/// submenu mechanics.
struct SummarizerInfo: Decodable {
  /// "cloud" | "local" | "self-hosted".
  let mode: String?
  /// Self-hosted endpoint — present only in self-hosted mode.
  let url: String?
  let model: String?
  /// True when the mode's env var is forcing it; a switch from the tray would
  /// be masked by the env var, so the submenu disables itself.
  let env_override: Bool?
}

struct DeviceInfo: Decodable {
  let hostname: String?
  let os_family: String?
  let daemon_status: String?
  let last_seen_at: String?
}

/// Server-side totals for an unclaimed device, from the device-view endpoint.
/// Only the token total is rendered — it is the one figure the menu shows that
/// the device cannot count for itself.
struct AnalyzedInfo: Decodable {
  let totalTokens: String?
}

struct LocalStatus: Decodable {
  let status: String?
  /// Stated by the daemon (busy right now?) — the tray must not re-derive it
  /// from phase names it happens to know.
  let active: Bool?
  /// The daemon's own sentence for the phase, and the only place the
  /// work-remaining number comes from. The tray does not recompute it from
  /// `progress_done`/`progress_total`: two renderings of one fact is exactly
  /// what this menu had too much of.
  let message: String?
  /// Epoch ms when the work now in progress started, or nil when idle. A
  /// timestamp rather than a duration so this menu can tick the elapsed clock
  /// on its own 1s beat — the daemon does not rewrite its mirror to animate it.
  let busy_since_ms: Int64?
  /// The batch in flight right now, if any. Sessions leave together, so its
  /// `since_ms` dates the oldest one still running.
  let uploading: UploadingNow?
  let daemon_version: String?
  /// When the daemon last wrote the mirror. It rewrites at least every 10s
  /// even when nothing changed, so this is the local liveness signal: a stale
  /// written_at means the file is a fossil from a daemon that's gone.
  let written_at: String?
  let stats: LocalStatsCounters?
  /// Server release verdict (the daemon sets this from the heartbeat response).
  let update: UpdateInfo?
  /// Effective auto-update setting — drives the tray's checkbox.
  let auto_update: Bool?
}

struct UploadingNow: Decodable {
  /// Sessions still in flight — counted down as each upload commits.
  let sessions: Int?
  /// Epoch ms the batch started — a timestamp, so this menu ticks the elapsed
  /// clock on its own 1s beat without the daemon rewriting the mirror.
  let since_ms: Int64?
}

struct UpdateInfo: Decodable {
  /// "ok" | "update_available" | "upgrade_required".
  let verdict: String?
  /// Latest published version, when known.
  let latest: String?
}

/// The lifetime counters the daemon keeps. It publishes many; the menu renders
/// the one that measures the WORK (events that reached the server). Files
/// scanned, batches, segments and detections are all by-products of producing
/// that number, and a row for each turned the menu into a debug dump.
struct LocalStatsCounters: Decodable {
  let events_uploaded: Int?
}

@MainActor
final class TrayController: NSObject {
  private let statusItem: NSStatusItem
  private let menu = NSMenu()
  /// Re-resolved by ensureDaemon() when nil — a transient resolution
  /// failure at tray boot (e.g. the installer is mid-rewrite of
  /// ~/.modelstat/bin) must not permanently strand the daemon.
  private var cli: URL?
  private var paused = false
  /// Serialises `_ensure-daemon` runs; collapses overlapping
  /// ensureDaemon() calls into one in-flight run.
  private let superviseQueue = DispatchQueue(label: "ai.modelstat.tray.supervise", qos: .utility)
  private var ensureInFlight = false
  private var latest: AgentStats?
  /// Live local heartbeat, read straight from ~/.modelstat/last-status.json
  /// on the fast timer. Decoupled from `latest` (the slower, network-backed
  /// `status --json` shell-out) so the menu's numbers move every second.
  private var localLatest: LocalStatus?
  /// Advances once per fast tick to drive the "alive" pulse on the status
  /// line — a cheap, honest signal that the agent is doing work right now.
  private var spinnerTick = 0
  private var fastTimer: Timer?
  private var slowTimer: Timer?
  private var watchdogTimer: Timer?

  // Menu items we update on every poll.
  //
  // FOUR numbers, one per kind, and no fifth. This menu once carried a dozen —
  // files left, new, skipped, events this pass, events uploaded, files scanned,
  // segments sent, live sessions, tools, accounts — which are four facts sliced
  // nine ways. A reader had to work out which counter answered their question
  // before they could read the answer, so the menu said less the more it said.
  //   1. how much work is left        → files left        (statusMI)
  //   2. how much has been measured   → events or tokens  (totalsMI)
  //   3. how much is moving right now → sessions in flight (workMI)
  //   4. how long that has taken      → the clock on workMI
  /// Phase + the one work-remaining number, verbatim from the daemon.
  private let statusMI = NSMenuItem(title: "Loading…", action: nil, keyEquivalent: "")
  /// Sessions the daemon has in flight, and how long the oldest has been there.
  /// The count says it is moving, the clock says it is not stuck — and a
  /// duration only means anything beside the thing it times, so they share a row.
  private let workMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  /// The one total: everything this device has measured, ever.
  private let totalsMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  private let deviceMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  private let claimMI = NSMenuItem(title: "Open device page", action: #selector(openDashboard), keyEquivalent: "o")
  private let copyClaimMI = NSMenuItem(title: "Copy claim URL", action: #selector(copyClaimUrl), keyEquivalent: "c")
  private let jobsMI = NSMenuItem(title: "View pipeline…", action: #selector(openJobs), keyEquivalent: "j")
  private let pauseMI = NSMenuItem(title: "Pause", action: #selector(togglePaused), keyEquivalent: "p")
  /// "Summariser: <mode>" parent + submenu to switch where sessions summarise.
  /// The active mode carries a checkmark; Local warns about its RAM/battery
  /// cost before switching; Self-hosted needs a URL+model so it points at the
  /// CLI. Redaction runs on-device in every mode — this only moves the summary.
  private let summariserMI = NSMenuItem(title: "Summariser", action: nil, keyEquivalent: "")
  private let summariserSubmenu = NSMenu()
  private let modeCloudMI = NSMenuItem(
    title: "Cloud — modelstat's servers", action: #selector(switchModeCloud), keyEquivalent: "")
  private let modeLocalMI = NSMenuItem(
    title: "Local — on this machine…", action: #selector(switchModeLocal), keyEquivalent: "")
  private let modeSelfHostedMI = NSMenuItem(
    title: "Self-hosted — your endpoint…", action: #selector(switchModeSelfHosted),
    keyEquivalent: "")
  /// "Redactor: <mode>" parent + submenu — where turns get SCRUBBED. The
  /// layer-1 secret floor always runs on this machine; the submenu only moves
  /// the layer-2 PII model (cloud is the default, local the privacy opt-out).
  private let redactorMI = NSMenuItem(title: "Redactor", action: nil, keyEquivalent: "")
  private let redactorSubmenu = NSMenu()
  private let redactorCloudMI = NSMenuItem(
    title: "Cloud — modelstat's servers", action: #selector(switchRedactorCloud), keyEquivalent: "")
  private let redactorLocalMI = NSMenuItem(
    title: "Local — on this machine…", action: #selector(switchRedactorLocal), keyEquivalent: "")
  private let redactorSelfHostedMI = NSMenuItem(
    title: "Self-hosted — your endpoint…", action: #selector(switchRedactorSelfHosted),
    keyEquivalent: "")
  /// "Update now" — shown only when the server says this daemon is behind.
  private let updateMI = NSMenuItem(title: "Update now", action: #selector(updateNow), keyEquivalent: "u")
  /// Checkable "Auto-update" — reflects (and toggles) the daemon's setting.
  private let autoUpdateMI = NSMenuItem(
    title: "Auto-update", action: #selector(toggleAutoUpdate), keyEquivalent: "")

  override init() {
    self.cli = locateCli()
    self.statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    super.init()
    configureStatusItem()
    buildMenu()
    ensureDaemon()
    refreshStats()
    tickLocal()

    // Two cadences, both registered in `.common` mode so they keep firing
    // while the menu is open and being tracked — a plain
    // `Timer.scheduledTimer` only runs in `.default` mode, so the dropdown
    // froze the moment you opened it. That alone made a working agent look
    // stuck.
    //   · fast (1s): read last-status.json directly — cheap, no subprocess
    //     — and re-render so phase/segment numbers tick live.
    //   · slow (15s): shell out to `modelstat status --json` for the
    //     network-backed paired/claimed/device/analyzed data that barely
    //     changes (and, for claimed devices, costs a 404 round-trip).
    let fast = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
      MainActor.assumeIsolated { self?.tickLocal() }
    }
    RunLoop.main.add(fast, forMode: .common)
    fastTimer = fast
    let slow = Timer(timeInterval: 15.0, repeats: true) { [weak self] _ in
      MainActor.assumeIsolated { self?.refreshStats() }
    }
    RunLoop.main.add(slow, forMode: .common)
    slowTimer = slow
    // Watchdog: re-converge the daemon every 30s no matter how the
    // last attempt ended — heals the give-up paths (CLI unresolvable
    // at boot, spawn throw, adopted daemon died) that used to strand
    // the pipeline until the user noticed the tray frozen.
    let watchdog = Timer(timeInterval: 30.0, repeats: true) { [weak self] _ in
      MainActor.assumeIsolated { self?.ensureDaemon() }
    }
    RunLoop.main.add(watchdog, forMode: .common)
    watchdogTimer = watchdog
  }

  private func configureStatusItem() {
    // SF Symbol falls back to a bullet on older macOS versions; the
    // title always works so we stack title + symbol for robustness.
    if #available(macOS 11.0, *),
       let btn = statusItem.button,
       let img = NSImage(systemSymbolName: "circle.hexagongrid.fill", accessibilityDescription: "modelstat")
    {
      img.isTemplate = true
      btn.image = img
    } else {
      statusItem.button?.title = "◉"
    }
    statusItem.button?.toolTip = "modelstat"
  }

  /// The non-clickable info rows at the top of the menu, in order.
  private var infoItems: [NSMenuItem] {
    [statusMI, workMI, totalsMI, deviceMI]
  }

  /// Set an info row's title, hiding the row when the title is empty.
  /// A disabled `NSMenuItem` with an empty title still takes a full
  /// row of height — that was the stray blank space under "Claimed ✓".
  private func setInfo(_ item: NSMenuItem, _ title: String) {
    item.title = title
    item.isHidden = title.isEmpty
  }

  private func buildMenu() {
    for mi in infoItems {
      mi.isEnabled = false
      mi.isHidden = mi.title.isEmpty
      menu.addItem(mi)
    }
    menu.addItem(NSMenuItem.separator())
    claimMI.target = self
    copyClaimMI.target = self
    jobsMI.target = self
    pauseMI.target = self
    menu.addItem(claimMI)
    menu.addItem(copyClaimMI)
    menu.addItem(jobsMI)
    menu.addItem(pauseMI)
    // Summariser mode — a submenu so switching is one click from the menu bar.
    modeCloudMI.target = self
    modeLocalMI.target = self
    modeSelfHostedMI.target = self
    summariserSubmenu.addItem(modeCloudMI)
    summariserSubmenu.addItem(modeLocalMI)
    summariserSubmenu.addItem(modeSelfHostedMI)
    summariserMI.submenu = summariserSubmenu
    menu.addItem(summariserMI)
    // Redactor mode — the summariser submenu's twin.
    redactorCloudMI.target = self
    redactorLocalMI.target = self
    redactorSelfHostedMI.target = self
    redactorSubmenu.addItem(redactorCloudMI)
    redactorSubmenu.addItem(redactorLocalMI)
    redactorSubmenu.addItem(redactorSelfHostedMI)
    redactorMI.submenu = redactorSubmenu
    menu.addItem(redactorMI)
    updateMI.target = self
    autoUpdateMI.target = self
    updateMI.isHidden = true
    menu.addItem(updateMI)
    menu.addItem(autoUpdateMI)
    menu.addItem(NSMenuItem.separator())
    let logsMI = NSMenuItem(title: "Open logs folder", action: #selector(openLogs), keyEquivalent: "l")
    logsMI.target = self
    menu.addItem(logsMI)
    menu.addItem(NSMenuItem.separator())
    let quitMI = NSMenuItem(title: "Quit modelstat", action: #selector(quit), keyEquivalent: "q")
    quitMI.target = self
    menu.addItem(quitMI)
    statusItem.menu = menu
  }

  // ── Daemon lifecycle ─────────────────────────────────────────────
  //
  // The tray NEVER runs the daemon. It used to spawn `modelstat start`
  // as its own child, which made it a second supervisor competing with
  // launchd (ai.modelstat.daemon): boot races ended with a tray-parented
  // daemon the service manager knew nothing about — the launchd service
  // stood down (exit 0; KeepAlive SuccessfulExit=false never revives
  // that), and when the tray quit, the collector silently died with it.
  // Now every trigger (boot, 30s watchdog, resume) shells
  // `modelstat _ensure-daemon`, which adopts a healthy lock owner and
  // otherwise reconciles the launchd service — the ONE supervisor.
  // Pause/Quit shell `modelstat _stop-daemon` (service stays installed;
  // the next ensure or login brings it back).

  /// Converge toward "exactly one live daemon" via the CLI. Safe to call
  /// from any trigger — overlapping calls collapse into one in-flight run.
  private func ensureDaemon() {
    guard !paused else { return }
    if cli == nil { cli = locateCli() }
    guard let cli else {
      statusMI.title = "modelstat CLI not found — retrying…"
      return
    }
    guard !ensureInFlight else { return }
    ensureInFlight = true
    superviseQueue.async {
      // Off-main: adopt is a sub-ms lock probe, but the reconcile path can
      // spend seconds in launchctl.
      let ok = Self.runCli(cli: cli, args: ["_ensure-daemon"])
      // Capture self on the closure that hops back to @MainActor, not the
      // supervise-queue closure — a weak var captured across the actor hop
      // is a data race the CI toolchain rejects.
      DispatchQueue.main.async { [weak self] in
        MainActor.assumeIsolated {
          guard let self else { return }
          self.ensureInFlight = false
          if !ok {
            // Watchdog retries in ≤30s — surface it, don't give up.
            self.statusMI.title = "daemon supervision failed — retrying…"
          }
        }
      }
    }
  }

  /// `~/.modelstat/logs`, created if absent.
  private nonisolated static func logsDir() -> String {
    let dir = ("~/.modelstat/logs" as NSString).expandingTildeInPath
    try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
    return dir
  }

  /// Open `<logs>/<name>` for **appending**.
  ///
  /// `O_APPEND` is the entire point. Three writers share these files — launchd
  /// holding its own fd for the supervised daemon, this tray, and the daemon's
  /// boot-time rotation truncating them in place — so every writer has to let
  /// the kernel place each write at the current end of file.
  /// `FileHandle(forWritingAtPath:)`, which this used to use, seeks to byte 0
  /// instead and overwrites whatever is already there: it left `out.log` full of
  /// shredded half-lines, each tray run eating the head of the previous one.
  ///
  /// `nil` means the file could not be opened; the caller lets the child inherit
  /// the tray's own streams so the output still lands somewhere (`tray-*.log`)
  /// rather than being dropped.
  private nonisolated static func appendingLog(_ name: String) -> FileHandle? {
    let path = "\(logsDir())/\(name)"
    let fd = open(path, O_WRONLY | O_CREAT | O_APPEND, 0o644)
    guard fd >= 0 else {
      // Loud, and never swallowed: without this the output would silently move
      // to another file and the next person to read out.log would conclude the
      // tray had simply stopped supervising.
      let why = String(cString: strerror(errno))
      FileHandle.standardError.write(
        Data("modelstat-tray: cannot append to \(path) (\(why)) — this run's output goes to tray-err.log instead\n".utf8))
      return nil
    }
    return FileHandle(fileDescriptor: fd, closeOnDealloc: true)
  }

  /// Point a child's streams at the daemon logs, keeping the two apart.
  ///
  /// The child is a `modelstat` verb in service mode, which routes INFO to
  /// stdout and WARN/ERROR to stderr. Filing them separately is what makes
  /// `out.log`/`err.log` mean the same thing here as they do when launchd
  /// supervises the daemon directly — merging both into one file would put
  /// warnings in `out.log` for these runs only.
  private nonisolated static func attachDaemonLogs(_ p: Process) {
    if let out = appendingLog("out.log") { p.standardOutput = out }
    if let err = appendingLog("err.log") { p.standardError = err }
  }

  /// Run `modelstat <args>` to completion, output onto the daemon logs.
  /// Returns whether it exited 0.
  private nonisolated static func runCli(cli: URL, args: [String]) -> Bool {
    let p = Process()
    p.launchPath = cli.path
    p.arguments = args
    attachDaemonLogs(p)
    do { try p.run() } catch { return false }
    p.waitUntilExit()
    return p.terminationStatus == 0
  }

  /// Stop the collector (managed service stays installed) — Pause/Quit.
  /// Serialised on the supervise queue so it lands after any in-flight
  /// ensure rather than racing it.
  private func stopDaemon() {
    guard let cli = cli ?? locateCli() else { return }
    superviseQueue.async { _ = Self.runCli(cli: cli, args: ["_stop-daemon"]) }
  }

  // ── Live local heartbeat (fast path) ────────────────────────────

  /// Read ~/.modelstat/last-status.json directly and re-render. Runs every
  /// second from a `.common`-mode timer, so it updates the dropdown even
  /// while it's open. No subprocess, no network — just a few-KB JSON read.
  /// The file's top-level shape is the daemon's heartbeat body, which
  /// matches `LocalStatus`, so we decode it straight.
  private func tickLocal() {
    guard !paused else { return }
    spinnerTick &+= 1
    let path = ("~/.modelstat/last-status.json" as NSString).expandingTildeInPath
    if let data = FileManager.default.contents(atPath: path),
       let ls = try? JSONDecoder().decode(LocalStatus.self, from: data)
    {
      localLatest = ls
    }
    // Re-render even if the read failed so the pulse keeps moving.
    renderStats()
  }

  // ── Polling `modelstat status --json` ────────────────────────────

  private func refreshStats() {
    guard let cli else { return }
    let p = Process()
    p.launchPath = cli.path
    p.arguments = ["status", "--json"]
    let pipe = Pipe()
    p.standardOutput = pipe
    p.standardError = Pipe()
    do {
      try p.run()
    } catch {
      // Surface the failure in the menu instead of leaving the title
      // stuck on whatever it was last (e.g. "Starting…" forever). Most
      // likely cause is the binary having been moved or removed since
      // `locateCli()` last resolved it.
      statusMI.title = "status failed: \(error.localizedDescription)"
      return
    }
    // Run on a background queue so we don't block the main loop.
    DispatchQueue.global(qos: .utility).async {
      p.waitUntilExit()
      let data = pipe.fileHandleForReading.readDataToEndOfFile()
      let stats = try? JSONDecoder().decode(AgentStats.self, from: data)
      // Hand self to the main-queue closure (which mutates @MainActor state),
      // not the background closure that only does blocking IO.
      DispatchQueue.main.async { [weak self] in
        self?.latest = stats
        self?.renderStats()
      }
    }
  }

  private func renderStats() {
    // Paused: togglePaused() owns the status line ("Paused"); don't let the
    // fast tick clobber it with a stale phase from the file.
    guard !paused else { return }
    // Auto-update toggle + "Update now" read straight from the local heartbeat
    // file, so render them before the loading/paired early-returns below.
    renderUpdateItems()
    // Summariser mode is a local setting — render it regardless of pairing.
    renderSummariser()
    renderRedactor()
    guard let s = latest else {
      setInfo(statusMI, "Loading…")
      return
    }
    // Prefer the live file read (fast timer) over the network shell-out's
    // embedded copy, which can be up to 15s stale.
    let local = localLatest ?? s.local
    if s.paired == false {
      setInfo(statusMI, "Not paired — run `npx modelstat@latest`")
      for mi in [workMI, totalsMI, deviceMI] {
        setInfo(mi, "")
      }
      claimMI.title = "Open modelstat.ai"
      copyClaimMI.isHidden = true
      return
    }

    // Live agent phase comes from the local heartbeat mirror — but the
    // mirror is only as honest as the process writing it. The daemon
    // rewrites it at least every 10s even when idle, so a stale
    // written_at means the file is a fossil from a daemon that's gone:
    // say offline, don't parrot its last words. (This tray once showed
    // "Shutting down" for hours off exactly such a fossil.)
    let phase: String
    let phaseMsg: String?
    if let ls = local, Self.mirrorIsFresh(ls.written_at) {
      phase = ls.status ?? "starting"
      phaseMsg = ls.message
    } else if local != nil {
      phase = "offline"
      phaseMsg = "daemon not running"
    } else {
      phase = s.device?.daemon_status ?? "starting"
      phaseMsg = nil
    }
    // Pulse the leading dot while the agent is actively working so the
    // menu reads as alive even on the rare beat where the numbers don't
    // change. Steady dot when idle/watching/offline.
    let active = localLatest?.active ?? (localLatest?.busy_since_ms != nil)
    let dot = active ? (spinnerTick % 2 == 0 ? "●" : "○") : "●"

    // Row 1: what is happening, and the ONE number for how much is left. The
    // message is the daemon's own sentence and carries at most that one figure
    // ("2,425 session files left") — the tray does not re-derive it, and adds no
    // clock: a duration only means something beside the thing it times, and what
    // is being timed is the work in flight, which is the row below.
    if let m = phaseMsg, !m.isEmpty {
      setInfo(statusMI, "\(dot) \(phase) — \(m)")
    } else {
      setInfo(statusMI, "\(dot) \(phase)")
    }

    // Row 2: how many sessions are in flight, and how long the oldest of them
    // has been there. The row above counts FILES, which can sit on one number
    // for minutes while sessions move behind it — that is the line that read as
    // wedged. Sessions in a batch leave together, so this clock dates the oldest
    // one still running.
    if let up = local?.uploading, Self.mirrorIsFresh(local?.written_at),
      let n = up.sessions, n > 0
    {
      let noun = n == 1 ? "session" : "sessions"
      var line = "⇡ \(n) \(noun) processing"
      if let since = up.since_ms {
        let secs = Int((Date().timeIntervalSince1970 * 1000 - Double(since)) / 1000)
        if secs >= 0 { line += " · \(Self.shortDuration(secs))" }
      }
      setInfo(workMI, line)
    } else {
      setInfo(workMI, "")
    }

    if s.claimed == true {
      // Claimed device: device-view 404s for the tray (no auth) so
      // we lean entirely on the local heartbeat snapshot for live
      // numbers, plus point the menu items at the dashboard. Keep the
      // reassuring "Claimed ✓" and append the agent version when the
      // local snapshot carries it.
      if let v = local?.daemon_version, !v.isEmpty {
        setInfo(deviceMI, "Claimed ✓ · \(v)")
      } else {
        setInfo(deviceMI, "Claimed ✓ — synced to your account")
      }
      claimMI.title = "Open dashboard"
      copyClaimMI.isHidden = true
    } else {
      // Unclaimed: name the machine instead, so the claim link below has a
      // subject. Its token total is the one figure the device view adds.
      let host = s.device?.hostname ?? "unknown"
      let os = s.device?.os_family ?? ""
      setInfo(deviceMI, "\(host) · \(os)")
      claimMI.title = "Open device page"
      copyClaimMI.isHidden = (s.claim_url == nil || s.claim_url?.isEmpty == true)
    }

    // Row 3: the ONE total — how much this device has measured, ever. Tokens
    // when the server has counted them, events otherwise: the same quantity at
    // two resolutions, so it is the better one or the other, never both. Every
    // other lifetime counter (files scanned, segments sent, batches, tools,
    // accounts) is a by-product of producing this one and is not shown.
    if let tok = s.analyzed?.totalTokens, (Double(tok) ?? 0) > 0 {
      setInfo(totalsMI, "\(fmtTokens(tok)) tokens analyzed")
    } else if let events = local?.stats?.events_uploaded, events > 0 {
      setInfo(totalsMI, "\(fmtCount(events)) events analyzed")
    } else {
      setInfo(totalsMI, "")
    }
  }

  /// Reflect the daemon's auto-update setting + any pending update in the menu.
  /// Both come from ~/.modelstat/last-status.json (written by the daemon every
  /// heartbeat), so a toggle made here shows up within a second once the daemon
  /// re-reads the preference.
  private func renderUpdateItems() {
    autoUpdateMI.state = (localLatest?.auto_update ?? true) ? .on : .off
    if let upd = localLatest?.update, let verdict = upd.verdict, verdict != "ok" {
      let suffix = upd.latest.map { " (\($0))" } ?? ""
      updateMI.title =
        verdict == "upgrade_required"
        ? "Update required — update now\(suffix)" : "Update now\(suffix)"
      updateMI.isHidden = false
    } else {
      updateMI.isHidden = true
    }
  }

  /// Reflect the active summariser mode in the "Summariser" submenu: the parent
  /// title names the mode, the matching child gets a checkmark. Sourced from the
  /// 15s `status --json` poll (`latest.summarizer`). When an env override forces
  /// the mode, a switch from here wouldn't take effect, so we grey the children
  /// out and say so.
  private func renderSummariser() {
    let info = latest?.summarizer
    let mode = info?.mode
    let envLocked = info?.env_override ?? false
    summariserMI.title =
      mode.map { "Summariser: \(Self.modeLabel($0))" } ?? "Summariser"
    if envLocked { summariserMI.title += " (env-locked)" }
    modeCloudMI.state = (mode == "cloud") ? .on : .off
    modeLocalMI.state = (mode == "local") ? .on : .off
    modeSelfHostedMI.state = (mode == "self-hosted") ? .on : .off
    for mi in [modeCloudMI, modeLocalMI, modeSelfHostedMI] { mi.isEnabled = !envLocked }
  }

  /// The redactor twin of `renderSummariser`, off the same 15s poll. Hidden
  /// entirely against a daemon too old to report the setting — a submenu that
  /// can't apply is worse than none.
  private func renderRedactor() {
    guard let info = latest?.redactor else {
      redactorMI.isHidden = true
      return
    }
    redactorMI.isHidden = false
    let mode = info.mode
    let envLocked = info.env_override ?? false
    redactorMI.title = mode.map { "Redactor: \(Self.modeLabel($0))" } ?? "Redactor"
    if envLocked { redactorMI.title += " (env-locked)" }
    redactorCloudMI.state = (mode == "cloud") ? .on : .off
    redactorLocalMI.state = (mode == "local") ? .on : .off
    redactorSelfHostedMI.state = (mode == "self-hosted") ? .on : .off
    for mi in [redactorCloudMI, redactorLocalMI, redactorSelfHostedMI] {
      mi.isEnabled = !envLocked
    }
  }

  /// Human label for a summariser mode string.
  private static func modeLabel(_ mode: String) -> String {
    switch mode {
    case "cloud": return "Cloud"
    case "local": return "Local"
    case "self-hosted": return "Self-hosted"
    default: return mode
    }
  }

  @objc private func toggleAutoUpdate() {
    runManaged(["autoupdate", "toggle"])
    // Optimistic flip; the next 1s tick confirms the real state from disk.
    autoUpdateMI.state = (autoUpdateMI.state == .on) ? .off : .on
  }

  @objc private func updateNow() {
    runManaged(["upgrade"])
  }

  // ── Summariser mode switching ───────────────────────────────────
  //
  // Each switch shells out to `modelstat mode <mode>` (the CLI persists the
  // choice, re-stages the runtime, and bounces the service). Redaction runs
  // on-device in every mode; only where the summary is written changes.

  @objc private func switchModeCloud() { applyMode("cloud") }

  @objc private func switchModeLocal() {
    // Confirm the resource cost before kicking off the ~2.7 GB download — the
    // same "communicate it up front" warning the installer shows.
    let alert = NSAlert()
    alert.messageText = "Summarise on this machine?"
    alert.informativeText =
      "Local mode downloads a ~2.7 GB model and uses about 4 GB of RAM plus extra "
      + "battery and CPU while it summarises your sessions.\n\n"
      + "Redaction already runs on your machine in every mode — this only changes "
      + "where the summary is written."
    alert.alertStyle = .warning
    alert.addButton(withTitle: "Switch to Local")
    alert.addButton(withTitle: "Cancel")
    if alert.runModal() == .alertFirstButtonReturn { applyMode("local") }
  }

  @objc private func switchModeSelfHosted() {
    // Self-hosted needs a URL that can't be typed into a menu, so point the
    // user at the one CLI command that sets it. (`--model` is gone — the
    // engine is ours; only its location is theirs.)
    let cmd = "modelstat mode self-hosted --url <URL>"
    let alert = NSAlert()
    alert.messageText = "Self-hosted summarising needs an endpoint"
    alert.informativeText =
      "Point modelstat at your org's summariser engine from a terminal:\n\n    "
      + cmd + "\n\nRedaction still runs on your machine first."
    alert.addButton(withTitle: "Copy command")
    alert.addButton(withTitle: "OK")
    if alert.runModal() == .alertFirstButtonReturn {
      let pb = NSPasteboard.general
      pb.clearContents()
      pb.setString(cmd, forType: .string)
    }
  }

  // ── Redactor mode switching ─────────────────────────────────────
  //
  // Same shape as the summariser: shell out to `modelstat redactor <mode>`.
  // The secret floor runs on-device in EVERY mode — these switches only move
  // the layer-2 PII model.

  @objc private func switchRedactorCloud() {
    let alert = NSAlert()
    alert.messageText = "Detect PII on modelstat's servers?"
    alert.informativeText =
      "Secrets, emails, keys and paths are always scrubbed on this machine first. "
      + "In Cloud mode the scrubbed text is then checked for names, addresses and "
      + "other PII on modelstat's servers, which return the matches and store "
      + "nothing — the final redaction still happens here, and only redacted "
      + "turns are uploaded.\n\nNo ~900 MB on-device model needed."
    alert.addButton(withTitle: "Switch to Cloud")
    alert.addButton(withTitle: "Cancel")
    if alert.runModal() == .alertFirstButtonReturn { applyRedactor("cloud") }
  }

  @objc private func switchRedactorLocal() {
    let alert = NSAlert()
    alert.messageText = "Detect PII on this machine?"
    alert.informativeText =
      "Local mode downloads a ~900 MB model and spends this machine's CPU "
      + "checking every turn for PII — nothing, not even scrubbed text, leaves "
      + "before it is fully redacted.\n\nExpect real CPU use while a backlog "
      + "catches up."
    alert.addButton(withTitle: "Switch to Local")
    alert.addButton(withTitle: "Cancel")
    if alert.runModal() == .alertFirstButtonReturn { applyRedactor("local") }
  }

  @objc private func switchRedactorSelfHosted() {
    let cmd = "modelstat redactor self-hosted --url <URL>"
    let alert = NSAlert()
    alert.messageText = "Self-hosted redaction needs an endpoint"
    alert.informativeText =
      "Point modelstat at your org's redactor service from a terminal:\n\n    "
      + cmd + "\n\nThe secret floor still runs on this machine first."
    alert.addButton(withTitle: "Copy command")
    alert.addButton(withTitle: "OK")
    if alert.runModal() == .alertFirstButtonReturn {
      let pb = NSPasteboard.general
      pb.clearContents()
      pb.setString(cmd, forType: .string)
    }
  }

  /// Persist a redactor switch via the CLI (which bounces the daemon), then
  /// refresh shortly after so the checkmark catches up before the 15s poll.
  private func applyRedactor(_ mode: String) {
    runManaged(["redactor", mode])
    DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) { [weak self] in
      MainActor.assumeIsolated { self?.refreshStats() }
    }
  }

  /// Persist a mode switch via the CLI, then refresh shortly after so the
  /// checkmark + title catch up without waiting for the next 15s poll. The CLI
  /// persists the mode before any download, so `status --json` reports it fast.
  private func applyMode(_ mode: String) {
    runManaged(["mode", mode])
    DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) { [weak self] in
      MainActor.assumeIsolated { self?.refreshStats() }
    }
  }

  /// Fire-and-forget a `modelstat <args>` invocation (autoupdate / upgrade).
  /// Best-effort, non-blocking; output is appended to the daemon logs.
  private func runManaged(_ args: [String]) {
    guard let cli else { return }
    let p = Process()
    p.launchPath = cli.path
    p.arguments = args
    Self.attachDaemonLogs(p)
    try? p.run()
  }

  /// Whether a mirror `written_at` stamp is young enough to trust (<30s).
  /// The daemon rewrites last-status.json at least every 10s, so 30s of
  /// silence means the process behind it is gone.
  private static func mirrorIsFresh(_ writtenAt: String?) -> Bool {
    guard let writtenAt else { return false }
    let withFrac = ISO8601DateFormatter()
    withFrac.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    guard let date = withFrac.date(from: writtenAt)
        ?? ISO8601DateFormatter().date(from: writtenAt)
    else { return false }
    return Date().timeIntervalSince(date) < 30
  }

  /// Phases where the agent is doing visible work right now — drives the
  /// pulsing status dot. "watching"/"idle" are healthy-but-quiet (steady
  /// dot); "offline"/"error" are problems (steady dot, not a busy pulse).
  /// `9s`, `1m 04s`, `1h 02m` — compact enough for one menu line.
  static func shortDuration(_ secs: Int) -> String {
    if secs < 60 { return "\(secs)s" }
    if secs < 3600 { return String(format: "%dm %02ds", secs / 60, secs % 60) }
    return String(format: "%dh %02dm", secs / 3600, (secs % 3600) / 60)
  }


  private func fmtCount(_ n: Int) -> String {
    if n >= 1_000_000 { return String(format: "%.1fM", Double(n) / 1_000_000) }
    if n >= 1_000 { return String(format: "%.1fK", Double(n) / 1_000) }
    return String(n)
  }

  private func fmtTokens(_ raw: String) -> String {
    guard let n = Double(raw) else { return raw }
    if n >= 1e9 { return String(format: "%.1fB", n / 1e9) }
    if n >= 1e6 { return String(format: "%.1fM", n / 1e6) }
    if n >= 1e3 { return String(format: "%.0fK", n / 1e3) }
    return String(format: "%.0f", n)
  }

  // ── Menu actions ────────────────────────────────────────────────

  @objc private func openDashboard() {
    let url: String
    if latest?.claimed == true {
      url = "https://modelstat.ai/dashboard"
    } else if let claim = latest?.claim_url, !claim.isEmpty {
      url = claim
    } else {
      url = "https://modelstat.ai"
    }
    if let u = URL(string: url) { NSWorkspace.shared.open(u) }
  }

  @objc private func openJobs() {
    let url: String
    if latest?.claimed == true {
      url = "https://modelstat.ai/dashboard/jobs"
    } else if let claim = latest?.claim_url, !claim.isEmpty {
      url = claim + "/jobs"
    } else {
      url = "https://modelstat.ai/dashboard/jobs"
    }
    if let u = URL(string: url) { NSWorkspace.shared.open(u) }
  }

  @objc private func copyClaimUrl() {
    guard let claim = latest?.claim_url, !claim.isEmpty else { return }
    let pb = NSPasteboard.general
    pb.clearContents()
    pb.setString(claim, forType: .string)
  }

  @objc private func togglePaused() {
    paused.toggle()
    if paused {
      stopDaemon()
      pauseMI.title = "Resume"
      statusMI.title = "Paused"
    } else {
      pauseMI.title = "Pause"
      ensureDaemon()
    }
  }

  @objc private func openLogs() {
    let path = ("~/.modelstat/logs" as NSString).expandingTildeInPath
    NSWorkspace.shared.open(URL(fileURLWithPath: path))
  }

  @objc private func quit() {
    fastTimer?.invalidate()
    slowTimer?.invalidate()
    watchdogTimer?.invalidate()
    // "Quit modelstat" quits the product: stop the collector too, and only
    // terminate once `_stop-daemon` has actually run (NSApp.terminate first
    // would kill the queue before it fires).
    let cli = self.cli ?? locateCli()
    superviseQueue.async {
      if let cli { _ = Self.runCli(cli: cli, args: ["_stop-daemon"]) }
      DispatchQueue.main.async { NSApp.terminate(nil) }
    }
  }
}

// ── Single-instance guard ──────────────────────────────────────────
//
// Two copies of the tray each draw their own menu-bar icon, so the user
// sees TWO identical icons. A normal `npx modelstat@latest` install only
// launches one, but launchd KeepAlive respawns and a stray manual launch
// can briefly overlap — so we harden against it.
//
// Mechanism: ask Launch Services whether another process with our bundle
// identifier is already running (NSRunningApplication). This is host-
// agnostic — it queries the OS process table, not the filesystem, so it
// behaves identically no matter where the home directory lives (local
// disk, an NFS/SMB-mounted home, a read-only volume), whereas an flock
// lock file under ~/.modelstat silently no-ops or misbehaves on network
// filesystems. A crashed instance drops out of the process table at once,
// so a genuine restart still wins; only a live duplicate is turned away.
func anotherInstanceIsRunning() -> Bool {
  let myPid = NSRunningApplication.current.processIdentifier
  let bundleId = Bundle.main.bundleIdentifier ?? "ai.modelstat.tray"
  return NSRunningApplication
    .runningApplications(withBundleIdentifier: bundleId)
    .contains { $0.processIdentifier != myPid && !$0.isTerminated }
}

// ── App bootstrap ──────────────────────────────────────────────────
//
// LSUIElement=true in Info.plist hides the Dock icon. Without it the
// agent would bounce in the Dock on every boot, which is not what
// anyone signed up for. We set it here as a belt — the plist is the
// braces — so a malformed bundle still behaves.
//
// IMPORTANT: nothing in AppKit retains TrayController for us. The
// NSStatusItem holds the menu, and NSMenuItem.target is a weak
// reference, so the controller has no strong owners. Without a
// global anchor, ARC deallocates the controller as soon as init
// returns — which leaves the timer's `[weak self]` callback firing
// against nil and the menu title frozen on "Loading…" forever.
// The `controller` global below is the strong reference that keeps
// the controller alive for the entire app lifetime.
//
// CRITICAL: NSApplication.run() must NOT be called from inside a
// `DispatchQueue.main.async { ... }` closure. NSApplication.run()
// blocks for the lifetime of the app, and from libdispatch's
// perspective the wrapping closure is "still executing" the entire
// time — which means every other main-queue async block (including
// the stats-poll completion that updates the menu title) gets queued
// behind it and never runs. Schedule the controller setup separately
// and call app.run() directly from top-level code so libdispatch's
// main queue stays free to drain.
@MainActor
private var controller: TrayController?

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

// Single-instance guard. Run it after connecting to the window server
// (so Launch Services can enumerate us and any sibling) but before the
// controller draws an icon or spawns `modelstat start`: if another copy
// is already running, bow out quietly — no icon, no child, no dialog —
// leaving the existing instance in charge.
if anotherInstanceIsRunning() { exit(0) }

DispatchQueue.main.async {
  MainActor.assumeIsolated {
    controller = TrayController()
  }
}

app.run()
