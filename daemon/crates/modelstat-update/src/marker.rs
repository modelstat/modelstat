//! The self-update in-progress marker (feature §13):
//! `~/.modelstat/upgrade-in-progress.json` `{pid, workerPid, at, target}`, TTL
//! **5 min**, cleared early when the worker pid dies. It stops a supervisor
//! respawn from stacking a second concurrent update mid-swap.

use std::path::PathBuf;

use modelstat_ingest::{home_path, modelstat_home};
use serde::{Deserialize, Serialize};

/// Marker TTL — a genuinely stuck upgrade still retries after this. TS
/// `UPGRADE_MARKER_TTL_MS`.
pub const UPGRADE_MARKER_TTL_MS: i64 = 5 * 60_000;

fn marker_path() -> PathBuf {
    home_path("upgrade-in-progress.json")
}

/// On-disk shape (camelCase `workerPid`, TS parity). `target` is the version we
/// are moving to, or null for a "latest" manual upgrade.
#[derive(Serialize, Deserialize)]
struct Marker {
    pid: u32,
    #[serde(rename = "workerPid")]
    worker_pid: Option<u32>,
    at: i64,
    target: Option<String>,
}

/// Write (or refresh) the marker atomically (0600). `worker_pid` is `None` in
/// the tiny pre-spawn window, then rewritten with the child pid so
/// [`upgrade_in_progress`] can tell "install still running" from "install
/// finished/died". `now_ms` is injected so the state machine stays testable.
pub fn write_upgrade_marker(
    target: Option<&str>,
    worker_pid: Option<u32>,
    now_ms: i64,
) -> std::io::Result<()> {
    let dir = modelstat_home();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(
        "upgrade-in-progress.json.{}.tmp",
        std::process::id()
    ));
    let body = serde_json::to_string(&Marker {
        pid: std::process::id(),
        worker_pid,
        at: now_ms,
        target: target.map(String::from),
    })?;
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, marker_path())?;
    Ok(())
}

/// Best-effort marker removal.
pub fn clear_upgrade_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// Whether an update is in progress *right now* — honored only inside the TTL
/// and only while the worker pid is alive; a stale-by-clock or dead-worker
/// marker is cleared and reported not-in-progress (so the next verdict retries
/// rather than being wedged). TS `upgradeInProgress`.
pub fn upgrade_in_progress(now_ms: i64) -> bool {
    let Ok(raw) = std::fs::read_to_string(marker_path()) else {
        return false;
    };
    let Ok(m) = serde_json::from_str::<Marker>(&raw) else {
        return false;
    };
    if !(m.at > 0 && now_ms - m.at < UPGRADE_MARKER_TTL_MS) {
        clear_upgrade_marker(); // stale by the clock
        return false;
    }
    if let Some(worker) = m.worker_pid {
        if worker > 0 && !pid_alive(worker) {
            clear_upgrade_marker(); // the install worker is gone
            return false;
        }
    }
    true
}

/// Whether a process exists. Unix: `kill(pid, 0)` — success or `EPERM` (exists,
/// not ours) ⇒ alive; `ESRCH` ⇒ gone. Other platforms: conservative `true` —
/// the 5-min TTL is the backstop that clears a wedged marker there.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 performs only the existence/permission check.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped_home() -> (PathBuf, Option<std::ffi::OsString>) {
        let tmp = std::env::temp_dir().join(format!(
            "msu-mark-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("MODELSTAT_HOME");
        std::env::set_var("MODELSTAT_HOME", &tmp);
        (tmp, prev)
    }

    #[test]
    fn marker_honors_ttl_and_worker_liveness() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (tmp, prev) = scoped_home();
        let now = 1_000_000_000_000;

        // Live self (our own pid is alive) inside TTL ⇒ in progress.
        write_upgrade_marker(Some("daemon-1.2.3"), Some(std::process::id()), now).unwrap();
        assert!(upgrade_in_progress(now + 1000));

        // Past TTL ⇒ cleared + not in progress.
        assert!(!upgrade_in_progress(now + UPGRADE_MARKER_TTL_MS + 1));
        assert!(!marker_path().exists(), "stale marker is removed");

        // Dead worker pid ⇒ cleared (pid 999999999 is not alive on unix).
        #[cfg(unix)]
        {
            write_upgrade_marker(None, Some(999_999_999), now).unwrap();
            assert!(!upgrade_in_progress(now + 1000));
        }

        // Pre-spawn window (worker_pid None) is honored while inside TTL.
        write_upgrade_marker(Some("daemon-1.2.3"), None, now).unwrap();
        assert!(upgrade_in_progress(now + 1000));

        match prev {
            Some(v) => std::env::set_var("MODELSTAT_HOME", v),
            None => std::env::remove_var("MODELSTAT_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
