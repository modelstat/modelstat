//! `~/.modelstat/spool` — redacted batches parked on disk until the wire takes
//! them.
//!
//! # Why this exists
//!
//! Building a batch is the expensive half of the daemon: every turn goes through
//! the regex floor and then the on-device PII model, which is hundreds of
//! milliseconds per window of CPU on a laptop someone is using. Shipping it is the
//! cheap half.
//!
//! Before this module the two were welded together. A flush that could not commit
//! — the server unreachable for four minutes, a 503, a closed lid — discarded the
//! prepared batch and left the file cursor un-advanced, so the next cycle re-read
//! the same transcript, re-ran the same model over the same turns, and threw the
//! result away again. One user's `err.log` carried 77 held flushes in a single
//! morning; each one had already paid for redaction it then burned. A fleet-wide
//! outage turned into a fleet-wide CPU storm that lasted exactly as long as the
//! outage did.
//!
//! So the unit of durability moves one step earlier. A batch is *finished* the
//! moment it is `fsync`ed here — not when the server acknowledges it. The scan
//! advances its cursor against THIS file, and [`crate::uploader`] drains the
//! directory on its own clock, retrying forever. Redaction is paid for exactly
//! once per turn, no matter how long the network is gone.
//!
//! Never-drop is preserved, and in fact widened: previously a hold protected the
//! events (re-read next cycle) but not the work spent on them; now both survive,
//! and they survive a daemon restart too, which a held in-memory flush never did.
//!
//! # The ordering rule
//!
//! Files are named for a monotonic sequence, so a directory listing sorted by name
//! IS the FIFO order. Nothing depends on it for correctness — every id is
//! deterministic and the server upserts — but a user watching a backlog drain
//! should see their oldest sessions land first.
//!
//! # What is deliberately NOT here
//!
//! No compression, and no attempt to merge small batches. Both would put CPU back
//! on the path this module exists to take it off.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use modelstat_wire::IngestBatch;
use serde::{Deserialize, Serialize};

/// On-disk format version. A file written by a different version is quarantined
/// rather than guessed at — see [`Spool::load`].
const SPOOL_VERSION: u32 = 1;

/// How much redacted-but-unsent data the spool holds before the scan is told to
/// stop producing more (`MODELSTAT_SPOOL_MAX_BYTES` overrides).
///
/// A cap is required: without one, a laptop that cannot reach the server for a
/// week fills its own disk. With one, the daemon stops advancing cursors and says
/// so — the transcripts are still on disk, unread, and the scan resumes from
/// exactly where it stopped once the spool drains. Nothing is lost either way; the
/// difference is whose disk fills up.
///
/// 2 GB is roughly a fortnight of heavy single-user traffic, which is far longer
/// than any outage we should be planning around.
pub const DEFAULT_MAX_SPOOL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The periodic re-check of a non-empty spool, in case a `Notify` was missed.
/// Belt-and-braces around the event-driven wake in [`Spool::wake`].
pub const DRAIN_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);

/// One batch as it sits on disk: the wire payload plus the two facts the uploader
/// needs but cannot cheaply recompute (which endpoint, how many segments).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpooledBatch {
    pub version: u32,
    /// `true` → `/v1/ingest/raw` (cloud). Recorded rather than re-derived, so a
    /// batch prepared under one mode still ships to the endpoint it was BUILT for
    /// even if the user switches modes while it waits.
    pub raw: bool,
    /// What the tray counts once this lands.
    pub segment_count: usize,
    pub batch: IngestBatch,
}

/// A spooled file the uploader has not taken yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolEntry {
    pub path: PathBuf,
    pub seq: u64,
    pub bytes: u64,
}

/// How much is waiting. Surfaced in `status --json` + the tray, because "we are
/// holding 40 batches for a server that is not answering" is the single most
/// useful thing a stuck daemon can tell someone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpoolDepth {
    pub batches: u64,
    pub bytes: u64,
}

/// Why a batch could not be parked.
#[derive(Debug)]
pub enum SpoolError {
    /// The cap is reached. Backpressure, not an error in the machine: the scan
    /// stops advancing cursors and retries when the uploader has drained some.
    Full { bytes: u64, max: u64 },
    /// The write itself failed — disk full, permissions, a read-only volume. A
    /// real fault, and the scan must stop rather than pretend the batch is safe.
    Io(std::io::Error),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::Full { bytes, max } => write!(
                f,
                "spool is full ({} MB of {} MB) — the server has not been taking \
                 batches; nothing is lost, scanning resumes as it drains",
                bytes / (1024 * 1024),
                max / (1024 * 1024)
            ),
            SpoolError::Io(e) => write!(f, "could not write to the spool: {e}"),
        }
    }
}

/// The on-disk queue of prepared batches.
///
/// Shared behind an `Arc` between the scan (which pushes) and the uploader loop
/// (which drains). Both may run at once; the filesystem is the synchronisation —
/// a push is a rename, and a take is an unlink, so neither can observe the other
/// half-done.
pub struct Spool {
    dir: PathBuf,
    max_bytes: u64,
    next_seq: AtomicU64,
    wake: Arc<tokio::sync::Notify>,
}

impl Spool {
    /// Open (creating if absent) the spool at `dir`.
    ///
    /// Recovers the sequence counter from whatever is already there, so a restart
    /// keeps appending after the existing files instead of colliding with them.
    /// Leftover `.tmp` files are crash debris — a write that never reached its
    /// rename, and therefore a batch whose cursor never advanced — so they are
    /// removed here rather than left to accumulate forever.
    pub fn open(dir: impl Into<PathBuf>, max_bytes: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut highest = 0u64;
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("tmp") => {
                    let _ = std::fs::remove_file(&path);
                }
                Some("json") => {
                    if let Some(seq) = seq_of(&path) {
                        highest = highest.max(seq + 1);
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            dir,
            max_bytes,
            next_seq: AtomicU64::new(highest),
            wake: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// The handle the uploader parks on. Signalled by every successful [`push`],
    /// so a freshly-redacted batch goes out immediately rather than waiting for
    /// the next backstop tick — the spool must not add latency to the common case
    /// where the network is fine.
    ///
    /// Signalled with `notify_one`, never `notify_waiters`: the latter only wakes
    /// tasks ALREADY parked and drops the signal otherwise, so a batch pushed
    /// while the drain was mid-pass would be missed and sit until the backstop
    /// tick a minute later. `notify_one` stores a permit, so the next park returns
    /// immediately. There is exactly one consumer, which is what makes it correct.
    ///
    /// [`push`]: Spool::push
    pub fn wake(&self) -> Arc<tokio::sync::Notify> {
        self.wake.clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Park one prepared batch, durably.
    ///
    /// Returns only after the bytes are on the platter (`fsync`) and the rename
    /// has published them, because the caller advances a file cursor on the
    /// strength of this return — an `Ok` that a power cut could undo would lose
    /// the very events the cursor now claims are handled.
    pub fn push(
        &self,
        batch: &IngestBatch,
        raw: bool,
        segment_count: usize,
    ) -> Result<SpoolEntry, SpoolError> {
        let depth = self.depth().map_err(SpoolError::Io)?;
        if depth.bytes >= self.max_bytes {
            return Err(SpoolError::Full {
                bytes: depth.bytes,
                max: self.max_bytes,
            });
        }
        let payload = SpooledBatch {
            version: SPOOL_VERSION,
            raw,
            segment_count,
            batch: batch.clone(),
        };
        let body =
            serde_json::to_vec(&payload).map_err(|e| SpoolError::Io(std::io::Error::other(e)))?;

        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let final_path = self.dir.join(file_name(seq));
        let tmp = self.dir.join(format!("{}.{}.tmp", file_name(seq), pid()));

        write_durably(&tmp, &body).map_err(SpoolError::Io)?;
        std::fs::rename(&tmp, &final_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            SpoolError::Io(e)
        })?;

        // Only now — with the batch genuinely recoverable — is anyone told.
        self.wake.notify_one();
        Ok(SpoolEntry {
            path: final_path,
            seq,
            bytes: body.len() as u64,
        })
    }

    /// Everything waiting, oldest first.
    pub fn list(&self) -> std::io::Result<Vec<SpoolEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(seq) = seq_of(&path) else { continue };
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(SpoolEntry { path, seq, bytes });
        }
        out.sort_by_key(|e| e.seq);
        Ok(out)
    }

    /// Count + bytes without deserializing anything.
    pub fn depth(&self) -> std::io::Result<SpoolDepth> {
        let mut depth = SpoolDepth::default();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            depth.batches += 1;
            depth.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
        Ok(depth)
    }

    /// Read one entry back.
    ///
    /// A file that cannot be read as a batch is QUARANTINED (renamed
    /// `.corrupt`) and reported as `Ok(None)`, rather than retried forever. The
    /// reasoning is head-of-line blocking: this file's events already have an
    /// advanced cursor, so no amount of retrying will ever make it parse, and
    /// leaving it at the front of the queue would stop every batch behind it from
    /// ever shipping. Quarantine costs one batch loudly; the alternative costs all
    /// of them silently.
    ///
    /// It should be unreachable — every file here was fsynced whole and published
    /// by an atomic rename — so it is logged at ERROR with the path kept on disk
    /// for a human.
    pub fn load(&self, entry: &SpoolEntry) -> std::io::Result<Option<SpooledBatch>> {
        let bytes = match std::fs::read(&entry.path) {
            Ok(b) => b,
            // Already taken by nobody-else (we are the only drainer) — treat a
            // vanished file as done rather than an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<SpooledBatch>(&bytes) {
            Ok(b) if b.version == SPOOL_VERSION => Ok(Some(b)),
            Ok(b) => {
                self.quarantine(
                    entry,
                    &format!("unknown spool format version {}", b.version),
                );
                Ok(None)
            }
            Err(e) => {
                self.quarantine(entry, &e.to_string());
                Ok(None)
            }
        }
    }

    /// Drop a batch the server has confirmed.
    pub fn remove(&self, entry: &SpoolEntry) -> std::io::Result<()> {
        match std::fs::remove_file(&entry.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn quarantine(&self, entry: &SpoolEntry, why: &str) {
        let dest = entry.path.with_extension("corrupt");
        let moved = std::fs::rename(&entry.path, &dest).is_ok();
        modelstat_log::log_error!(
            "spooled batch {} is unreadable ({why}) — it can never be uploaded, so it \
             has been {} and the rest of the queue continues. Please report this with \
             the file; its sessions return on the next processing-version re-scan.",
            entry.path.display(),
            if moved {
                format!("set aside at {}", dest.display())
            } else {
                "left in place".to_string()
            }
        );
    }
}

/// `00000000000000000042.json` — zero-padded so a lexical sort is a numeric one,
/// which is what makes a plain `ls` show the drain order.
fn file_name(seq: u64) -> String {
    format!("{seq:020}.json")
}

fn seq_of(path: &Path) -> Option<u64> {
    path.file_stem()?.to_str()?.parse().ok()
}

fn pid() -> u32 {
    std::process::id()
}

/// Write bytes so that a crash cannot leave them half-visible: create, write,
/// `fsync`, close. The caller renames afterwards — rename is atomic, so a reader
/// sees either no file or the whole one.
fn write_durably(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(body)?;
    // The load-bearing line. Without it the rename can be visible while the
    // contents are still in the page cache, and a power cut leaves a correctly
    // named, empty file whose events the cursor has already skipped past.
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Redacted, but still someone's transcripts — owner-only, like every other
        // file the daemon writes under ~/.modelstat.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_wire::{RawEvent, TokenUsage};

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("modelstat-spool-{}-{tag}", std::process::id()))
    }

    fn event(id: &str) -> RawEvent {
        RawEvent {
            content_bytes: None,
            source_event_id: id.into(),
            ts: "2026-08-08T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: Some(TokenUsage::default()),
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: Some("already redacted".into()),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    fn batch(id: &str, events: usize) -> IngestBatch {
        IngestBatch {
            batch_id: id.into(),
            device_id: "dev_x".into(),
            daemon_version: "test".into(),
            events: (0..events).map(|i| event(&format!("{id}-e{i}"))).collect(),
            segments: Vec::new(),
            tool_calls: Vec::new(),
            session_installs: None,
            session_titles: None,
            session_metadata: None,
            summarizer_mode: None,
            redactor_mode: None,
        }
    }

    fn fresh(tag: &str, max: u64) -> Spool {
        let dir = tmp_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        Spool::open(dir, max).unwrap()
    }

    #[test]
    fn a_pushed_batch_survives_and_round_trips() {
        let s = fresh("roundtrip", DEFAULT_MAX_SPOOL_BYTES);
        s.push(&batch("b1", 2), true, 0).unwrap();
        let entries = s.list().unwrap();
        assert_eq!(entries.len(), 1);
        let loaded = s.load(&entries[0]).unwrap().expect("readable");
        assert!(loaded.raw);
        assert_eq!(loaded.batch.batch_id, "b1");
        assert_eq!(loaded.batch.events.len(), 2);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn the_queue_drains_oldest_first_and_survives_a_reopen() {
        let dir = tmp_dir("fifo");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = Spool::open(&dir, DEFAULT_MAX_SPOOL_BYTES).unwrap();
            for i in 0..3 {
                s.push(&batch(&format!("b{i}"), 1), false, i).unwrap();
            }
        }
        // A restart must keep appending AFTER what is already there, not collide
        // with it — the bug that would silently overwrite an unsent batch.
        let s = Spool::open(&dir, DEFAULT_MAX_SPOOL_BYTES).unwrap();
        s.push(&batch("b3", 1), false, 3).unwrap();
        let ids: Vec<String> = s
            .list()
            .unwrap()
            .iter()
            .map(|e| s.load(e).unwrap().unwrap().batch.batch_id)
            .collect();
        assert_eq!(ids, vec!["b0", "b1", "b2", "b3"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_spool_pushes_back_instead_of_dropping() {
        // Cap below one batch, so the second push must be refused.
        let s = fresh("full", 1);
        s.push(&batch("b1", 1), false, 0).unwrap();
        match s.push(&batch("b2", 1), false, 0) {
            Err(SpoolError::Full { .. }) => {}
            other => panic!("expected backpressure, got {other:?}"),
        }
        // The refusal must not have consumed or damaged what was already parked.
        assert_eq!(s.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn removing_a_committed_batch_empties_the_queue() {
        let s = fresh("remove", DEFAULT_MAX_SPOOL_BYTES);
        s.push(&batch("b1", 1), false, 0).unwrap();
        let entries = s.list().unwrap();
        s.remove(&entries[0]).unwrap();
        assert!(s.list().unwrap().is_empty());
        assert_eq!(s.depth().unwrap(), SpoolDepth::default());
        // Removing twice is not an error — a retry after a partial failure must
        // not wedge the drain.
        s.remove(&entries[0]).unwrap();
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn a_corrupt_file_is_quarantined_so_the_queue_behind_it_still_ships() {
        let s = fresh("corrupt", DEFAULT_MAX_SPOOL_BYTES);
        s.push(&batch("good", 1), false, 0).unwrap();
        let bad = s.dir().join(file_name(999));
        std::fs::write(&bad, b"{ not json").unwrap();

        let entries = s.list().unwrap();
        assert_eq!(entries.len(), 2);
        let bad_entry = entries.iter().find(|e| e.seq == 999).unwrap();
        assert!(
            s.load(bad_entry).unwrap().is_none(),
            "a corrupt file must not parse"
        );
        // Set aside, not left to block the head of the queue forever.
        assert!(!bad.exists());
        assert!(bad.with_extension("corrupt").exists());
        // And the good one is still readable.
        let good = entries.iter().find(|e| e.seq != 999).unwrap();
        assert_eq!(s.load(good).unwrap().unwrap().batch.batch_id, "good");
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn crash_debris_is_cleared_on_open() {
        let dir = tmp_dir("debris");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A half-written file from a killed process: its cursor never advanced,
        // so dropping it loses nothing and keeping it leaks disk forever.
        let junk = dir.join(format!("{}.123.tmp", file_name(7)));
        std::fs::write(&junk, b"partial").unwrap();
        let s = Spool::open(&dir, DEFAULT_MAX_SPOOL_BYTES).unwrap();
        assert!(!junk.exists());
        assert!(s.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The latency guarantee. A batch pushed while the uploader was busy — not yet
    /// parked on the notify — must still wake it the moment it parks. With
    /// `notify_waiters` that signal is dropped on the floor and the batch waits a
    /// full backstop tick, which would make the spool cost exactly the latency it
    /// is not allowed to cost.
    #[tokio::test]
    async fn a_push_while_the_uploader_is_busy_still_wakes_it() {
        let s = fresh("wake", DEFAULT_MAX_SPOOL_BYTES);
        let wake = s.wake();
        // Push FIRST, with nobody waiting yet.
        s.push(&batch("b1", 1), false, 0).unwrap();
        // Parking afterwards must return immediately, not hang.
        tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
            .await
            .expect("a push before the wait must still be seen");
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn depth_counts_what_is_waiting() {
        let s = fresh("depth", DEFAULT_MAX_SPOOL_BYTES);
        assert_eq!(s.depth().unwrap().batches, 0);
        s.push(&batch("b1", 3), false, 0).unwrap();
        s.push(&batch("b2", 3), false, 0).unwrap();
        let d = s.depth().unwrap();
        assert_eq!(d.batches, 2);
        assert!(d.bytes > 0);
        let _ = std::fs::remove_dir_all(s.dir());
    }
}
