//! macOS menu-bar tray agent (§15). The Swift tray app itself ships prebuilt in
//! the release archive (installed by `connect`, M6); this owns its launchd agent
//! (`ai.modelstat.tray`) — SEPARATE from the daemon agent so the pipeline survives
//! a tray quit — plus the pure plist body. Windows/Linux are headless (no tray):
//! the install/uninstall entry points are no-ops there.
//!
//! §15's "one tray change" (the Swift CLI-resolution candidate
//! `~/.modelstat/bin/modelstat`) is a patch to the Swift source, out of scope for
//! the Rust binaries; what the Rust side guarantees is that the binary lives at
//! that path (`_setup-runtime`) + the agent below keeps the tray running.

use std::path::PathBuf;

use crate::spec::TRAY_LABEL;

/// `~/Applications/ModelstatTray.app` — where `connect` stages the prebuilt tray.
pub fn tray_app_path() -> PathBuf {
    os_home().join("Applications/ModelstatTray.app")
}

/// The tray's inner executable the launchd agent runs. The bundle is
/// `ModelstatTray.app`, but its `CFBundleExecutable` — the file `build-app.sh`
/// copies into `Contents/MacOS/` — is `modelstat-tray`. This name MUST match
/// that, or `tray_status()` reports "not bundled" and the autostart agent is
/// never installed (the tray then never launches).
pub fn tray_binary() -> PathBuf {
    tray_app_path().join("Contents/MacOS/modelstat-tray")
}

/// `~/Library/LaunchAgents/ai.modelstat.tray.plist`.
pub fn tray_plist_path() -> PathBuf {
    os_home()
        .join("Library/LaunchAgents")
        .join(format!("{TRAY_LABEL}.plist"))
}

fn os_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The tray launchd agent plist (§15): RunAtLoad, KeepAlive on non-clean exit,
/// ThrottleInterval 10, PATH without any node dir. Pure — testable.
pub fn tray_plist_contents(
    tray_binary: &std::path::Path,
    out_log: &std::path::Path,
    err_log: &std::path::Path,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
  </dict>
</dict>
</plist>
"#,
        label = TRAY_LABEL,
        bin = tray_binary.display(),
        out = out_log.display(),
        err = err_log.display(),
    )
}

/// Whether the tray app is staged (its inner binary exists).
pub fn tray_status() -> (bool, Option<PathBuf>) {
    let app = tray_app_path();
    if tray_binary().exists() {
        (true, Some(app))
    } else {
        (false, None)
    }
}

/// Install (or refresh) the tray launchd agent so the menu-bar app autostarts.
/// `Ok(None)` when there's nothing to do (tray app not staged, or off macOS);
/// `Err` when the plist can't be written or launchd never accepts the bootstrap
/// — loud, because the silent version of that failure left the tray agent
/// missing from the domain with nothing to relaunch it.
pub fn install_tray_autostart() -> std::io::Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        if !tray_binary().exists() {
            return Ok(None);
        }
        let logs = modelstat_ingest::home_path("logs");
        std::fs::create_dir_all(&logs)?;
        let plist = tray_plist_path();
        if let Some(dir) = plist.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = tray_plist_contents(
            &tray_binary(),
            &logs.join("tray-out.log"),
            &logs.join("tray-err.log"),
        );
        std::fs::write(&plist, body)?;
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        crate::launchd_reload(&domain, TRAY_LABEL, &plist)?;
        Ok(Some(plist))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}

/// Restart the running tray so a freshly swapped bundle is what the user is
/// looking at. Without this a self-update replaces the binary on disk while the
/// OLD process keeps running until the next login — the tray on screen was four
/// days behind its own build, which is indistinguishable from a tray fix that
/// never shipped.
///
/// `kickstart -k` alone is NOT enough, which took a live machine to notice. The
/// tray is single-instance by design (it asks Launch Services whether another copy
/// of its bundle is running and exits if so), and `-k` only kills the process the
/// JOB owns. An older install can leave an ORPHAN behind — observed here at PPID
/// 1, four days old, outliving the job that started it — and every relaunch then
/// politely defers to that orphan. The bundle updates, the menu bar never does.
///
/// So: kill anything running OUR tray binary by its exact path, then let launchd
/// bring it back. Precise (the path is this install's bundle, nobody else's) and
/// safe (a menu-bar app relaunching is invisible). A machine with no tray has
/// nothing to match and no agent to kickstart, so this stays a no-op there.
///
/// Silent by design: the update itself already succeeded, and a failure here costs
/// a stale menu bar until the next login, not data.
pub fn restart_tray() {
    #[cfg(target_os = "macos")]
    {
        let bin = tray_binary();
        if !bin.exists() {
            return;
        }
        let _ = crate::run("pkill", &["-f", &bin.to_string_lossy()]);
        let uid = unsafe { libc::getuid() };
        let target = format!("gui/{uid}/{TRAY_LABEL}");
        let _ = crate::run("launchctl", &["kickstart", "-k", &target]);
    }
}

/// Remove the tray launchd agent. No-op off macOS.
pub fn uninstall_tray_autostart() {
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        let target = format!("gui/{uid}/{TRAY_LABEL}");
        let _ = crate::run("launchctl", &["bootout", &target]);
        let plist = tray_plist_path();
        if plist.exists() {
            let _ = std::fs::remove_file(&plist);
        }
    }
}

/// Reconcile the tray agent on every `_install-service`: install it when the app
/// is staged, leave things alone otherwise. `Ok(true)` = agent active,
/// `Ok(false)` = nothing to do, `Err` = the install failed (loud). No-op off
/// macOS.
pub fn ensure_tray_installed() -> std::io::Result<bool> {
    install_tray_autostart().map(|p| p.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn tray_plist_runs_the_inner_binary_with_keepalive_no_node() {
        let body = tray_plist_contents(
            Path::new("/Users/x/Applications/ModelstatTray.app/Contents/MacOS/modelstat-tray"),
            Path::new("/Users/x/.modelstat/logs/tray-out.log"),
            Path::new("/Users/x/.modelstat/logs/tray-err.log"),
        );
        assert!(body.contains("<key>Label</key><string>ai.modelstat.tray</string>"));
        assert!(body.contains("Contents/MacOS/modelstat-tray</string>"));
        assert!(body.contains("<key>ThrottleInterval</key><integer>10</integer>"));
        assert!(body.contains("<key>SuccessfulExit</key><false/>"));
        assert!(!body.contains("node"), "{body}");
        assert!(body.contains("tray-out.log"));
    }

    #[test]
    fn tray_paths_are_under_the_os_home() {
        // (Only asserts the shape — HOME resolution is environment-dependent, and
        // the separator differs by host, so normalize to `/` before matching.)
        let plist = tray_plist_path().to_string_lossy().replace('\\', "/");
        assert!(plist.ends_with("Library/LaunchAgents/ai.modelstat.tray.plist"));
        let app = tray_app_path().to_string_lossy().replace('\\', "/");
        assert!(app.ends_with("Applications/ModelstatTray.app"));
        // The inner executable name MUST equal build-app.sh's CFBundleExecutable
        // (`modelstat-tray`). A mismatch makes `tray_status()` report "not
        // bundled", so the autostart agent is never installed and the tray never
        // launches — the daemon-1.0.3 regression this guard exists to catch.
        let bin = tray_binary().to_string_lossy().replace('\\', "/");
        assert!(
            bin.ends_with("ModelstatTray.app/Contents/MacOS/modelstat-tray"),
            "{bin}"
        );
    }
}
