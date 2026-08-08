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

/// The minimal covering set of directories to watch for the given discovered
/// transcript paths.
///
/// Derived from DISCOVERY, never from an app-name list: the watcher's world
/// must be exactly the scanner's, or an install discovery finds (relocated,
/// renamed, newly supported) gets real-time capture silently downgraded to
/// the 5-minute backstop. The old shape — a hard-coded list of well-known app
/// dirs, "byte-faithful" to the TS daemon — was precisely that: discovery had
/// been weakened to artefact shapes and running processes while the watcher
/// still assumed the apps of one machine in 2025.
///
/// Coverage rule: each transcript's grandparent directory (so a Claude-style
/// `root/<project>/<session>.jsonl` tree is one recursive watch at `root`,
/// covering projects that don't exist yet), clamped to the parent when the
/// grandparent would leave the tree unbounded ($HOME, its ancestors, or the
/// filesystem root), then collapsed so nothing in the set is covered by an
/// ancestor also in the set.
pub fn covering_watch_dirs(paths: &[&Path], home: &Path) -> Vec<PathBuf> {
    let too_broad = |d: &Path| d == home || home.starts_with(d);
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for p in paths {
        let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) else {
            continue;
        };
        let candidate = match parent.parent() {
            Some(gp) if !too_broad(gp) && !gp.as_os_str().is_empty() => gp.to_path_buf(),
            _ if !too_broad(parent) => parent.to_path_buf(),
            _ => continue,
        };
        dirs.insert(candidate);
    }
    // Ancestor collapse: recursive watches make a covered child redundant.
    let all = dirs.clone();
    dirs.into_iter()
        .filter(|d| !all.iter().any(|a| a != d && d.starts_with(a)))
        .collect()
}

/// True for a file change the watcher acts on — the observed transcript
/// containers: `.jsonl` streams, and the SQLite stores Cursor keeps chat in
/// (`.vscdb` + its WAL sidecars, which is where a live write actually lands).
/// The `.jsonl`-only filter silently made the Cursor watch dead weight: its
/// roots were watched, its writes never matched.
pub fn is_transcript_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".jsonl")
        || name.ends_with(".vscdb")
        || name.ends_with(".vscdb-wal")
        || name.ends_with(".vscdb-shm")
}

pub(crate) fn home_dir() -> Option<PathBuf> {
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

    let jobs = crate::discover_jobs::discover_jobs();
    let paths: Vec<PathBuf> = jobs.iter().map(|j| PathBuf::from(&j.path)).collect();
    let path_refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let dirs = covering_watch_dirs(&path_refs, &home_dir().unwrap_or_default());
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
    fn covering_set_is_the_grandparent_collapsed_by_ancestors() {
        let home = Path::new("/home/dev");
        let a = Path::new("/home/dev/.claude/projects/app-one/s1.jsonl");
        let b = Path::new("/home/dev/.claude/projects/app-two/s2.jsonl");
        let c = Path::new("/home/dev/.codex/sessions/r1.jsonl");
        let dirs = covering_watch_dirs(&[a, b, c], home);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/dev/.claude/projects"),
                PathBuf::from("/home/dev/.codex")
            ],
            "one recursive watch per tree — future project dirs are covered \
             before they exist"
        );
    }

    #[test]
    fn a_relocated_install_is_watched_wherever_discovery_found_it() {
        // The case the app-name list silently mishandled: discovery finds it
        // (shape + process probe), so the watcher must cover it too.
        let home = Path::new("/home/dev");
        let odd = Path::new("/opt/tools/my-agent/data/projects/x/s.jsonl");
        let dirs = covering_watch_dirs(&[odd], home);
        assert_eq!(
            dirs,
            vec![PathBuf::from("/opt/tools/my-agent/data/projects")]
        );
    }

    #[test]
    fn the_watch_never_widens_to_home_or_its_ancestors() {
        let home = Path::new("/home/dev");
        // A transcript sitting one level under $HOME: grandparent would be
        // $HOME itself — clamp to the parent instead of watching everything.
        let shallow = Path::new("/home/dev/.aider/history.jsonl");
        assert_eq!(
            covering_watch_dirs(&[shallow], home),
            vec![PathBuf::from("/home/dev/.aider")]
        );
        // Directly in $HOME: nothing safe to watch; contribute nothing.
        let in_home = Path::new("/home/dev/loose.jsonl");
        assert!(covering_watch_dirs(&[in_home], home).is_empty());
    }

    #[test]
    fn cursor_stores_and_their_wal_sidecars_are_watchable_events() {
        // The .jsonl-only filter made the Cursor watch dead weight: roots
        // watched, writes never matched — a live chat lands in the WAL first.
        assert!(is_transcript_file(Path::new("/x/state.vscdb")));
        assert!(is_transcript_file(Path::new("/x/state.vscdb-wal")));
        assert!(is_transcript_file(Path::new("/x/state.vscdb-shm")));
        assert!(is_transcript_file(Path::new("/x/s.jsonl")));
        assert!(!is_transcript_file(Path::new("/x/notes.txt")));
    }

    #[test]
    fn only_jsonl_is_a_transcript() {
        assert!(is_transcript_file(Path::new("/x/a.jsonl")));
        assert!(!is_transcript_file(Path::new("/x/a.txt")));
        assert!(!is_transcript_file(Path::new("/x/a")));
    }
}
