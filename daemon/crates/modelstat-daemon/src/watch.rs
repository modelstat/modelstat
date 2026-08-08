//! Watch-mode directory resolution — the cross-platform set of AI-tool data dirs
//! the daemon's filesystem watcher subscribes to. A port of
//! `apps/daemon/src/watch.ts::resolveWatchDirs`.
//!
//! This module owns the pure dir list + the `.jsonl` filter the watcher shares.
//! The `notify`-based watch LOOP itself is assembled in daemon-main (where the
//! scan trigger + the 5-minute backstop live), the same way `discover_jobs` owns
//! discovery separately from the scan loop that consumes it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The candidate watch dirs under the given roots, deduped (first-seen order) and
/// filtered to those that exist. `is_darwin` adds the macOS App-Support dirs;
/// `exists` is injected for testability. Byte-faithful to `resolveWatchDirs` —
/// note it deliberately OMITS `.pi` (the TS list does too; the scan discovery
/// covers pi, so the watcher mirrors the exact TS set).
pub fn resolve_watch_dirs_in(
    home: &Path,
    xdg_config: &Path,
    xdg_data: &Path,
    is_darwin: bool,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        // universal (default HOME-rooted CLI data dirs)
        home.join(".claude/projects"),
        home.join(".codex/sessions"),
        home.join(".cursor/ai-tracking"),
        home.join(".gemini"),
        home.join(".aider"),
        // XDG / Linux
        xdg_config.join("claude/projects"),
        xdg_config.join("codex/sessions"),
        xdg_config.join("Cursor/User/workspaceStorage"),
        xdg_config.join("Code/User/workspaceStorage"),
        xdg_config.join("Code - Insiders/User/workspaceStorage"),
        xdg_data.join("claude/projects"),
    ];
    if is_darwin {
        let app = home.join("Library/Application Support");
        candidates.extend([
            app.join("Cursor/User/workspaceStorage"),
            app.join("Claude"),
            app.join("Code/User/workspaceStorage"),
            app.join("Windsurf/User/workspaceStorage"),
            app.join("Zed"),
        ]);
    }
    // Dedupe (preserve first-seen order), then keep only dirs that exist so the
    // startup log reads cleanly on a fresh machine.
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .filter(|p| exists(p))
        .collect()
}

/// The real watch dirs: `$HOME` + `$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` (defaulted
/// to `~/.config` and `~/.local/share`) + the OS, filtered by real existence.
pub fn resolve_watch_dirs() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let xdg_data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    resolve_watch_dirs_in(
        &home,
        &xdg_config,
        &xdg_data,
        cfg!(target_os = "macos"),
        |p| p.exists(),
    )
}

/// True for a transcript file the watcher acts on (a `.jsonl`) — the watch loop
/// ignores every other change under the watched trees.
pub fn is_transcript_file(path: &Path) -> bool {
    path.extension().map(|e| e == "jsonl").unwrap_or(false)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

/// The foreground `modelstat watch` loop (feature §5, dev convenience): an
/// initial scan, then a debounced (1s) re-scan whenever a transcript changes,
/// plus the 5-minute backstop. Runs until Ctrl-C. Shares the notify/scan glue
/// with the daemon's own watcher (`run.rs`) but without the service machinery.
pub async fn watch_forever(daemon: std::sync::Arc<crate::runtime::Daemon>) {
    use notify::{Event, RecursiveMode, Watcher};

    let dirs = resolve_watch_dirs();
    let joined = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    modelstat_log::log_info!("watching: {joined}");

    // A foreground watcher is still a collector: scanning only redacts and parks
    // batches in the spool, so without this loop nothing it produced would ever be
    // sent. Started before the first scan so anything a previous run left queued
    // goes out immediately.
    tokio::spawn(crate::uploader::run_drain_loop(
        daemon.spool.clone(),
        daemon.api.clone(),
        crate::runtime::UploadStatusObserver::new(daemon.status.clone(), daemon.state.clone()),
    ));

    crate::runtime::run_scan_cycle(daemon.clone(), "startup".to_string()).await;

    // notify's callback runs on its own thread; bridge to async via a channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            if ev.paths.iter().any(|p| is_transcript_file(p)) {
                let _ = tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            modelstat_log::log_error!("filesystem watcher failed to start: {e}");
            return;
        }
    };
    for dir in &dirs {
        let _ = watcher.watch(dir, RecursiveMode::Recursive);
    }

    // 5-minute backstop — catches anything the OS event stream missed.
    let backstop = daemon.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        tick.tick().await; // consume the immediate first tick
        loop {
            tick.tick().await;
            crate::runtime::run_scan_cycle(backstop.clone(), "interval".to_string()).await;
        }
    });

    let sig = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(sig);
    loop {
        tokio::select! {
            _ = &mut sig => break,
            r = rx.recv() => {
                if r.is_none() {
                    break;
                }
                // 1s debounce: collapse a burst of writes into one coalesced scan.
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                while rx.try_recv().is_ok() {}
                crate::runtime::run_scan_cycle(daemon.clone(), "change".to_string()).await;
            }
        }
    }
    drop(watcher);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_existing_dirs_deduped_and_omits_pi() {
        let home = PathBuf::from("/home/dev");
        let xdg_config = home.join(".config");
        let xdg_data = home.join(".local/share");
        // Pretend only two of the candidates exist.
        let existing: BTreeSet<PathBuf> =
            [home.join(".claude/projects"), home.join(".codex/sessions")]
                .into_iter()
                .collect();
        let dirs = resolve_watch_dirs_in(&home, &xdg_config, &xdg_data, false, |p| {
            existing.contains(p)
        });
        assert_eq!(
            dirs,
            vec![home.join(".claude/projects"), home.join(".codex/sessions")]
        );
        // .pi is never a candidate (matches watch.ts).
        assert!(!dirs.iter().any(|p| p.to_string_lossy().contains(".pi")));
    }

    #[test]
    fn darwin_adds_app_support_dirs() {
        let home = PathBuf::from("/Users/dev");
        // Everything "exists" so we can see the full candidate set.
        let linux = resolve_watch_dirs_in(
            &home,
            &home.join(".config"),
            &home.join(".local/share"),
            false,
            |_| true,
        );
        let mac = resolve_watch_dirs_in(
            &home,
            &home.join(".config"),
            &home.join(".local/share"),
            true,
            |_| true,
        );
        assert!(mac.len() > linux.len());
        assert!(mac
            .iter()
            .any(|p| p.ends_with("Library/Application Support/Zed")));
        assert!(!linux
            .iter()
            .any(|p| p.to_string_lossy().contains("Application Support")));
    }

    #[test]
    fn only_jsonl_is_a_transcript() {
        assert!(is_transcript_file(Path::new("/x/a.jsonl")));
        assert!(!is_transcript_file(Path::new("/x/a.txt")));
        assert!(!is_transcript_file(Path::new("/x/a")));
    }
}
