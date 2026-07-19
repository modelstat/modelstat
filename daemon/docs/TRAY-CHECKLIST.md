# Tray manual sign-off checklist (M5)

The menu-bar app is a GUI — no automated test can click it. This is the one-time
manual pass that closes the M5 tray acceptance criterion. Run it once on a Mac with
the Rust daemon installed (the tray ships inside the macOS archive as
`ModelstatTray.app`). Tick each box; note anything that fails.

Handy throughout: `~/.modelstat/bin/modelstat status` (the daemon's own view).

---

## 0. Launch
- [ ] Open `ModelstatTray.app` (or it autostarts after install). A menu-bar icon appears.
- [ ] The dropdown populates and refreshes (~every 5 s): pairing/device line, analyzed
      counts, pipeline, detected installs.

## 1. Spawn — starts the daemon when none is running
- Stop the daemon: `~/.modelstat/bin/modelstat stop`.
- [ ] Within a few seconds the tray restarts it on its own (`ensureDaemon`), and
      `modelstat status` shows the service **running** again.

## 2. Adopt — never murders a healthy daemon
- With the daemon running, quit and relaunch the tray.
- [ ] The running daemon is **adopted**, not killed/relaunched — it stays up steadily
      (no rapid respawn loop; status stays "running" the whole time).

## 3. Pause / Resume
- Click **Pause** (⌘P).
- [ ] Indexing stops; the item flips to **Resume**; `modelstat status` reflects paused.
- Click **Resume**.
- [ ] Indexing resumes.

## 4. Mode switch (Summariser submenu)
- Open **Summariser**.
- [ ] It lists **Cloud — modelstat's servers**, **Local — on this machine…**,
      **Self-hosted…**, with the current mode checked.
- Switch **Cloud → Local**, then back to **Cloud**.
- [ ] `modelstat mode` reflects each change. (Local is **beta** — expect the ⚠ note;
      it downloads the ~2.7 GB model on first use, so only pick it if you want to test that.)

## 5. Update nudge  *(best-effort — needs an actual available update)*
- When the server reports an update (status `update_available` / `upgrade_required`):
- [ ] **Update now** (⌘U) appears, and the **Auto-update** checkbox reflects the stored
      setting (toggling it flips `modelstat autoupdate`).
- If you can't stage a newer release, skip this — the update *decision* logic is
  unit-tested (`maybe_auto_update_decision_matrix`); this box is only the visual surfacing.

## 6. Replace on version mismatch  *(best-effort)*
- If a mismatched daemon version is ever running, the tray replaces it with the matching
  build via the lock.
- Hard to stage by hand; skip unless you happen to have a mismatched binary. The
  lock-based adopt/replace is unit-tested in the daemon (`lock.rs`).

## 7. Quit
- [ ] **Quit** exits the tray cleanly; the daemon keeps running as its own launchd
      service (check `modelstat status`).

---

**Done?** If every non-optional box is ticked, the M5 tray criterion is signed off.
Report any box that failed with what you saw.
