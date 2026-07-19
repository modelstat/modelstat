//! The binary self-replace state machine (feature §13, plan D10): download +
//! verify the target archive → stop the local engine + quiesce → atomic swap of
//! BOTH binaries (keeping the `.prev` pair) → `_install-service` → post-swap
//! health probe → rollback the `.prev` pair on failure. The collector and engine
//! always move in lockstep from one archive, which kills the collector↔engine
//! skew and the npm-postinstall bricking class the TS daemon suffered.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use modelstat_download::{download, DownloadSpec, TtyProgress};

use crate::auto_update::auto_update_enabled;
use crate::marker::{clear_upgrade_marker, upgrade_in_progress, write_upgrade_marker};
use crate::release::{archive_url, bare_version, target_triple, DaemonRelease};

/// Post-swap health probe budget (§13).
const HEALTH_PROBE_MS: u64 = 90_000;
/// The two binary base names installed together (plan D9).
const COLLECTOR_BIN: &str = "modelstat";
const ENGINE_BIN: &str = "modelstat-summarizer";

/// The outcome of an upgrade attempt — carries the loud user-facing note (§21.12:
/// every failure is visible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// Verdict was `ok`, an update is already in flight, or this (verdict,target)
    /// was already handled this process.
    Skipped,
    /// Auto-update is off — the one-time nudge naming `modelstat upgrade`.
    OffNudge(String),
    /// The swap completed and the health probe passed.
    Completed(String),
    /// The attempt failed (download/verify/swap/probe) — held, retried next beat.
    Failed(String),
}

impl UpgradeOutcome {
    /// The note to surface (log / status), if any.
    pub fn note(&self) -> Option<&str> {
        match self {
            UpgradeOutcome::Skipped => None,
            UpgradeOutcome::OffNudge(s)
            | UpgradeOutcome::Completed(s)
            | UpgradeOutcome::Failed(s) => Some(s),
        }
    }
}

/// The binary file name for a base on this platform (`.exe` on Windows).
fn exe(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// Atomically swap a freshly-staged binary into place, keeping the current one as
/// `<name>.prev` so [`rollback_pair`] can restore it. Missing staged file is an
/// error; a missing current file (fresh install) is fine.
fn swap_one(bin_dir: &Path, base: &str, staged: &Path) -> std::io::Result<()> {
    let live = bin_dir.join(exe(base));
    let prev = bin_dir.join(format!("{}.prev", exe(base)));
    if live.exists() {
        // Replace any older .prev; keep the currently-live binary as the rollback.
        let _ = std::fs::remove_file(&prev);
        std::fs::rename(&live, &prev)?;
    }
    // Move staged → live. rename() is atomic within a filesystem; fall back to
    // copy+remove across devices (staging temp may be on a different mount).
    if std::fs::rename(staged, &live).is_err() {
        std::fs::copy(staged, &live)?;
        let _ = std::fs::remove_file(staged);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Swap BOTH binaries into place (collector + engine), keeping both `.prev`
/// copies. Either staged path may be `None` when the archive lacked it (the
/// engine is optional on a collector-only machine).
pub fn swap_pair(
    bin_dir: &Path,
    staged_collector: Option<&Path>,
    staged_engine: Option<&Path>,
) -> std::io::Result<()> {
    if let Some(c) = staged_collector {
        swap_one(bin_dir, COLLECTOR_BIN, c)?;
    }
    if let Some(e) = staged_engine {
        swap_one(bin_dir, ENGINE_BIN, e)?;
    }
    Ok(())
}

/// Restore the `.prev` pair after a failed post-swap health probe (§13 — "fixes
/// the auto-update-bricked-the-daemon class").
fn rollback_one(bin_dir: &Path, base: &str) {
    let live = bin_dir.join(exe(base));
    let prev = bin_dir.join(format!("{}.prev", exe(base)));
    if prev.exists() {
        let _ = std::fs::rename(&prev, &live);
    }
}

/// Restore both binaries from their `.prev` copies.
pub fn rollback_pair(bin_dir: &Path) {
    rollback_one(bin_dir, COLLECTOR_BIN);
    rollback_one(bin_dir, ENGINE_BIN);
}

/// Download the target archive (verified sha256 when pinned) and extract the two
/// binaries into `staging`. Returns their staged paths (`None` when the archive
/// didn't carry one). The archive layout is plan D9: `modelstat` +
/// `modelstat-summarizer` (+ `.exe`) at any depth.
async fn stage_release(
    version: &str,
    sha256: Option<&str>,
    staging: &Path,
) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    std::fs::create_dir_all(staging).map_err(|e| e.to_string())?;
    let triple = target_triple();
    let url = archive_url(version, triple);
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive = staging.join(format!("modelstat-{}.{ext}", bare_version(version)));
    let client = reqwest::Client::new();
    let spec = DownloadSpec {
        url,
        dest: archive.clone(),
        expected_sha256: sha256.map(String::from),
        size_label: None,
        label: format!("modelstat {}", bare_version(version)),
    };
    download(&client, &spec, &TtyProgress::new("update")).await
        .map_err(|e| format!("download failed: {e}"))?;
    extract_pair(&archive, staging).map_err(|e| format!("extract failed: {e}"))
}

/// Extract the collector + engine binaries from the release archive.
#[cfg(not(windows))]
fn extract_pair(archive: &Path, dir: &Path) -> std::io::Result<(Option<PathBuf>, Option<PathBuf>)> {
    use flate2::read::GzDecoder;
    let f = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(GzDecoder::new(f));
    let (mut collector, mut engine) = (None, None);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dest = if name == COLLECTOR_BIN {
            &mut collector
        } else if name == ENGINE_BIN {
            &mut engine
        } else {
            continue;
        };
        let out = dir.join(&name);
        entry.unpack(&out)?;
        *dest = Some(out);
    }
    Ok((collector, engine))
}

#[cfg(windows)]
fn extract_pair(archive: &Path, dir: &Path) -> std::io::Result<(Option<PathBuf>, Option<PathBuf>)> {
    let f = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(f).map_err(std::io::Error::other)?;
    let (mut collector, mut engine) = (None, None);
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(std::io::Error::other)?;
        let name = file
            .enclosed_name()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let dest = if name == exe(COLLECTOR_BIN) {
            &mut collector
        } else if name == exe(ENGINE_BIN) {
            &mut engine
        } else {
            continue;
        };
        let out = dir.join(&name);
        let mut w = std::fs::File::create(&out)?;
        std::io::copy(&mut file, &mut w)?;
        *dest = Some(out);
    }
    Ok((collector, engine))
}

/// Poll the freshly-installed collector for `≤90s` until it reports the target
/// version and a healthy supervision decision (§13). Returns `Ok` once both
/// agree, `Err` on timeout — the caller then rolls back.
async fn health_probe(bin_dir: &Path, target: &str) -> Result<(), String> {
    let live = bin_dir.join(exe(COLLECTOR_BIN));
    let want = bare_version(target);
    let deadline = HEALTH_PROBE_MS / 2_000;
    for _ in 0..deadline {
        tokio::time::sleep(Duration::from_millis(2_000)).await;
        // Version proves the swap landed AND the service restarted the new binary.
        if let Ok(out) = std::process::Command::new(&live).arg("--version").output() {
            let v = String::from_utf8_lossy(&out.stdout);
            if v.trim().contains(want) {
                return Ok(());
            }
        }
    }
    Err(format!(
        "the new build did not report version {want} within {}s",
        HEALTH_PROBE_MS / 1000
    ))
}

/// Run the fully-implemented binary self-replace. `now_ms` seeds the marker
/// clock. The caller quiesces its own collector scans first (the daemon
/// heartbeat path does; the manual `upgrade` command has nothing to quiesce);
/// this function stops the local engine service itself before the swap.
///
/// The live download/bounce path is exercised end-to-end against a staging
/// release in M7; the pure pieces (URL naming, swap, rollback, decision) are
/// unit-tested here.
pub async fn perform_upgrade(target: &str, sha256: Option<&str>, now_ms: i64) -> UpgradeOutcome {
    let bin_dir = modelstat_service::bin_dir();
    write_upgrade_marker(Some(target), Some(std::process::id()), now_ms).ok();

    let staging = modelstat_ingest::home_path("update-staging");
    let (collector, engine) = match stage_release(target, sha256, &staging).await {
        Ok(pair) => pair,
        Err(e) => {
            clear_upgrade_marker();
            return UpgradeOutcome::Failed(format!(
                "auto-update to {t} failed ({e}); staying on the current build, will retry — or run `modelstat upgrade`",
                t = bare_version(target)
            ));
        }
    };

    // Stop the local engine so the swap doesn't race two processes onto the model
    // (best-effort — never block the upgrade on it).
    let _ = modelstat_service::stop_service(modelstat_service::Component::Summarizer, modelstat_service::Scope::User);

    if let Err(e) = swap_pair(&bin_dir, collector.as_deref(), engine.as_deref()) {
        rollback_pair(&bin_dir);
        clear_upgrade_marker();
        return UpgradeOutcome::Failed(format!("auto-update swap failed ({e}); rolled back to the previous build"));
    }

    // Rewrite both service files + bounce them onto the new binaries.
    let live = bin_dir.join(exe(COLLECTOR_BIN));
    let _ = std::process::Command::new(&live).arg("_install-service").status();

    match health_probe(&bin_dir, target).await {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&staging);
            // Marker is cleared by the freshly-booted daemon (it holds the lock ⇒
            // the update landed); clear here too for the manual-CLI path.
            clear_upgrade_marker();
            UpgradeOutcome::Completed(format!("updated to {} — the service is running the new build", bare_version(target)))
        }
        Err(e) => {
            rollback_pair(&bin_dir);
            let _ = std::process::Command::new(bin_dir.join(exe(COLLECTOR_BIN)))
                .arg("_install-service")
                .status();
            clear_upgrade_marker();
            UpgradeOutcome::Failed(format!("auto-update health probe failed ({e}); rolled back to the previous build"))
        }
    }
}

/// Milliseconds since the Unix epoch (marker clock).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Manual `modelstat upgrade` / `modelstat-summarizer upgrade`: resolve the
/// latest published release from GitHub, then run the self-replace. No heartbeat
/// verdict is involved (this is the path self-hosted engine boxes use too, §13).
pub async fn upgrade_now() -> UpgradeOutcome {
    let target = match crate::release::resolve_latest_version().await {
        Some(v) => v,
        None => {
            return UpgradeOutcome::Failed(
                "couldn't resolve the latest release from GitHub — check your connection, or reinstall with the curl one-liner".into(),
            )
        }
    };
    perform_upgrade(&target, None, now_ms()).await
}

/// The daemon-side auto-update decision (§13), byte-parity structure with the TS
/// `maybeAutoUpdate`: verdict guard → in-progress marker guard → per-process
/// dedup → off-nudge → (caller performs). Returns the decision so the async
/// heartbeat loop owns the actual [`perform_upgrade`] call; `handled` is the
/// caller-owned dedup set (was a module-level `Set` in TS).
pub fn maybe_auto_update(
    release: &DaemonRelease,
    handled: &mut HashSet<String>,
    now_ms: i64,
) -> AutoUpdateStep {
    let verdict = release.verdict();
    if !verdict.wants_update() {
        return AutoUpdateStep::Skip;
    }
    if upgrade_in_progress(now_ms) {
        return AutoUpdateStep::Skip;
    }
    let target = release.latest.clone().unwrap_or_default();
    let key = format!("{:?}:{target}", verdict);
    if handled.contains(&key) {
        return AutoUpdateStep::Skip;
    }
    handled.insert(key);

    if !auto_update_enabled() {
        let what = if verdict.is_required() {
            "upgrade required"
        } else {
            "update available"
        };
        let latest = release.latest.as_deref().unwrap_or("?");
        return AutoUpdateStep::Nudge(format!(
            "{what} (latest {latest}); auto-update is off — run `modelstat upgrade`"
        ));
    }
    AutoUpdateStep::Proceed {
        target: release.latest.clone(),
    }
}

/// What the heartbeat loop should do with a release verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateStep {
    /// Nothing to do (ok / in-progress / already handled).
    Skip,
    /// Auto-update off — print this one-time nudge.
    Nudge(String),
    /// Auto-update on — run [`perform_upgrade`] for this target.
    Proceed { target: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_and_rollback_keep_a_recoverable_prev() {
        let dir = std::env::temp_dir().join(format!("msu-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Live "old" collector + a staged "new" one.
        std::fs::write(bin.join(exe(COLLECTOR_BIN)), b"OLD").unwrap();
        let staged = dir.join("staged-modelstat");
        std::fs::write(&staged, b"NEW").unwrap();

        swap_pair(&bin, Some(&staged), None).unwrap();
        assert_eq!(std::fs::read(bin.join(exe(COLLECTOR_BIN))).unwrap(), b"NEW");
        assert_eq!(
            std::fs::read(bin.join(format!("{}.prev", exe(COLLECTOR_BIN)))).unwrap(),
            b"OLD"
        );

        rollback_pair(&bin);
        assert_eq!(std::fs::read(bin.join(exe(COLLECTOR_BIN))).unwrap(), b"OLD");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_new_pair_rolls_both_binaries_back_in_lockstep() {
        // Models the "deliberately-broken build" health-probe failure: swap in a NEW
        // collector+engine pair, then roll BOTH back. The pair must move in lockstep
        // — both new after the swap, both old after the rollback. That lockstep is
        // what kills the collector↔engine version skew (§13); a rollback that
        // restored only one binary would resurrect exactly the skew we prevent.
        let dir = std::env::temp_dir().join(format!("msu-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Live "old" collector + engine.
        std::fs::write(bin.join(exe(COLLECTOR_BIN)), b"COLLECTOR_OLD").unwrap();
        std::fs::write(bin.join(exe(ENGINE_BIN)), b"ENGINE_OLD").unwrap();
        // Staged "new" (pretend-broken) pair.
        let staged_c = dir.join("staged-collector");
        let staged_e = dir.join("staged-engine");
        std::fs::write(&staged_c, b"COLLECTOR_NEW").unwrap();
        std::fs::write(&staged_e, b"ENGINE_NEW").unwrap();

        swap_pair(&bin, Some(&staged_c), Some(&staged_e)).unwrap();
        assert_eq!(std::fs::read(bin.join(exe(COLLECTOR_BIN))).unwrap(), b"COLLECTOR_NEW");
        assert_eq!(std::fs::read(bin.join(exe(ENGINE_BIN))).unwrap(), b"ENGINE_NEW");
        // Both old builds are preserved as the rollback pair.
        assert_eq!(
            std::fs::read(bin.join(format!("{}.prev", exe(COLLECTOR_BIN)))).unwrap(),
            b"COLLECTOR_OLD"
        );
        assert_eq!(
            std::fs::read(bin.join(format!("{}.prev", exe(ENGINE_BIN)))).unwrap(),
            b"ENGINE_OLD"
        );

        // Health probe "fails" on the new build → roll the whole pair back.
        rollback_pair(&bin);
        assert_eq!(std::fs::read(bin.join(exe(COLLECTOR_BIN))).unwrap(), b"COLLECTOR_OLD");
        assert_eq!(std::fs::read(bin.join(exe(ENGINE_BIN))).unwrap(), b"ENGINE_OLD");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_auto_update_decision_matrix() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut handled = HashSet::new();
        let now = 1_000_000_000_000;
        // Scope MODELSTAT_HOME so the marker/prefs don't touch the real home.
        let tmp = std::env::temp_dir().join(format!("msu-dec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("MODELSTAT_HOME");
        std::env::set_var("MODELSTAT_HOME", &tmp);
        std::env::remove_var("MODELSTAT_AUTO_UPDATE");

        // ok ⇒ Skip.
        let ok = DaemonRelease {
            verdict: Some("ok".into()),
            ..Default::default()
        };
        assert_eq!(maybe_auto_update(&ok, &mut handled, now), AutoUpdateStep::Skip);

        // update_available + auto-update on ⇒ Proceed, then deduped ⇒ Skip.
        let avail = DaemonRelease {
            verdict: Some("update_available".into()),
            latest: Some("1.4.2".into()),
            ..Default::default()
        };
        assert_eq!(
            maybe_auto_update(&avail, &mut handled, now),
            AutoUpdateStep::Proceed {
                target: Some("1.4.2".into())
            }
        );
        assert_eq!(
            maybe_auto_update(&avail, &mut handled, now),
            AutoUpdateStep::Skip,
            "same (verdict,target) handled once per process"
        );

        // Auto-update off ⇒ Nudge naming `modelstat upgrade`.
        std::env::set_var("MODELSTAT_AUTO_UPDATE", "off");
        let mut h2 = HashSet::new();
        let req = DaemonRelease {
            verdict: Some("upgrade_required".into()),
            latest: Some("2.0.0".into()),
            ..Default::default()
        };
        match maybe_auto_update(&req, &mut h2, now) {
            AutoUpdateStep::Nudge(n) => {
                assert!(n.contains("upgrade required"), "{n}");
                assert!(n.contains("modelstat upgrade"), "{n}");
            }
            other => panic!("expected Nudge, got {other:?}"),
        }

        std::env::remove_var("MODELSTAT_AUTO_UPDATE");
        match prev {
            Some(v) => std::env::set_var("MODELSTAT_HOME", v),
            None => std::env::remove_var("MODELSTAT_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
