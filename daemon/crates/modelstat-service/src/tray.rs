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
pub fn tray_plist_contents(tray_binary: &std::path::Path, out_log: &std::path::Path, err_log: &std::path::Path) -> String {
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
/// No-op (returns `None`) when the tray app isn't staged, or off macOS.
pub fn install_tray_autostart() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if !tray_binary().exists() {
            return None;
        }
        let logs = modelstat_ingest::home_path("logs");
        let _ = std::fs::create_dir_all(&logs);
        let plist = tray_plist_path();
        if let Some(dir) = plist.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = tray_plist_contents(
            &tray_binary(),
            &logs.join("tray-out.log"),
            &logs.join("tray-err.log"),
        );
        if std::fs::write(&plist, body).is_err() {
            return None;
        }
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let target = format!("{domain}/{TRAY_LABEL}");
        let _ = crate::run("launchctl", &["bootout", &target]);
        let _ = crate::run("launchctl", &["bootstrap", &domain, &plist.to_string_lossy()]);
        let _ = crate::run("launchctl", &["kickstart", "-k", &target]);
        Some(plist)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
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
/// is staged, leave things alone otherwise. Returns whether the agent is active.
/// No-op off macOS.
pub fn ensure_tray_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        install_tray_autostart().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
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
