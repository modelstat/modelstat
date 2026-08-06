//! The binary self-replace state machine (feature §13, plan D10): download +
//! verify the target archive → stop the local engine + quiesce → atomic swap of
//! BOTH binaries (keeping the `.prev` pair) → `_install-service` → post-swap
//! health probe → rollback the `.prev` pair on failure. The collector and engine
//! always move in lockstep from one archive, which kills the collector↔engine
//! skew and the npm-postinstall bricking class the TS daemon suffered.
//!
//! # The macOS tray rides along
//!
//! The mac archives carry `ModelstatTray.app` next to the two binaries, and it is
//! swapped in the same step. Before that it was staged ONLY by `_setup-runtime`,
//! which runs from the install script and never from here — so a machine that
//! installed once kept its original tray binary forever while the daemon updated
//! underneath it, and a tray fix reached nobody until they re-ran the installer
//! by hand. Nothing announced the skew.
//!
//! Two rules the tray does not share with the binaries:
//!
//! * **Replace, never install.** `modelstat stop` deletes the bundle on purpose.
//!   An absent tray is a decision, so the swap is skipped rather than
//!   resurrecting an app somebody removed.
//! * **A tray failure does not roll back the daemon.** The collector is the
//!   product and the tray is a menu-bar convenience; undoing a good daemon
//!   update because an icon would not copy is the worse outcome. The failure is
//!   carried into the outcome note instead of being swallowed, and [`swap_tray`]
//!   restores the previous bundle before returning, so a failed swap leaves a
//!   working tray rather than none.

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
/// The macOS menu-bar tray bundle, carried only by the mac archives. Must match
/// the directory `release-daemon-rs.yml` copies into the mac stage, and the name
/// `modelstat_service::tray::tray_app_path()` installs to.
#[cfg(target_os = "macos")]
const TRAY_APP: &str = "ModelstatTray.app";

/// Everything a release archive carried, once staged into `update-staging`.
///
/// A named struct rather than a tuple: which `Option<PathBuf>` is which stopped
/// being obvious at three, and each one is absent for its own reason.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StagedRelease {
    /// The collector (`modelstat`).
    pub collector: Option<PathBuf>,
    /// The summariser engine — absent on a collector-only archive.
    pub engine: Option<PathBuf>,
    /// The staged `ModelstatTray.app` root. Only the mac archives carry one.
    pub tray_app: Option<PathBuf>,
    /// Shared libraries that must land beside the binaries. Only the x86_64 macOS
    /// archive carries one today (ONNX Runtime, linked dynamically there because
    /// `ort` publishes no prebuilt static library for that target). Dropping these
    /// on self-update would leave a NEW collector next to an absent library — an
    /// install that worked until the first auto-update, then died at `dyld: Library
    /// not loaded`, which is the worst possible time to find out.
    pub sidecars: Vec<PathBuf>,
}

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

/// A shared library that must travel beside the binaries.
///
/// Matched on the loader's own extension rather than a per-target filename list:
/// the archive decides what it carries, and there is exactly one place (here) that
/// decides what "carry it along" means.
fn is_sidecar(name: &str) -> bool {
    name.ends_with(".dylib") || name.ends_with(".so") || name.ends_with(".dll")
}

/// Copy staged shared libraries into `bin_dir` BEFORE the binaries are swapped, so
/// the new collector never goes live without the library it was linked against.
///
/// Additive on purpose — the filenames are version-stamped
/// (`libonnxruntime.1.22.0.dylib`), so the previous library stays in place and the
/// `.prev` binary a rollback restores still has its own. Best-effort: a failure here
/// is reported by the caller's health probe, which is the thing that decides whether
/// the upgrade holds.
pub fn place_sidecars(bin_dir: &Path, staged: &[PathBuf]) -> std::io::Result<()> {
    for src in staged {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = bin_dir.join(name);
        if src.as_path() == dst {
            continue;
        }
        if std::fs::rename(src, &dst).is_err() {
            std::fs::copy(src, &dst)?;
            let _ = std::fs::remove_file(src);
        }
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

/// The rollback copy of an installed tray bundle: `ModelstatTray.app.prev`, a
/// sibling of the live one so both stay on the same filesystem and the restore
/// is a rename rather than a copy.
#[cfg(target_os = "macos")]
fn tray_prev(live: &Path) -> PathBuf {
    let name = live
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| TRAY_APP.to_string());
    live.with_file_name(format!("{name}.prev"))
}

/// Swap a staged tray bundle over the installed one, keeping the current bundle
/// as `<name>.prev`.
///
/// **Transactional.** A bundle is a directory tree, so unlike a binary there is a
/// window where the live path holds neither version. If anything after the first
/// rename fails, the previous bundle is put back before returning the error —
/// the caller is then reporting "the tray could not be updated", never "the tray
/// is gone". A half-swapped `.app` would leave `tray_binary().exists()` false,
/// `ensure_tray_installed()` would quietly decline to install the agent, and the
/// user's menu bar would just be empty with nothing said.
#[cfg(target_os = "macos")]
pub fn swap_tray(live: &Path, staged: &Path) -> std::io::Result<()> {
    let prev = tray_prev(live);
    let _ = std::fs::remove_dir_all(&prev);
    std::fs::rename(live, &prev)?;
    if std::fs::rename(staged, live).is_ok() {
        return Ok(());
    }
    // Staging may sit on another mount, where rename() cannot cross. `cp -R` is
    // what `_setup-runtime` already stages the bundle with, and it preserves the
    // symlinks and permissions an ad-hoc code signature is validated against.
    let copied = std::process::Command::new("cp")
        .arg("-R")
        .arg(staged)
        .arg(live)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if copied {
        let _ = std::fs::remove_dir_all(staged);
        return Ok(());
    }
    // Undo the first rename so the machine keeps the tray it had.
    let _ = std::fs::remove_dir_all(live);
    let _ = std::fs::rename(&prev, live);
    Err(std::io::Error::other(format!(
        "could not move the staged bundle into {}",
        live.display()
    )))
}

/// Restore the tray bundle from its `.prev` copy, for the health-probe rollback.
/// Silent no-op when there is no `.prev` — the same shape as [`rollback_one`],
/// because a rollback that itself fails must not mask the failure being rolled
/// back.
#[cfg(target_os = "macos")]
pub fn rollback_tray(live: &Path) {
    let prev = tray_prev(live);
    if prev.exists() {
        let _ = std::fs::remove_dir_all(live);
        let _ = std::fs::rename(&prev, live);
    }
}

/// Download the target archive (verified sha256 when pinned) and extract the two
/// binaries into `staging`. Returns their staged paths (`None` when the archive
/// didn't carry one). The archive layout is plan D9: `modelstat` +
/// `modelstat-summarizer` (+ `.exe`) at any depth.
async fn stage_release(
    version: &str,
    sha256: Option<&str>,
    staging: &Path,
) -> Result<StagedRelease, String> {
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
    // No prefix — `DownloadError` already renders as "download failed: …", and
    // this string is now read by users rather than discarded, so the doubled
    // "download failed: download failed: HTTP 404" it used to produce matters.
    download(&client, &spec, &TtyProgress::new("update"))
        .await
        .map_err(|e| e.to_string())?;
    extract_release(&archive, staging).map_err(|e| format!("extract failed: {e}"))
}

/// The path of an archive entry relative to the tray bundle root, when the entry
/// lives inside it. Archive paths arrive as `./ModelstatTray.app/Contents/…`, so
/// the leading `.` is dropped before matching; the bundle root itself maps to an
/// empty path.
#[cfg(target_os = "macos")]
fn tray_relative(p: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut comps = p
        .components()
        .filter(|c| !matches!(c, Component::CurDir | Component::RootDir));
    match comps.next() {
        Some(Component::Normal(name)) if name == TRAY_APP => Some(comps.collect()),
        _ => None,
    }
}

/// Extract the collector + engine binaries — and, on macOS, the whole tray bundle
/// — from the release archive.
#[cfg(not(windows))]
fn extract_release(archive: &Path, dir: &Path) -> std::io::Result<StagedRelease> {
    use flate2::read::GzDecoder;
    let f = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(GzDecoder::new(f));
    let mut staged = StagedRelease::default();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        // The tray is a directory tree, so it is unpacked under its own relative
        // path rather than flattened by file name: `Contents/MacOS/modelstat-tray`
        // and `Contents/_CodeSignature/CodeResources` have to keep their places or
        // the bundle stops being loadable and its signature stops validating.
        #[cfg(target_os = "macos")]
        if let Some(rel) = tray_relative(&path) {
            let root = dir.join(TRAY_APP);
            let out = root.join(&rel);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry.unpack(&out)?;
            staged.tray_app = Some(root);
            continue;
        }

        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if is_sidecar(&name) {
            let out = dir.join(&name);
            entry.unpack(&out)?;
            staged.sidecars.push(out);
            continue;
        }
        let dest = if name == COLLECTOR_BIN {
            &mut staged.collector
        } else if name == ENGINE_BIN {
            &mut staged.engine
        } else {
            continue;
        };
        let out = dir.join(&name);
        entry.unpack(&out)?;
        *dest = Some(out);
    }
    Ok(staged)
}

/// Windows carries no tray (§15 — Windows and Linux are headless).
#[cfg(windows)]
fn extract_release(archive: &Path, dir: &Path) -> std::io::Result<StagedRelease> {
    let f = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(f).map_err(std::io::Error::other)?;
    let mut staged = StagedRelease::default();
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(std::io::Error::other)?;
        let name = file
            .enclosed_name()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if is_sidecar(&name) {
            let out = dir.join(&name);
            let mut w = std::fs::File::create(&out)?;
            std::io::copy(&mut file, &mut w)?;
            staged.sidecars.push(out);
            continue;
        }
        let dest = if name == exe(COLLECTOR_BIN) {
            &mut staged.collector
        } else if name == exe(ENGINE_BIN) {
            &mut staged.engine
        } else {
            continue;
        };
        let out = dir.join(&name);
        let mut w = std::fs::File::create(&out)?;
        std::io::copy(&mut file, &mut w)?;
        *dest = Some(out);
    }
    Ok(staged)
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
    let staged = match stage_release(target, sha256, &staging).await {
        Ok(s) => s,
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
    let _ = modelstat_service::stop_service(
        modelstat_service::Component::Summarizer,
        modelstat_service::Scope::User,
    );

    // Libraries first: a collector that goes live before its dylib is a binary that
    // cannot start, and the health probe would roll back an upgrade that was only
    // ever mis-ordered.
    let sidecar_note = place_sidecars(&bin_dir, &staged.sidecars)
        .err()
        .map(|e| format!(" — but a shared library could not be staged ({e}); the health probe below decides whether this upgrade holds"));
    if let Err(e) = swap_pair(
        &bin_dir,
        staged.collector.as_deref(),
        staged.engine.as_deref(),
    ) {
        rollback_pair(&bin_dir);
        clear_upgrade_marker();
        return UpgradeOutcome::Failed(format!(
            "auto-update swap failed ({e}); rolled back to the previous build"
        ));
    }

    // The tray bundle rides the same archive, so a tray fix lands here instead of
    // waiting for someone to re-run the installer. Deliberately NOT fatal: see
    // the module header — a menu-bar icon that would not copy must not undo a
    // good daemon update, so the failure is carried into the note below rather
    // than swallowed, and the previous bundle is still in place.
    let tray_note = swap_tray_if_installed(&staged);

    // Rewrite both service files + bounce them onto the new binaries. This also
    // reloads the tray agent (`_install-service` → `ensure_tray_installed` →
    // `launchd_reload`), which is what restarts the running tray onto the bundle
    // just swapped in — hence the swap has to happen above this line.
    let live = bin_dir.join(exe(COLLECTOR_BIN));
    let _ = std::process::Command::new(&live)
        .arg("_install-service")
        .status();

    match health_probe(&bin_dir, target).await {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&staging);
            // Drop the tray's rollback copy now the probe has passed. The
            // binaries keep their `.prev` in the hidden `~/.modelstat/bin`, but
            // the tray's sibling lives in `~/Applications` — somewhere people
            // actually browse — and a stray `ModelstatTray.app.prev` there reads
            // as a second, broken install.
            discard_tray_prev();
            // Marker is cleared by the freshly-booted daemon (it holds the lock ⇒
            // the update landed); clear here too for the manual-CLI path.
            clear_upgrade_marker();
            UpgradeOutcome::Completed(format!(
                "updated to {} — the service is running the new build{}",
                bare_version(target),
                format!(
                    "{}{}",
                    sidecar_note.unwrap_or_default(),
                    tray_note.unwrap_or_default()
                )
            ))
        }
        Err(e) => {
            rollback_pair(&bin_dir);
            rollback_tray_if_installed();
            let _ = std::process::Command::new(bin_dir.join(exe(COLLECTOR_BIN)))
                .arg("_install-service")
                .status();
            clear_upgrade_marker();
            UpgradeOutcome::Failed(format!(
                "auto-update health probe failed ({e}); rolled back to the previous build"
            ))
        }
    }
}

/// Swap the staged tray over the installed one, when there is both a staged
/// bundle and an installed one to replace. Returns a note to append to the
/// upgrade outcome — `None` when there was nothing to do, `Some` when the swap
/// failed and the user needs to know the tray stayed behind.
///
/// Replace-never-install is the point: `modelstat stop` deletes the bundle, so an
/// absent tray means someone removed it and an update must not bring it back.
#[cfg(target_os = "macos")]
fn swap_tray_if_installed(staged: &StagedRelease) -> Option<String> {
    let staged_app = staged.tray_app.as_deref()?;
    let live = modelstat_service::tray::tray_app_path();
    if !live.exists() {
        return None;
    }
    match swap_tray(&live, staged_app) {
        Ok(()) => {
            // The bundle on disk is new; the PROCESS is not until it is
            // relaunched. Skipping this is why a tray fix could sit installed and
            // unseen for days.
            modelstat_service::tray::restart_tray();
            None
        }
        Err(e) => Some(format!(
            " — but the menu-bar tray could not be updated ({e}); it is still running the previous build, reinstall to refresh it"
        )),
    }
}

/// No tray off macOS (§15 — Windows and Linux are headless).
#[cfg(not(target_os = "macos"))]
fn swap_tray_if_installed(_staged: &StagedRelease) -> Option<String> {
    None
}

/// Put the previous tray bundle back alongside the binary rollback, so a failed
/// health probe leaves the whole install on the build it was working on.
#[cfg(target_os = "macos")]
fn rollback_tray_if_installed() {
    rollback_tray(&modelstat_service::tray::tray_app_path());
    // Symmetric with the swap: the restored bundle is only what the user sees
    // once the process running the rolled-back build is replaced too.
    modelstat_service::tray::restart_tray();
}

#[cfg(not(target_os = "macos"))]
fn rollback_tray_if_installed() {}

/// Remove the tray's `.prev` bundle once the update is confirmed good.
#[cfg(target_os = "macos")]
fn discard_tray_prev() {
    let _ = std::fs::remove_dir_all(tray_prev(&modelstat_service::tray::tray_app_path()));
}

#[cfg(not(target_os = "macos"))]
fn discard_tray_prev() {}

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
        assert_eq!(
            std::fs::read(bin.join(exe(COLLECTOR_BIN))).unwrap(),
            b"COLLECTOR_NEW"
        );
        assert_eq!(
            std::fs::read(bin.join(exe(ENGINE_BIN))).unwrap(),
            b"ENGINE_NEW"
        );
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
        assert_eq!(
            std::fs::read(bin.join(exe(COLLECTOR_BIN))).unwrap(),
            b"COLLECTOR_OLD"
        );
        assert_eq!(
            std::fs::read(bin.join(exe(ENGINE_BIN))).unwrap(),
            b"ENGINE_OLD"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.app` is a directory tree, and the tray fix in #75 only reaches an
    /// existing install if that tree is swapped whole and can be put back.
    #[cfg(target_os = "macos")]
    #[test]
    fn tray_bundle_swaps_whole_and_rolls_back_whole() {
        let dir = std::env::temp_dir().join(format!("msu-tray-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let apps = dir.join("Applications");
        let live = apps.join(TRAY_APP);

        // An installed bundle, with its binary nested where a real one lives.
        std::fs::create_dir_all(live.join("Contents/MacOS")).unwrap();
        std::fs::write(live.join("Contents/MacOS/modelstat-tray"), b"TRAY_OLD").unwrap();
        std::fs::write(live.join("Contents/Info.plist"), b"PLIST_OLD").unwrap();
        // …and a staged replacement.
        let staged = dir.join("staging").join(TRAY_APP);
        std::fs::create_dir_all(staged.join("Contents/MacOS")).unwrap();
        std::fs::write(staged.join("Contents/MacOS/modelstat-tray"), b"TRAY_NEW").unwrap();
        std::fs::write(staged.join("Contents/Info.plist"), b"PLIST_NEW").unwrap();

        swap_tray(&live, &staged).unwrap();
        assert_eq!(
            std::fs::read(live.join("Contents/MacOS/modelstat-tray")).unwrap(),
            b"TRAY_NEW"
        );
        // The nested structure has to survive, or the bundle stops being loadable.
        assert_eq!(
            std::fs::read(live.join("Contents/Info.plist")).unwrap(),
            b"PLIST_NEW"
        );
        assert_eq!(
            std::fs::read(tray_prev(&live).join("Contents/MacOS/modelstat-tray")).unwrap(),
            b"TRAY_OLD"
        );

        rollback_tray(&live);
        assert_eq!(
            std::fs::read(live.join("Contents/MacOS/modelstat-tray")).unwrap(),
            b"TRAY_OLD"
        );
        assert_eq!(
            std::fs::read(live.join("Contents/Info.plist")).unwrap(),
            b"PLIST_OLD"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The transactional guarantee. A bundle swap has a window where the live
    /// path holds neither version; if the second move fails there, the previous
    /// bundle must come back. Leaving no bundle would make
    /// `ensure_tray_installed()` quietly decline to install the agent, and the
    /// user's menu bar would empty with nothing said.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_failed_tray_swap_leaves_the_previous_bundle_in_place() {
        let dir = std::env::temp_dir().join(format!("msu-tray-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let live = dir.join("Applications").join(TRAY_APP);
        std::fs::create_dir_all(live.join("Contents/MacOS")).unwrap();
        std::fs::write(live.join("Contents/MacOS/modelstat-tray"), b"TRAY_OLD").unwrap();

        // Staged path that does not exist: both the rename and the `cp -R`
        // fallback fail, which is the window this guards.
        let missing = dir.join("staging").join(TRAY_APP);
        let err = swap_tray(&live, &missing).unwrap_err();
        assert!(
            err.to_string().contains("could not move"),
            "expected a loud move failure, got {err}"
        );

        // The machine still has the tray it started with.
        assert!(live.exists(), "the live bundle was left missing");
        assert_eq!(
            std::fs::read(live.join("Contents/MacOS/modelstat-tray")).unwrap(),
            b"TRAY_OLD"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `modelstat stop` deletes the bundle on purpose, so "no tray installed" is
    /// a decision the updater has to respect rather than undo.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_uninstalled_tray_is_never_resurrected() {
        // No `tray_app` in the archive ⇒ nothing to do, whatever is on disk.
        assert_eq!(swap_tray_if_installed(&StagedRelease::default()), None);
    }

    /// End-to-end over a real `.tar.gz` laid out exactly like the published mac
    /// archive (verified against `modelstat-1.4.1-aarch64-apple-darwin.tar.gz`):
    /// `./modelstat`, `./modelstat-summarizer`, and a `./ModelstatTray.app/`
    /// tree beside them.
    ///
    /// The trap this guards: the binary matcher keys off the entry's FILE NAME,
    /// so a bundle unpacked by file name would flatten `Contents/MacOS/…` into
    /// the staging root and destroy the app.
    #[cfg(target_os = "macos")]
    #[test]
    fn extract_pulls_the_tray_tree_out_of_a_real_archive_layout() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let dir = std::env::temp_dir().join(format!("msu-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("release.tar.gz");

        let entries: [(&str, &[u8]); 5] = [
            ("./modelstat", b"COLLECTOR"),
            ("./modelstat-summarizer", b"ENGINE"),
            ("./ModelstatTray.app/Contents/Info.plist", b"PLIST"),
            ("./ModelstatTray.app/Contents/MacOS/modelstat-tray", b"TRAY"),
            (
                "./ModelstatTray.app/Contents/_CodeSignature/CodeResources",
                b"SIG",
            ),
        ];
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut builder = tar::Builder::new(GzEncoder::new(f, Compression::fast()));
            for (path, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, path, data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }

        let out = dir.join("staging");
        std::fs::create_dir_all(&out).unwrap();
        let staged = extract_release(&archive, &out).unwrap();

        assert_eq!(staged.collector, Some(out.join(COLLECTOR_BIN)));
        assert_eq!(staged.engine, Some(out.join(ENGINE_BIN)));
        assert_eq!(staged.tray_app, Some(out.join(TRAY_APP)));

        // The bundle keeps its shape — this is what makes it loadable and keeps
        // its signature checkable.
        let app = out.join(TRAY_APP);
        assert_eq!(
            std::fs::read(app.join("Contents/MacOS/modelstat-tray")).unwrap(),
            b"TRAY"
        );
        assert_eq!(
            std::fs::read(app.join("Contents/Info.plist")).unwrap(),
            b"PLIST"
        );
        assert_eq!(
            std::fs::read(app.join("Contents/_CodeSignature/CodeResources")).unwrap(),
            b"SIG"
        );
        // …and nothing from inside the bundle was flattened into the root.
        assert!(
            !out.join("modelstat-tray").exists(),
            "a bundle file was unpacked by file name into the staging root"
        );
        // The collector is the archive's own binary, not anything from the app.
        assert_eq!(
            std::fs::read(out.join(COLLECTOR_BIN)).unwrap(),
            b"COLLECTOR"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Archive paths arrive as `./ModelstatTray.app/…`; everything else in the
    /// archive must be left to the binary matcher.
    #[cfg(target_os = "macos")]
    #[test]
    fn tray_entries_are_recognised_and_nothing_else_is() {
        assert_eq!(
            tray_relative(Path::new(
                "./ModelstatTray.app/Contents/MacOS/modelstat-tray"
            )),
            Some(PathBuf::from("Contents/MacOS/modelstat-tray"))
        );
        // The bundle root maps to an empty relative path (the directory itself).
        assert_eq!(
            tray_relative(Path::new("./ModelstatTray.app/")),
            Some(PathBuf::new())
        );
        assert_eq!(
            tray_relative(Path::new("ModelstatTray.app/Contents")),
            Some(PathBuf::from("Contents"))
        );
        // The two binaries sit beside the bundle and must not be captured by it.
        assert_eq!(tray_relative(Path::new("./modelstat")), None);
        assert_eq!(tray_relative(Path::new("./modelstat-summarizer")), None);
        assert_eq!(tray_relative(Path::new("./ModelstatTrayOther.app/x")), None);
    }

    #[test]
    fn maybe_auto_update_decision_matrix() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        assert_eq!(
            maybe_auto_update(&ok, &mut handled, now),
            AutoUpdateStep::Skip
        );

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
