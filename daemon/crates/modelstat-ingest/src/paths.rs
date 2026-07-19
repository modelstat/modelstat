//! The daemon's single home directory — a byte-for-byte port of the TS
//! `apps/daemon/src/paths.ts` (feature §19/§20).
//!
//! Default `~/.modelstat`; `MODELSTAT_HOME` (trimmed, non-empty) relocates
//! EVERYTHING at once — one function, one location, identical on every platform,
//! so daemon state can never fork across OS/name-specific config dirs the way the
//! old per-subsystem `conf` store did.

use std::path::{Path, PathBuf};

/// The user's home directory. Mirrors Node `os.homedir()`'s primary behavior:
/// `$HOME` on Unix, `%USERPROFILE%` on Windows. (Node's rare getpwuid fallback
/// isn't reproduced — in every environment we run, the env var is set, and test
/// isolation always goes through `MODELSTAT_HOME` anyway.)
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Absolute path to the daemon home. `MODELSTAT_HOME` wins; else `~/.modelstat`.
///
/// Read on every call (not memoised) so a test — or a relocated install — that
/// changes `MODELSTAT_HOME` is honored immediately, exactly like the TS reading
/// `process.env.MODELSTAT_HOME` per call.
pub fn modelstat_home() -> PathBuf {
    match std::env::var("MODELSTAT_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => home_dir().join(".modelstat"),
    }
}

/// A path inside the daemon home, e.g. `home_path("state.json")`.
pub fn home_path(segment: &str) -> PathBuf {
    modelstat_home().join(segment)
}

/// The `logs/` directory under the daemon home (surfaced by `paths`/`status`).
pub fn logs_dir() -> PathBuf {
    home_path("logs")
}

/// `mkdir -p` the home directory with `0700` perms (Unix), matching the TS
/// `mkdirSync(modelstatHome(), { recursive: true, mode: 0o700 })`. On Windows the
/// mode is ignored (owner-ACL hardening lands with the service work in M5).
pub fn ensure_home() -> std::io::Result<()> {
    let home = modelstat_home();
    std::fs::create_dir_all(&home)?;
    set_dir_0700(&home);
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_dir_0700(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
pub(crate) fn set_dir_0700(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process environment, so they must not run
    // concurrently with each other; `serial` here is a plain mutex used across
    // the env-touching tests in this crate.
    use crate::test_env_lock;

    #[test]
    fn modelstat_home_prefers_env_override() {
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_HOME", "/opt/modelstat");
        assert_eq!(modelstat_home(), PathBuf::from("/opt/modelstat"));
        std::env::remove_var("MODELSTAT_HOME");
        assert!(modelstat_home().ends_with(".modelstat"));
    }

    #[test]
    fn blank_env_override_falls_back_to_home() {
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_HOME", "   ");
        assert!(modelstat_home().ends_with(".modelstat"));
        std::env::remove_var("MODELSTAT_HOME");
    }
}
