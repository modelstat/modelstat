//! Boot-time runaway-log rotation.
//!
//! Whoever supervises the daemon — launchd writing `StandardOutPath` /
//! `StandardErrorPath`, or the tray holding its own O_APPEND handle on the same
//! two files — keeps the log fd OPEN across daemon restarts. So an oversized log
//! must be truncated **in place** (`set_len(0)`): renaming would detach the path
//! from the live fd and the supervisor would keep growing the renamed inode
//! forever. We copy the tail to `<name>.old.log` first so the last crash stacks
//! survive. Best-effort throughout: any fs error leaves the log as-is and never
//! blocks boot.
//!
//! Both files are rotated, not just `err.log`: since the daemon routes INFO to
//! stdout, a reprocess narrating every session grows `out.log` the same way a
//! warn loop grows `err.log`.
//!
//! (The 2026-06-11 incident left a 992 MB `err.log` — one warn line repeated ~5M
//! times during a full reprocess — because launchd never rotates its own paths.)

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use modelstat_ingest::home_path;

/// Rotate a log once it exceeds this. 64 MB — generous for real forensics, far
/// below the multi-hundred-MB runaway that motivated this.
pub const LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// How much of an oversized log's tail survives into `<name>.old.log` — enough to
/// keep the most recent crash stacks.
pub const LOG_TAIL_KEEP_BYTES: u64 = 4 * 1024 * 1024;

/// What a rotation did (returned for the boot log + tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotation {
    pub was_bytes: u64,
    pub kept_bytes: u64,
    pub old_log: PathBuf,
}

/// The `<name>.old.log` sibling: strip a trailing `.log` and append `.old.log`
/// (so `out.log` → `out.old.log`), matching the TS `replace(/\.log$/, ".old.log")`.
fn old_log_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let stem = name.strip_suffix(".log").unwrap_or(name);
    path.with_file_name(format!("{stem}.old.log"))
}

/// Rotate one log file in place if it exceeds `max_bytes`: copy its last
/// `keep_bytes` to `<name>.old.log`, then truncate the live file to 0 WITHOUT
/// re-creating the inode (so an O_APPEND supervisor fd keeps writing to the same,
/// now-empty file). `Ok(None)` = under the cap (untouched) or the file is absent.
/// The injectable core (thresholds as params) so the size logic is unit-tested
/// without a 64 MB fixture.
pub fn rotate_log_file(
    path: &Path,
    max_bytes: u64,
    keep_bytes: u64,
) -> std::io::Result<Option<Rotation>> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        // Absent log (fresh install / not yet supervised) — nothing to rotate.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = meta.len();
    if size <= max_bytes {
        return Ok(None);
    }
    let keep = keep_bytes.min(size);
    // Read the tail: seek to size-keep, read `keep` bytes.
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(size - keep))?;
    let mut tail = Vec::with_capacity(keep as usize);
    f.by_ref().take(keep).read_to_end(&mut tail)?;
    drop(f);
    let old = old_log_path(path);
    std::fs::write(&old, &tail)?;
    // Truncate IN PLACE — set_len(0) keeps the inode the supervisor's fd points at.
    let live = std::fs::OpenOptions::new().write(true).open(path)?;
    live.set_len(0)?;
    Ok(Some(Rotation {
        was_bytes: size,
        kept_bytes: keep,
        old_log: old,
    }))
}

/// Trim `~/.modelstat/logs/{out,err}.log` before anything else writes to them, so
/// a daemon that died spamming one line never boots back up on top of a
/// gigabyte-scale log. Best-effort: every fs error is swallowed. Port of
/// `rotateRunawayLogs`.
pub fn rotate_runaway_logs() {
    let dir = home_path("logs");
    for name in ["out.log", "err.log"] {
        let path = dir.join(name);
        match rotate_log_file(&path, LOG_MAX_BYTES, LOG_TAIL_KEEP_BYTES) {
            Ok(Some(r)) => {
                modelstat_log::log_warn!(
                    "rotated {name}: was {} MB (> {} MB cap), tail kept in {}",
                    r.was_bytes / 1024 / 1024,
                    LOG_MAX_BYTES / 1024 / 1024,
                    r.old_log.display()
                );
            }
            Ok(None) => {}
            // Best-effort — never block daemon boot on log hygiene.
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("modelstat-rotate-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn oversized_log_is_truncated_in_place_with_tail_kept() {
        let dir = tmp_dir("big");
        let path = dir.join("err.log");
        // 200 bytes of distinguishable content; the last 20 are the tail to keep.
        let mut body: Vec<u8> = (0..180u16).map(|i| b'A' + (i % 26) as u8).collect();
        body.extend_from_slice(b"TAIL-KEEP-LAST-BYTES");
        assert_eq!(body.len(), 200);
        std::fs::write(&path, &body).unwrap();

        // Capture the inode so we can prove truncate kept it (not a re-create).
        let ino_before = std::fs::metadata(&path).unwrap();

        let r = rotate_log_file(&path, 100, 20).unwrap().unwrap();
        assert_eq!(r.was_bytes, 200);
        assert_eq!(r.kept_bytes, 20);

        // Live log truncated to 0, SAME inode (O_APPEND supervisor fd survives).
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(after.len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                ino_before.ino(),
                after.ino(),
                "truncate must keep the inode"
            );
        }
        let _ = ino_before;

        // The tail (last 20 bytes) landed in err.old.log.
        let old = dir.join("err.old.log");
        assert_eq!(std::fs::read(&old).unwrap(), b"TAIL-KEEP-LAST-BYTES");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn under_cap_log_is_left_untouched() {
        let dir = tmp_dir("small");
        let path = dir.join("out.log");
        std::fs::write(&path, b"just a little log").unwrap();
        assert!(rotate_log_file(&path, 100, 20).unwrap().is_none());
        // Untouched, and no .old.log was written.
        assert_eq!(std::fs::read(&path).unwrap(), b"just a little log");
        assert!(!dir.join("out.old.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_log_is_a_noop() {
        let dir = tmp_dir("absent");
        let path = dir.join("err.log"); // never created
        assert!(rotate_log_file(&path, 100, 20).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_log_name_swaps_the_suffix() {
        assert_eq!(
            old_log_path(Path::new("/l/out.log")),
            PathBuf::from("/l/out.old.log")
        );
        assert_eq!(
            old_log_path(Path::new("/l/err.log")),
            PathBuf::from("/l/err.old.log")
        );
    }
}
