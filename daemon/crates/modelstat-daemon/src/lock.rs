//! Daemon singleton lock — a port of `apps/daemon/src/lock.ts`.
//!
//! Prevents a second `modelstat start` (or a tray-launched second instance) from
//! running in parallel: two daemons would double-POST /v1/ingest, clobber the
//! shared file cursor, and interleave heartbeats. Mechanism: a lockfile at
//! `~/.modelstat/daemon.lock` holding `{ pid, startedAt, daemonVersion, apiUrl }`,
//! taken via `open(O_CREAT|O_EXCL)` + rename, with a `kill(pid,0)` staleness probe.
//!
//! NOT a bulletproof distributed lock — two daemons started in the same
//! millisecond can both win the rename. The post-acquire recheck
//! ([`check_lock_ownership`]) makes the loser stand down; the daemon main loop
//! schedules that recheck + wires the SIGINT/SIGTERM cleanup (this module owns the
//! pure mechanism + decision, tested without touching `~/.modelstat`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn unknown() -> String {
    "unknown".to_string()
}

/// The lockfile payload (camelCase on disk, matching the TS `JSON.stringify`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockMeta {
    pub pid: i64,
    #[serde(default = "unknown")]
    pub started_at: String,
    #[serde(default = "unknown")]
    pub daemon_version: String,
    #[serde(default = "unknown")]
    pub api_url: String,
}

/// Whether a process is alive — `kill(pid, 0)` on Unix (EPERM = exists-but-other-
/// user → alive), `OpenProcess` on Windows. `pid <= 0` is never alive.
#[cfg(unix)]
pub fn is_process_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only probes existence/permission; it delivers
    // no signal and cannot corrupt process state.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return true;
    }
    // EPERM: the process exists but is owned by another user — treat as alive so
    // we don't steal their lock. ESRCH: no such process — dead.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_process_alive(pid: i64) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid <= 0 {
        return false;
    }
    // SAFETY: OpenProcess/CloseHandle are the documented liveness probe; a null
    // handle means the process is gone (the per-user lock means the owner is
    // always same-user, so the access-denied case doesn't arise here).
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// Best-effort terminate (the `--force` recovery path + the supervisor's
/// replace-a-wedged-owner path). Unix SIGTERM; Windows `TerminateProcess`. A
/// gone/unpermitted process is silently ignored.
#[cfg(unix)]
pub fn terminate_process(pid: i64) {
    if pid > 0 {
        // SAFETY: SIGTERM to a pid we already probed; the OS handles a since-dead
        // pid with ESRCH, which we ignore.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(windows)]
pub fn terminate_process(pid: i64) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if pid <= 0 {
        return;
    }
    // SAFETY: standard terminate sequence; a failed OpenProcess yields null which
    // we skip.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

/// Last-resort kill for an owner that ignored SIGTERM (the supervisor's replace
/// escalation). Unix SIGKILL; on Windows [`terminate_process`] is already
/// immediate, so this is the same call.
#[cfg(unix)]
pub fn terminate_process_hard(pid: i64) {
    if pid > 0 {
        // SAFETY: SIGKILL to a probed pid; a since-dead pid yields ESRCH, ignored.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
pub fn terminate_process_hard(pid: i64) {
    terminate_process(pid);
}

/// The default lockfile path: `<MODELSTAT_HOME>/daemon.lock`.
pub fn daemon_lock_path() -> PathBuf {
    modelstat_ingest::home_path("daemon.lock")
}

/// Read + validate the lockfile. `None` on a missing/corrupt file or a payload
/// without a numeric `pid`. Path is injectable for tests + the supervise probe.
pub fn read_daemon_lock(lock_file: &Path) -> Option<LockMeta> {
    let raw = std::fs::read_to_string(lock_file).ok()?;
    // A payload without a numeric `pid` fails to deserialize → None (matching the
    // TS `typeof obj.pid !== "number"` guard); missing strings default to "unknown".
    serde_json::from_str::<LockMeta>(&raw).ok()
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Atomically write the lockfile: `create_new` (O_CREAT|O_EXCL) a pid+ms-suffixed
/// tmp path, then rename over the target. Only one racer wins a given tmp name.
fn write_lock_atomic(lock_file: &Path, meta: &LockMeta) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = lock_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let base = lock_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "daemon.lock".to_string());
    let tmp = lock_file.with_file_name(format!("{}.{}.{}.tmp", base, meta.pid, now_millis()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(meta)?.as_bytes())?;
    }
    std::fs::rename(&tmp, lock_file)?;
    Ok(())
}

/// Remove the lockfile IFF it's still owned by `owner_pid` (idempotent; a no-op if
/// someone else took over). Called from the daemon main loop's shutdown path.
pub fn remove_lock_if_owned(lock_file: &Path, owner_pid: i64) {
    if let Some(lock) = read_daemon_lock(lock_file) {
        if lock.pid == owner_pid {
            let _ = std::fs::remove_file(lock_file);
        }
    }
}

/// The outcome of acquiring the lock.
#[derive(Debug, Clone, PartialEq)]
pub enum AcquireResult {
    /// We own the lock — proceed to run the daemon.
    Acquired,
    /// A live daemon already owns it — print a message and exit 0.
    AlreadyRunning { owner: LockMeta, age_sec: i64 },
}

/// Options for [`acquire_daemon_lock`].
pub struct AcquireOpts {
    pub daemon_version: String,
    pub api_url: String,
    /// Kill any live owner and take the lock (`modelstat start --force`). A tray
    /// supervisor must NOT pass this blindly — a healthy live owner is adopted,
    /// not killed (see supervise).
    pub force: bool,
}

/// How long after acquiring to re-read the lock and confirm ownership — long
/// enough for a racing daemon's write to land, short enough that a duplicate does
/// no meaningful double-work.
pub const LOCK_RECHECK_MS: u64 = 5_000;

/// Try to acquire the singleton lock. On [`AcquireResult::Acquired`] the lockfile
/// is ours; the caller (daemon main) schedules the [`LOCK_RECHECK_MS`] recheck via
/// [`check_lock_ownership`] and installs the signal cleanup that calls
/// [`remove_lock_if_owned`].
pub fn acquire_daemon_lock(lock_file: &Path, opts: &AcquireOpts) -> std::io::Result<AcquireResult> {
    let existing = read_daemon_lock(lock_file);
    if let Some(e) = &existing {
        if is_process_alive(e.pid) {
            if !opts.force {
                let age_sec = age_in_seconds(&e.started_at);
                return Ok(AcquireResult::AlreadyRunning {
                    owner: e.clone(),
                    age_sec,
                });
            }
            terminate_process(e.pid);
        }
    }
    // Either no lock, a stale lock, or we just killed the owner.
    let meta = LockMeta {
        pid: std::process::id() as i64,
        started_at: now_iso(),
        daemon_version: opts.daemon_version.clone(),
        api_url: opts.api_url.clone(),
    };
    write_lock_atomic(lock_file, &meta)?;
    Ok(AcquireResult::Acquired)
}

/// What the post-acquire recheck concluded (pure; the pid probe is injectable so
/// the duplicate-daemon convergence rule is unit-testable):
///   - `Owned`      — the lock is ours (or vanished): keep running.
///   - `WinnerDead` — someone overwrote it but they're already gone: keep running.
///   - `Lost`       — a LIVE rival owns it: stand down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipCheck {
    Owned,
    Lost,
    WinnerDead,
}

pub fn check_lock_ownership(
    my_pid: i64,
    current: Option<&LockMeta>,
    pid_alive: impl Fn(i64) -> bool,
) -> OwnershipCheck {
    match current {
        None => OwnershipCheck::Owned,
        Some(c) if c.pid == my_pid => OwnershipCheck::Owned,
        Some(c) if !pid_alive(c.pid) => OwnershipCheck::WinnerDead,
        Some(_) => OwnershipCheck::Lost,
    }
}

/// Seconds since an ISO-8601 instant, or -1 when unparseable (matches the TS
/// `Number.isNaN` guard).
pub fn age_in_seconds(iso: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => {
            let ms = chrono::Utc::now().timestamp_millis() - t.timestamp_millis();
            (ms / 1000).max(0)
        }
        Err(_) => -1,
    }
}

/// Human-friendly age like `3m 12s` for log lines.
pub fn format_age(seconds: i64) -> String {
    if seconds < 0 {
        return "?".to_string();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let m = seconds / 60;
    let s = seconds % 60;
    if m < 60 {
        return format!("{m}m {s}s");
    }
    let h = m / 60;
    format!("{h}h {}m", m % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pid: i64) -> LockMeta {
        LockMeta {
            pid,
            started_at: "2026-07-16T10:00:00.000Z".into(),
            daemon_version: "9.9.9".into(),
            api_url: "https://modelstat.ai".into(),
        }
    }

    #[test]
    fn ownership_decision_is_pure_and_covers_the_race() {
        // No lock or our own pid → owned.
        assert_eq!(
            check_lock_ownership(10, None, |_| true),
            OwnershipCheck::Owned
        );
        assert_eq!(
            check_lock_ownership(10, Some(&meta(10)), |_| true),
            OwnershipCheck::Owned
        );
        // A different, LIVE pid → we lost the race.
        assert_eq!(
            check_lock_ownership(10, Some(&meta(20)), |_| true),
            OwnershipCheck::Lost
        );
        // A different but DEAD pid → winner already gone, keep running.
        assert_eq!(
            check_lock_ownership(10, Some(&meta(20)), |_| false),
            OwnershipCheck::WinnerDead
        );
    }

    #[test]
    fn format_age_matches_the_ts_buckets() {
        assert_eq!(format_age(-1), "?");
        assert_eq!(format_age(3), "3s");
        assert_eq!(format_age(72), "1m 12s");
        assert_eq!(format_age(3 * 3600 + 4 * 60), "3h 4m");
    }

    #[test]
    fn is_process_alive_probes_self_and_rejects_nonpositive() {
        assert!(is_process_alive(std::process::id() as i64));
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(-1));
    }

    #[test]
    fn read_write_roundtrip_and_acquire_on_a_fresh_path() {
        let dir = std::env::temp_dir().join(format!("modelstat-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = dir.join("daemon.lock");

        // Fresh path → acquired, file present, round-trips.
        let r = acquire_daemon_lock(
            &lock,
            &AcquireOpts {
                daemon_version: "1.2.3".into(),
                api_url: "https://x".into(),
                force: false,
            },
        )
        .unwrap();
        assert_eq!(r, AcquireResult::Acquired);
        let read = read_daemon_lock(&lock).unwrap();
        assert_eq!(read.pid, std::process::id() as i64);
        assert_eq!(read.daemon_version, "1.2.3");

        // A live owner (our own pid) blocks a non-forced acquire.
        let r2 = acquire_daemon_lock(
            &lock,
            &AcquireOpts {
                daemon_version: "1.2.3".into(),
                api_url: "https://x".into(),
                force: false,
            },
        )
        .unwrap();
        assert!(matches!(r2, AcquireResult::AlreadyRunning { .. }));

        // remove_lock_if_owned only removes OUR lock.
        remove_lock_if_owned(&lock, 999999); // not us → left alone
        assert!(read_daemon_lock(&lock).is_some());
        remove_lock_if_owned(&lock, std::process::id() as i64);
        assert!(read_daemon_lock(&lock).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_pidless_lock_reads_as_none() {
        let dir = std::env::temp_dir().join(format!("modelstat-lock2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("daemon.lock");
        std::fs::write(&lock, "{ not json").unwrap();
        assert!(read_daemon_lock(&lock).is_none());
        std::fs::write(&lock, r#"{"startedAt":"x"}"#).unwrap(); // no pid
        assert!(read_daemon_lock(&lock).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
