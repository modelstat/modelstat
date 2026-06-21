// ModelstatTray — macOS menu-bar daemon for the modelstat agent.
//
// What it does:
//   · puts a "◉" status item in the menu bar
//   · runs `modelstat start` as a child process so the pipeline is live
//   · polls `modelstat stats --json` every 5 s to refresh the dropdown
//   · offers: Open dashboard, Copy claim URL, View pipeline, Pause/Resume,
//     Quit
//
// Spawning the CLI is deliberate: the CLI already owns discover/scan/
// watch + IngestClient retry semantics, and the tray never needs to
// touch ingestion or auth. Keeps this binary a thin shell — one file,
// ~300 LOC, no dependencies beyond AppKit/Foundation.

import AppKit
import Darwin
import Foundation

// ── Resolve the `modelstat` CLI on $PATH, then at the install-path
//    the agent-dev installer writes into (~/.modelstat/bin/modelstat.mjs)
//    so we don't rely on shell profiles loading in launchd's env.
func locateCli() -> URL? {
  let fm = FileManager.default
  let home = NSHomeDirectory()
  let candidates = [
    "\(home)/.modelstat/bin/modelstat.mjs",
    "/opt/homebrew/bin/modelstat",
    "/usr/local/bin/modelstat",
    "/usr/bin/modelstat",
  ]
  for p in candidates {
    if fm.isExecutableFile(atPath: p) { return URL(fileURLWithPath: p) }
  }
  // Last-ditch: `which modelstat` via a login shell so PATH lookups
  // honour the user's zsh/bash config.
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
}

struct DeviceInfo: Decodable {
  let hostname: String?
  let os_family: String?
  let daemon_status: String?
  let last_seen_at: String?
}

struct AnalyzedInfo: Decodable {
  let count: Int?
  /// `processing` = sessions with at least one un-classified segment;
  /// `finished` = every segment classified. From device-view endpoint.
  let processing: Int?
  let finished: Int?
  let totalTokens: String?
  let totalCostUsd: Double?
}

struct LocalStatus: Decodable {
  let status: String?
  let message: String?
  let queue_size: Int?
  let last_event_at: String?
  let daemon_version: String?
  let stats: LocalStatsCounters?
  /// Server release verdict (the daemon sets this from the heartbeat response).
  let update: UpdateInfo?
  /// Effective auto-update setting — drives the tray's checkbox.
  let auto_update: Bool?
}

struct UpdateInfo: Decodable {
  /// "ok" | "update_available" | "upgrade_required".
  let verdict: String?
  /// Latest published version, when known.
  let latest: String?
}

struct LocalStatsCounters: Decodable {
  let installations_detected: Int?
  let identities_detected: Int?
  let files_scanned: Int?
  let files_unchanged: Int?
  let events_uploaded: Int?
  let batches_uploaded: Int?
  /// Lifetime count of cognition segments uploaded from this machine
  /// (persisted by the daemon, so it survives restarts).
  let segments_sent: Int?
  /// Segments in the batch being uploaded right now — a gauge that
  /// drops back to 0 between bursts. Usually 0 when idle.
  let segments_sending: Int?
}

@MainActor
final class TrayController: NSObject {
  private let statusItem: NSStatusItem
  private let menu = NSMenu()
  /// Re-resolved by ensureDaemon() when nil — a transient resolution
  /// failure at tray boot (e.g. the installer is mid-rewrite of
  /// ~/.modelstat/bin) must not permanently strand the daemon.
  private var cli: URL?
  private var daemon: Process?
  /// When the current `daemon` child was spawned — used to detect
  /// "exited cleanly almost immediately" (another daemon owns the
  /// lock) so we back off to the watchdog instead of respawning hot.
  private var daemonSpawnedAt: Date?
  private var paused = false
  /// Serialises _daemon-health probes; collapses overlapping
  /// ensureDaemon() calls into one in-flight probe.
  private let superviseQueue = DispatchQueue(label: "ai.modelstat.tray.supervise", qos: .utility)
  private var ensureInFlight = false
  private var latest: AgentStats?
  /// Live local heartbeat, read straight from ~/.modelstat/last-status.json
  /// on the fast timer. Decoupled from `latest` (the slower, network-backed
  /// `stats --json` shell-out) so the menu's numbers move every second.
  private var localLatest: LocalStatus?
  /// Advances once per fast tick to drive the "alive" pulse on the status
  /// line — a cheap, honest signal that the agent is doing work right now.
  private var spinnerTick = 0
  private var fastTimer: Timer?
  private var slowTimer: Timer?
  private var watchdogTimer: Timer?

  // Menu items we update on every poll
  private let statusMI = NSMenuItem(title: "Loading…", action: nil, keyEquivalent: "")
  private let deviceMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  private let analyzedMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  /// Pipeline activity — sessions processing/finished + events uploaded.
  private let pipelineMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  /// What the agent has discovered on this machine — installations + identities.
  private let detectedMI = NSMenuItem(title: "", action: nil, keyEquivalent: "")
  private let claimMI = NSMenuItem(title: "Open device page", action: #selector(openDashboard), keyEquivalent: "o")
  private let copyClaimMI = NSMenuItem(title: "Copy claim URL", action: #selector(copyClaimUrl), keyEquivalent: "c")
  private let jobsMI = NSMenuItem(title: "View pipeline…", action: #selector(openJobs), keyEquivalent: "j")
  private let pauseMI = NSMenuItem(title: "Pause", action: #selector(togglePaused), keyEquivalent: "p")
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
    //   · slow (15s): shell out to `modelstat stats --json` for the
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

  /// The five non-clickable info rows at the top of the menu, in order.
  private var infoItems: [NSMenuItem] {
    [statusMI, deviceMI, analyzedMI, pipelineMI, detectedMI]
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
  // The tray no longer spawns `start --force` blindly. Blind --force
  // SIGTERMs whatever live daemon owns the singleton lock (see
  // apps/daemon/src/lock.ts), so two briefly-coexisting trays
  // (kickstart -k racing a reinstall, KeepAlive respawn overlap) had
  // their daemons kill each other in a loop — observed 2026-06-12
  // ending with zero daemons and nothing restarting them. Instead,
  // every (re)start funnels through ensureDaemon(), which asks the CLI
  // `_daemon-health` (decision logic + tests live in
  // apps/daemon/src/supervise.ts):
  //   adopt   → a live, heartbeating daemon owns the lock — leave it.
  //   spawn   → no live owner — plain `start` (a dead owner's stale
  //             lock is reclaimed without --force by lock.ts).
  //   replace → live owner that stopped heartbeating — `start --force`.
  // A 30s watchdog re-runs ensureDaemon() so one-shot failure modes
  // (CLI unresolvable at boot, Process.run() throw, adopted daemon
  // dying with no terminationHandler) heal on the next tick instead of
  // stranding the pipeline forever.

  /// Converge toward "exactly one live daemon". Safe to call from any
  /// trigger (boot, watchdog, child exit, resume) — overlapping calls
  /// collapse into one in-flight health probe.
  private func ensureDaemon() {
    guard !paused else { return }
    if let d = daemon, d.isRunning { return }
    if cli == nil { cli = locateCli() }
    guard let cli else {
      statusMI.title = "modelstat CLI not found — retrying…"
      return
    }
    guard !ensureInFlight else { return }
    ensureInFlight = true
    superviseQueue.async {
      // Probe off-main: the health command boots node (~100-300ms).
      let decision = Self.queryDaemonHealth(cli: cli) ?? "spawn"
      // Capture self on the closure that actually hops back to @MainActor,
      // not the supervise-queue closure (which only touches statics) — a weak
      // *var* captured across the actor hop is a data race the CI toolchain
      // rejects outright.
      DispatchQueue.main.async { [weak self] in
        MainActor.assumeIsolated {
          guard let self else { return }
          self.ensureInFlight = false
          guard !self.paused else { return }
          if let d = self.daemon, d.isRunning { return }
          switch decision {
          case "adopt":
            // A healthy daemon someone else spawned. Adopt: render its
            // heartbeat (tickLocal already does) and spawn nothing.
            break
          case "replace":
            self.spawnDaemon(cli: cli, force: true)
          default:
            self.spawnDaemon(cli: cli, force: false)
          }
        }
      }
    }
  }

  /// Run `modelstat _daemon-health` and return its decision, or nil if
  /// the command failed (older CLI, node missing) — caller treats nil
  /// as "spawn", which is safe: an unforced spawn against a healthy
  /// owner exits 0 in <1s without killing anything.
  private nonisolated static func queryDaemonHealth(cli: URL) -> String? {
    let p = Process()
    if cli.pathExtension == "mjs" {
      p.launchPath = "/usr/bin/env"
      p.arguments = ["node", cli.path, "_daemon-health"]
    } else {
      p.launchPath = cli.path
      p.arguments = ["_daemon-health"]
    }
    let pipe = Pipe()
    p.standardOutput = pipe
    p.standardError = Pipe()
    do { try p.run() } catch { return nil }
    p.waitUntilExit()
    guard p.terminationStatus == 0 else { return nil }
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let decision = obj["decision"] as? String else { return nil }
    return decision
  }

  private func spawnDaemon(cli: URL, force: Bool) {
    let p = Process()
    var args = ["start"]
    if force { args.append("--force") }
    if cli.pathExtension == "mjs" {
      p.launchPath = "/usr/bin/env"
      p.arguments = ["node", cli.path] + args
    } else {
      p.launchPath = cli.path
      p.arguments = args
    }
    // Bolt stdout/stderr onto the same log the launchd plist uses so
    // `modelstat status` still sees the same tail.
    let logsDir = ("~/.modelstat/logs" as NSString).expandingTildeInPath
    try? FileManager.default.createDirectory(atPath: logsDir, withIntermediateDirectories: true)
    let out = FileHandle(forWritingAtPath: "\(logsDir)/out.log") ?? FileHandle.standardOutput
    let err = FileHandle(forWritingAtPath: "\(logsDir)/err.log") ?? FileHandle.standardError
    p.standardOutput = out
    p.standardError = err
    p.terminationHandler = { proc in
      // Daemon exited — re-converge via the health check, which adopts
      // a replacement daemon instead of counter-killing it. A clean
      // sub-5s exit means "another daemon owns the lock" (or an equally
      // immediate no-op); skip the hot retry and let the 30s watchdog
      // re-check, so a stale CLI can't put us in a 2s spawn loop.
      let status = proc.terminationStatus
      // Capture weak self on the @MainActor Task, not the (non-isolated)
      // terminationHandler — capturing the outer weak var across the actor
      // boundary is a data race the CI toolchain rejects.
      Task { @MainActor [weak self] in
        guard let self else { return }
        let uptime = self.daemonSpawnedAt.map { Date().timeIntervalSince($0) } ?? .infinity
        self.daemon = nil
        self.daemonSpawnedAt = nil
        guard !self.paused else { return }
        if status == 0 && uptime < 5 { return } // watchdog will re-ensure
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) { [weak self] in
          MainActor.assumeIsolated { self?.ensureDaemon() }
        }
      }
    }
    do {
      try p.run()
      daemon = p
      daemonSpawnedAt = Date()
    } catch {
      // Watchdog retries in ≤30s — do NOT give up permanently here.
      statusMI.title = "modelstat start failed (retrying): \(error.localizedDescription)"
    }
  }

  private func stopDaemon() {
    if let d = daemon {
      d.terminate()
      daemon = nil
      daemonSpawnedAt = nil
      return
    }
    // No child of our own — but Pause/Quit must also stop an ADOPTED
    // daemon (one we found healthy and left alone). SIGTERM the lock
    // owner; harmless no-op if it's already gone.
    if let pid = Self.readLockOwnerPid() {
      kill(pid, SIGTERM)
    }
  }

  /// pid from ~/.modelstat/daemon.lock, for stopping an adopted daemon.
  private nonisolated static func readLockOwnerPid() -> pid_t? {
    let path = ("~/.modelstat/daemon.lock" as NSString).expandingTildeInPath
    guard let data = FileManager.default.contents(atPath: path),
          let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let pid = obj["pid"] as? Int, pid > 0
    else { return nil }
    return pid_t(pid)
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

  // ── Polling `modelstat stats --json` ────────────────────────────

  private func refreshStats() {
    guard let cli else { return }
    let p = Process()
    if cli.pathExtension == "mjs" {
      p.launchPath = "/usr/bin/env"
      p.arguments = ["node", cli.path, "stats", "--json"]
    } else {
      p.launchPath = cli.path
      p.arguments = ["stats", "--json"]
    }
    let pipe = Pipe()
    p.standardOutput = pipe
    p.standardError = Pipe()
    do {
      try p.run()
    } catch {
      // Surface the failure in the menu instead of leaving the title
      // stuck on whatever it was last (e.g. "Starting…" forever). Most
      // likely cause is `node` not being on the launchd-inherited PATH.
      statusMI.title = "stats failed: \(error.localizedDescription)"
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
    guard let s = latest else {
      setInfo(statusMI, "Loading…")
      return
    }
    // Prefer the live file read (fast timer) over the network shell-out's
    // embedded copy, which can be up to 15s stale.
    let local = localLatest ?? s.local
    if s.paired == false {
      setInfo(statusMI, "Not paired — run `npx modelstat@latest`")
      for mi in [deviceMI, analyzedMI, pipelineMI, detectedMI] { setInfo(mi, "") }
      claimMI.title = "Open modelstat.ai"
      copyClaimMI.isHidden = true
      return
    }

    // Live agent phase comes from the local heartbeat mirror.
    // Falls back to the device-view's reported daemon_status. If
    // both are missing we say "running" rather than "starting" so
    // the menu doesn't lie about the daemon's state.
    let phase = local?.status ?? s.device?.daemon_status ?? "running"
    let phaseMsg = local?.message
    // Pulse the leading dot while the agent is actively working so the
    // menu reads as alive even on the rare beat where the numbers don't
    // change. Steady dot when idle/watching/offline.
    let dot = isActivePhase(phase) ? (spinnerTick % 2 == 0 ? "●" : "○") : "●"
    if let m = phaseMsg, !m.isEmpty {
      setInfo(statusMI, "\(dot) \(phase) — \(m)")
    } else {
      setInfo(statusMI, "\(dot) \(phase)")
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
      // Unclaimed: device-view fills in the rich numbers.
      let host = s.device?.hostname ?? "unknown"
      let os = s.device?.os_family ?? ""
      setInfo(deviceMI, "\(host) · \(os)")
      claimMI.title = "Open device page"
      copyClaimMI.isHidden = (s.claim_url == nil || s.claim_url?.isEmpty == true)
    }

    // Sessions / tokens / cost (only available for unclaimed since
    // the device-view exposes them). Claimed devices show pipeline
    // + detected counts instead — see below.
    if let a = s.analyzed {
      let tok = a.totalTokens ?? "0"
      let cnt = a.count ?? 0
      let usd = String(format: "%.2f", a.totalCostUsd ?? 0.0)
      let proc = a.processing ?? 0
      let done = a.finished ?? cnt
      let breakdown = proc > 0 ? " (\(done) finished · \(proc) processing)" : ""
      setInfo(analyzedMI, "\(cnt) sessions\(breakdown) · \(fmtTokens(tok)) tokens · $\(usd)")
    } else {
      setInfo(analyzedMI, "")
    }

    // Pipeline activity — segments are the headline (what the user
    // asked to see): how many are uploading right now, and how many
    // have been sent in total. Events / files trail as context. All
    // sourced from the local heartbeat mirror so it works for both
    // claimed and unclaimed devices.
    if let c = local?.stats {
      let sending = c.segments_sending ?? 0
      let sent = c.segments_sent ?? 0
      let events = c.events_uploaded ?? 0
      let scanned = c.files_scanned ?? 0
      let queue = local?.queue_size ?? 0
      var bits: [String] = []
      if sending > 0 { bits.append("↑ \(sending) sending") }
      if sent > 0 { bits.append("\(fmtCount(sent)) segments sent") }
      if events > 0 { bits.append("\(fmtCount(events)) events") }
      if scanned > 0 { bits.append("\(scanned) files") }
      if queue > 0 { bits.append("\(queue) in queue") }
      setInfo(pipelineMI, bits.joined(separator: " · "))
    } else {
      setInfo(pipelineMI, "")
    }

    // What the agent found on this machine — installations +
    // identities (Claude Keychain, Codex JWT, …). Mirror of the
    // discover() output the daemon ran at startup.
    if let c = local?.stats {
      let installs = c.installations_detected ?? 0
      let ids = c.identities_detected ?? 0
      if installs > 0 || ids > 0 {
        setInfo(detectedMI, "\(installs) tools · \(ids) accounts detected")
      } else {
        setInfo(detectedMI, "")
      }
    } else {
      setInfo(detectedMI, "")
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

  @objc private func toggleAutoUpdate() {
    runManaged(["autoupdate", "toggle"])
    // Optimistic flip; the next 1s tick confirms the real state from disk.
    autoUpdateMI.state = (autoUpdateMI.state == .on) ? .off : .on
  }

  @objc private func updateNow() {
    runManaged(["upgrade"])
  }

  /// Fire-and-forget a `modelstat <args>` invocation (autoupdate / upgrade).
  /// Best-effort, non-blocking; output is appended to the daemon log.
  private func runManaged(_ args: [String]) {
    guard let cli else { return }
    let p = Process()
    if cli.pathExtension == "mjs" {
      p.launchPath = "/usr/bin/env"
      p.arguments = ["node", cli.path] + args
    } else {
      p.launchPath = cli.path
      p.arguments = args
    }
    let logsDir = ("~/.modelstat/logs" as NSString).expandingTildeInPath
    let out = FileHandle(forWritingAtPath: "\(logsDir)/out.log") ?? FileHandle.standardOutput
    p.standardOutput = out
    p.standardError = out
    try? p.run()
  }

  /// Phases where the agent is doing visible work right now — drives the
  /// pulsing status dot. "watching"/"idle" are healthy-but-quiet (steady
  /// dot); "offline"/"error" are problems (steady dot, not a busy pulse).
  private func isActivePhase(_ phase: String) -> Bool {
    switch phase {
    case "starting", "discovering", "scanning", "processing", "uploading":
      return true
    default:
      return false
    }
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
    stopDaemon()
    fastTimer?.invalidate()
    slowTimer?.invalidate()
    watchdogTimer?.invalidate()
    NSApp.terminate(nil)
  }
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

DispatchQueue.main.async {
  MainActor.assumeIsolated {
    controller = TrayController()
  }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
app.run()
