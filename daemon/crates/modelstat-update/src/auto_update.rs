//! The auto-update **preference** (feature §13): `~/.modelstat/auto-update.json`
//! `{autoUpdate}` (default **on**), with the `MODELSTAT_AUTO_UPDATE` env pin that
//! wins for managed fleets. A separate file from `state.json` on purpose — the
//! daemon rewrites `state.json` on every cursor advance, which would clobber a
//! toggle written by a separate `modelstat autoupdate` process; this file is
//! *only read* by the daemon and *only written* by the CLI, so the two never race.
//!
//! Byte-parity port of `apps/daemon/src/update.ts` (prefs half). The self-update
//! MECHANISM is npm-free here (binary self-replace — [`crate::perform`]); only the
//! preference shape + env-pin semantics are preserved.

use std::path::PathBuf;

use modelstat_ingest::{home_path, modelstat_home};
use serde::{Deserialize, Serialize};

/// `~/.modelstat/auto-update.json`.
fn pref_path() -> PathBuf {
    home_path("auto-update.json")
}

/// On-disk shape: a single optional bool, absent ⇒ on. Serialized as exactly
/// `{"autoUpdate":true}` / `{"autoUpdate":false}` (camelCase, TS parity).
#[derive(Serialize, Deserialize)]
struct AutoUpdatePref {
    #[serde(rename = "autoUpdate")]
    auto_update: bool,
}

/// Parse an env flag to an explicit bool, or `None` when unset/unrecognized.
/// Truthy = `1|on|true|yes|enable|enabled`; falsy = `0|off|false|no|disable|disabled`;
/// anything else (incl. empty) ⇒ `None` (treated as *not pinned*). Port of TS
/// `parseFlag` (`update.ts`), used for both the value and the "pinned?" test.
pub fn parse_flag(raw: Option<&str>) -> Option<bool> {
    match raw?.trim().to_lowercase().as_str() {
        "1" | "on" | "true" | "yes" | "enable" | "enabled" => Some(true),
        "0" | "off" | "false" | "no" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

/// The stored preference (default **on**). Read fresh from disk each call so a
/// `modelstat autoupdate` toggle from another process is seen by the running
/// daemon without a restart. Missing/unparseable ⇒ `true`.
pub fn stored_auto_update() -> bool {
    match std::fs::read_to_string(pref_path()) {
        Ok(raw) => serde_json::from_str::<AutoUpdatePref>(&raw)
            .map(|p| p.auto_update)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Persist the preference atomically (0700 home, 0600 file, `<path>.<pid>.tmp`
/// + rename) — TS `setStoredAutoUpdate` parity.
pub fn set_stored_auto_update(enabled: bool) -> std::io::Result<()> {
    let dir = modelstat_home();
    std::fs::create_dir_all(&dir)?;
    let path = pref_path();
    let tmp = dir.join(format!("auto-update.json.{}.tmp", std::process::id()));
    let body = serde_json::to_string(&AutoUpdatePref {
        auto_update: enabled,
    })?;
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Effective setting: the `MODELSTAT_AUTO_UPDATE` env override wins (fleet
/// control), else the stored preference. TS `autoUpdateEnabled`.
pub fn auto_update_enabled() -> bool {
    match parse_flag(std::env::var("MODELSTAT_AUTO_UPDATE").ok().as_deref()) {
        Some(v) => v,
        None => stored_auto_update(),
    }
}

/// True when pinned by env (so the CLI/tray toggle is informational only). TS
/// `autoUpdatePinnedByEnv`.
pub fn auto_update_pinned_by_env() -> bool {
    parse_flag(std::env::var("MODELSTAT_AUTO_UPDATE").ok().as_deref()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flag_matches_ts_truth_tables() {
        for t in ["1", "on", "true", "yes", "enable", "enabled", "ON", " Yes "] {
            assert_eq!(parse_flag(Some(t)), Some(true), "{t}");
        }
        for f in ["0", "off", "false", "no", "disable", "disabled", "OFF"] {
            assert_eq!(parse_flag(Some(f)), Some(false), "{f}");
        }
        for n in ["", "  ", "maybe", "2", "yep"] {
            assert_eq!(parse_flag(Some(n)), None, "{n}");
        }
        assert_eq!(parse_flag(None), None);
    }

    #[test]
    fn stored_pref_roundtrips_and_defaults_on() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("msu-au-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // MODELSTAT_HOME drives home_path; scope the env to this test dir.
        let prev = std::env::var_os("MODELSTAT_HOME");
        std::env::set_var("MODELSTAT_HOME", &tmp);
        std::env::remove_var("MODELSTAT_AUTO_UPDATE");

        assert!(stored_auto_update(), "absent file ⇒ on");
        set_stored_auto_update(false).unwrap();
        assert!(!stored_auto_update());
        // Exact on-disk bytes (camelCase, no spaces).
        let raw = std::fs::read_to_string(tmp.join("auto-update.json")).unwrap();
        assert_eq!(raw, r#"{"autoUpdate":false}"#);
        set_stored_auto_update(true).unwrap();
        assert!(stored_auto_update());

        // Env pin wins over the stored value + flags "pinned".
        std::env::set_var("MODELSTAT_AUTO_UPDATE", "off");
        assert!(!auto_update_enabled());
        assert!(auto_update_pinned_by_env());
        std::env::remove_var("MODELSTAT_AUTO_UPDATE");
        assert!(!auto_update_pinned_by_env());

        match prev {
            Some(v) => std::env::set_var("MODELSTAT_HOME", v),
            None => std::env::remove_var("MODELSTAT_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
