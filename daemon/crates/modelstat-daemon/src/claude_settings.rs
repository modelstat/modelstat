//! Claude Code `settings.json` integration (§14) — installs/removes the
//! `modelstat statusline` entry so it renders at the bottom of every turn. A port
//! of `apps/daemon/src/claude-settings.ts`.
//!
//! Idempotent + reversible: writing twice is a no-op once ours is in place; a
//! pre-existing FOREIGN statusLine is stashed under `_modelstatPrevStatusLine`
//! (not clobbered) and restored on remove. Best-effort — a parse/write failure is
//! reported to the caller but never aborts an install (ingest is the core duty).
//!
//! §14 CHANGE from the TS: the registered command is the ABSOLUTE path to the
//! binary (`<abs> statusline`), so PATH isn't required under launchd/Windows.
//! Detection still matches the `modelstat statusline` substring, which an
//! absolute path (`…/modelstat statusline`) contains — so it recognises both the
//! new absolute form and any legacy bare `modelstat statusline`.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The substring that marks a statusLine as ours (detection is tolerant of a
/// user-tweaked padding + of bare-vs-absolute command forms).
pub const STATUSLINE_MARKER: &str = "modelstat statusline";

/// Path to Claude Code's user settings. `CLAUDE_CONFIG_DIR` overrides the default
/// `~/.claude` (mirrors Claude Code's own resolution), so a custom config dir or
/// a test home is honoured.
pub fn claude_settings_path() -> PathBuf {
    if let Ok(base) = std::env::var("CLAUDE_CONFIG_DIR") {
        let base = base.trim();
        if !base.is_empty() {
            return Path::new(base).join("settings.json");
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("settings.json")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

/// The command string we register for a given absolute binary path.
pub fn statusline_command(exe_path: &str) -> String {
    format!("{exe_path} {}", "statusline")
}

/// True when this statusLine setting is modelstat's — its `command` contains the
/// [`STATUSLINE_MARKER`] substring.
fn is_modelstat_statusline(status_line: Option<&Value>) -> bool {
    status_line
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
        .map(|c| c.contains(STATUSLINE_MARKER))
        .unwrap_or(false)
}

/// Parse the settings file, tolerating absence + emptiness (→ `{}`) but surfacing
/// a real parse error / non-object to the caller (so we never silently overwrite
/// a file we couldn't understand).
fn read_settings(path: &Path) -> Result<Map<String, Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(format!("{} is not a JSON object", path.display())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Atomically write settings (pretty-printed, trailing newline) via tmp+rename so
/// a crash mid-write can't truncate the config. Backs the previous file up to
/// `<path>.modelstat.bak` the first time we touch it.
fn write_settings(
    path: &Path,
    settings: &Map<String, Value>,
    had_file: bool,
) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    if had_file {
        // Best-effort backup — don't block the write on it.
        let bak = PathBuf::from(format!("{}.modelstat.bak", path.display()));
        let _ = std::fs::rename(path, bak);
    }
    let tmp = PathBuf::from(format!("{}.modelstat.tmp", path.display()));
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(settings.clone())).map_err(|e| e.to_string())?
    );
    std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The outcome of [`install_statusline_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallResult {
    /// Registered ours (`preserved` = a foreign statusLine was stashed for restore).
    Installed { preserved: bool },
    /// Ours was already in place — no write.
    Already,
    /// A parse/write error (the file was left untouched on a parse error).
    Error(String),
}

/// Ensure `<command>` is the registered statusLine at `path`. Idempotent. A
/// foreign statusLine is stashed under `_modelstatPrevStatusLine` (restored on
/// remove) rather than dropped. Pure core (path + command injected) so it's
/// testable against a temp file.
pub fn install_statusline_at(path: &Path, command: &str) -> InstallResult {
    let mut settings = match read_settings(path) {
        Ok(s) => s,
        Err(e) => return InstallResult::Error(e),
    };
    let had_file = std::fs::metadata(path).is_ok();

    if is_modelstat_statusline(settings.get("statusLine")) {
        return InstallResult::Already;
    }

    let mut preserved = false;
    // A foreign statusLine (one with a command) is stashed, not lost.
    let foreign = settings
        .get("statusLine")
        .filter(|s| s.get("command").and_then(Value::as_str).is_some())
        .cloned();
    if let Some(prev) = foreign {
        settings.insert("_modelstatPrevStatusLine".to_string(), prev);
        preserved = true;
    }
    settings.insert(
        "statusLine".to_string(),
        json!({ "type": "command", "command": command, "padding": 0 }),
    );

    match write_settings(path, &settings, had_file) {
        Ok(()) => InstallResult::Installed { preserved },
        Err(e) => InstallResult::Error(e),
    }
}

/// Install using the real settings path + the given absolute binary path.
pub fn install_statusline(exe_path: &str) -> InstallResult {
    install_statusline_at(&claude_settings_path(), &statusline_command(exe_path))
}

/// The outcome of [`remove_statusline_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveResult {
    /// Removed ours (`restored` = a stashed foreign statusLine was put back).
    Removed {
        restored: bool,
    },
    /// Nothing of ours to remove.
    Absent,
    Error(String),
}

/// Remove modelstat's statusLine at `path`, restoring a stashed foreign one if
/// present. A non-modelstat statusLine is left untouched (we never installed over
/// it) — only a dangling stash is cleaned up.
pub fn remove_statusline_at(path: &Path) -> RemoveResult {
    let mut settings = match read_settings(path) {
        Ok(s) => s,
        Err(e) => return RemoveResult::Error(e),
    };

    if !is_modelstat_statusline(settings.get("statusLine")) {
        // Not ours — leave the statusLine be, but clean up a dangling stash.
        if settings.contains_key("_modelstatPrevStatusLine") {
            settings.remove("_modelstatPrevStatusLine");
            if let Err(e) = write_settings(path, &settings, true) {
                return RemoveResult::Error(e);
            }
        }
        return RemoveResult::Absent;
    }

    let mut restored = false;
    let prev = settings.remove("_modelstatPrevStatusLine");
    match prev.filter(|p| p.get("command").and_then(Value::as_str).is_some()) {
        Some(p) => {
            settings.insert("statusLine".to_string(), p);
            restored = true;
        }
        None => {
            settings.remove("statusLine");
        }
    }

    match write_settings(path, &settings, true) {
        Ok(()) => RemoveResult::Removed { restored },
        Err(e) => RemoveResult::Error(e),
    }
}

/// Remove using the real settings path.
pub fn remove_statusline() -> RemoveResult {
    remove_statusline_at(&claude_settings_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("modelstat-clset-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("settings.json")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    const CMD: &str = "/Users/x/.modelstat/bin/modelstat statusline";

    #[test]
    fn install_into_a_fresh_home_writes_ours() {
        let p = tmp("fresh");
        let r = install_statusline_at(&p, CMD);
        assert_eq!(r, InstallResult::Installed { preserved: false });
        let v = read(&p);
        assert_eq!(v["statusLine"]["command"], CMD);
        assert_eq!(v["statusLine"]["type"], "command");
        assert_eq!(v["statusLine"]["padding"], 0);
        // Trailing newline + no leftover tmp.
        assert!(std::fs::read_to_string(&p).unwrap().ends_with("}\n"));
        assert!(!p.with_extension("json.modelstat.tmp").exists());
    }

    #[test]
    fn reinstall_is_a_noop_and_absolute_command_is_detected_as_ours() {
        let p = tmp("reinstall");
        install_statusline_at(&p, CMD);
        assert_eq!(install_statusline_at(&p, CMD), InstallResult::Already);
    }

    #[test]
    fn a_foreign_statusline_is_stashed_and_restored() {
        let p = tmp("foreign");
        std::fs::write(
            &p,
            r#"{"statusLine":{"type":"command","command":"other-tool render"},"model":"opus"}"#,
        )
        .unwrap();
        let r = install_statusline_at(&p, CMD);
        assert_eq!(r, InstallResult::Installed { preserved: true });
        let v = read(&p);
        assert_eq!(v["statusLine"]["command"], CMD);
        assert_eq!(
            v["_modelstatPrevStatusLine"]["command"],
            "other-tool render"
        );
        // An unrelated key survives; a .bak of the original was written.
        assert_eq!(v["model"], "opus");
        assert!(p.with_extension("json.modelstat.bak").exists());

        // Remove restores the foreign one + drops the stash.
        let rr = remove_statusline_at(&p);
        assert_eq!(rr, RemoveResult::Removed { restored: true });
        let v = read(&p);
        assert_eq!(v["statusLine"]["command"], "other-tool render");
        assert!(v.get("_modelstatPrevStatusLine").is_none());
    }

    #[test]
    fn remove_ours_with_no_stash_drops_the_statusline() {
        let p = tmp("removeours");
        install_statusline_at(&p, CMD);
        let rr = remove_statusline_at(&p);
        assert_eq!(rr, RemoveResult::Removed { restored: false });
        assert!(read(&p).get("statusLine").is_none());
    }

    #[test]
    fn remove_leaves_a_foreign_statusline_untouched() {
        let p = tmp("foreignremove");
        std::fs::write(&p, r#"{"statusLine":{"command":"other render"}}"#).unwrap();
        assert_eq!(remove_statusline_at(&p), RemoveResult::Absent);
        assert_eq!(read(&p)["statusLine"]["command"], "other render");
    }

    #[test]
    fn malformed_json_errors_without_overwriting() {
        let p = tmp("malformed");
        std::fs::write(&p, "{ this is not json").unwrap();
        assert!(matches!(
            install_statusline_at(&p, CMD),
            InstallResult::Error(_)
        ));
        // The bad file is left exactly as-is (we never overwrite what we can't parse).
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ this is not json");
    }

    #[test]
    fn a_json_array_is_rejected_as_not_an_object() {
        let p = tmp("array");
        std::fs::write(&p, "[1,2,3]").unwrap();
        assert!(matches!(
            install_statusline_at(&p, CMD),
            InstallResult::Error(_)
        ));
    }
}
