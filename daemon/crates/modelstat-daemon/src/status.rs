//! The daemon's live status reporter — the mutable phase/progress/stats state
//! that feeds BOTH the heartbeat POST body and the `last-status.json` mirror the
//! tray + `modelstat statusline` read. Port of the `status` object + `snapshotBody`
//! + `writeLocalStatus` in `apps/daemon/src/daemon.ts`.
//!
//! `Status` is the pure state + snapshot serialization (unit-tested here);
//! daemon-main wraps it in an `Arc<Mutex<..>>`, drives the mutators from the scan
//! callbacks, and owns the 400ms throttled flush + the 10s heartbeat writer.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

/// The daemon phase surfaced to the dashboard + tray. `discovering`/`idle` exist
/// for wire-compat but are never set in daemon-main (discovery rides the heartbeat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Discovering,
    Idle,
    Scanning,
    Processing,
    Uploading,
    Watching,
    Offline,
    Error,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Discovering => "discovering",
            Phase::Idle => "idle",
            Phase::Scanning => "scanning",
            Phase::Processing => "processing",
            Phase::Uploading => "uploading",
            Phase::Watching => "watching",
            Phase::Offline => "offline",
            Phase::Error => "error",
        }
    }
}

/// A release verdict + latest version, surfaced from the heartbeat response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub verdict: String,
    pub latest: Option<String>,
}

/// The upload fan-out happening RIGHT NOW: how many sessions are still on the
/// wire, and when the set started. `None` when nothing is uploading.
///
/// Its own thing rather than two more `stats` counters, because the pair only
/// means anything together — a count with no clock reads as frozen, and a clock
/// with no count doesn't say what is taking that long. Uploads within a set go
/// out together, so `since_ms` also dates the LONGEST one still running, which
/// is the number a watcher wants: how long has the slowest session been going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadingNow {
    /// Sessions still in flight — counted down as each upload commits.
    pub sessions: u64,
    /// Uploads still in flight. Equals `sessions` in cloud mode (one session per
    /// batch); a local-mode batch can carry several sessions, so both are kept.
    pub uploads: u64,
    /// Epoch ms the set started, for the same reason as
    /// [`Status::busy_since_ms`]: the reader subtracts and ticks on its own beat.
    pub since_ms: i64,
}

/// The mutable live status. Every field maps to a `snapshotBody` key.
#[derive(Debug, Clone)]
pub struct Status {
    pub phase: Phase,
    pub message: Option<String>,
    pub progress_done: u64,
    pub progress_total: u64,
    pub queue_size: u64,
    /// `Record<string, number|string>` — arbitrary counters/labels.
    pub stats: BTreeMap<String, Value>,
    pub last_event_at: Option<String>,
    pub update: Option<UpdateInfo>,
    pub auto_update: bool,
    /// When the unit of work now in progress started, epoch ms — `None` when
    /// nothing is being processed.
    ///
    /// A TIMESTAMP rather than a duration on purpose: the reader (tray, CLI)
    /// re-renders every second and subtracts, so the elapsed clock ticks live
    /// without the daemon rewriting this file once a second to animate it.
    pub busy_since_ms: Option<i64>,
    /// The upload set in flight, or `None` when nothing is on the wire.
    pub uploading: Option<UploadingNow>,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            phase: Phase::Starting,
            message: None,
            progress_done: 0,
            progress_total: 0,
            queue_size: 0,
            stats: BTreeMap::new(),
            last_event_at: None,
            update: None,
            auto_update: false,
            busy_since_ms: None,
            uploading: None,
        }
    }
}

impl Status {
    pub fn set_phase(&mut self, phase: Phase, message: impl Into<String>) {
        self.phase = phase;
        self.message = Some(message.into());
    }
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }
    pub fn set_progress(&mut self, done: u64, total: u64) {
        self.progress_done = done;
        self.progress_total = total;
    }
    /// Mark work as started NOW, so readers can show how long it has been
    /// running. Idle again → [`Status::clear_busy`].
    pub fn set_busy_now(&mut self) {
        self.busy_since_ms = Some(chrono::Utc::now().timestamp_millis());
    }
    pub fn clear_busy(&mut self) {
        self.busy_since_ms = None;
        // A reader that shows "3 sessions uploading" against a stopped daemon is
        // worse than showing nothing, so the two go idle together.
        self.uploading = None;
    }

    /// A fan-out of `uploads` batches covering `sessions` sessions just started.
    /// Restarts the clock: they go out together, so this dates all of them.
    pub fn start_upload_set(&mut self, uploads: u64, sessions: u64) {
        if uploads == 0 {
            self.uploading = None;
            return;
        }
        self.uploading = Some(UploadingNow {
            sessions,
            uploads,
            since_ms: chrono::Utc::now().timestamp_millis(),
        });
    }

    /// One upload of the current set committed (or held). Counts sessions down
    /// proportionally when a batch carried several, and clears the whole thing
    /// once the last one lands — never showing a stuck "1 session uploading"
    /// after the wire went quiet.
    pub fn finish_one_upload(&mut self) {
        let Some(cur) = self.uploading.as_mut() else {
            return;
        };
        let per_batch = (cur.sessions / cur.uploads.max(1)).max(1);
        cur.uploads = cur.uploads.saturating_sub(1);
        cur.sessions = cur.sessions.saturating_sub(per_batch);
        if cur.uploads == 0 || cur.sessions == 0 {
            self.uploading = None;
        }
    }
    pub fn set_queue(&mut self, size: u64) {
        self.queue_size = size;
    }
    /// Increment a numeric stat by `n` (missing/non-numeric → starts at 0).
    pub fn bump_stat(&mut self, key: &str, n: u64) -> u64 {
        let cur = self.stats.get(key).and_then(Value::as_u64).unwrap_or(0);
        let next = cur + n;
        self.stats.insert(key.to_string(), json!(next));
        next
    }
    pub fn set_stat(&mut self, key: &str, value: Value) {
        self.stats.insert(key.to_string(), value);
    }
    pub fn note_event_at(&mut self, iso: impl Into<String>) {
        self.last_event_at = Some(iso.into());
    }
    pub fn set_update(&mut self, update: Option<UpdateInfo>) {
        self.update = update;
    }

    /// Mirror the stored auto-update preference into the status, read FRESH from
    /// `~/.modelstat/auto-update.json` (via env override → file). The daemon calls
    /// this right before each snapshot so a tray/CLI toggle shows up on the very
    /// next heartbeat + `last-status.json` write. Kept OUT of `snapshot_body` (which
    /// stays a pure serializer for the tests) and off the hot setters (it touches
    /// the filesystem).
    pub fn refresh_auto_update(&mut self) {
        self.auto_update = modelstat_update::auto_update_enabled();
    }

    /// The full snapshot body — the `last-status.json` mirror payload AND (minus
    /// `device_id`) the heartbeat wire body. `device_id` serializes to `null`
    /// when absent. Port of `snapshotBody`.
    pub fn snapshot_body(
        &self,
        device_id: Option<&str>,
        daemon_version: &str,
        machine_id: &str,
    ) -> Value {
        json!({
            "device_id": device_id,
            "status": self.phase.as_str(),
            "message": self.message,
            "busy_since_ms": self.busy_since_ms,
            "uploading": self.uploading.as_ref().map(|u| json!({
                "sessions": u.sessions,
                "uploads": u.uploads,
                "since_ms": u.since_ms,
            })),
            "progress_done": self.progress_done,
            "progress_total": self.progress_total,
            "queue_size": self.queue_size,
            "stats": self.stats,
            "last_event_at": self.last_event_at,
            "daemon_version": daemon_version,
            "machine_id": machine_id,
            "update": self.update.as_ref().map(|u| json!({ "verdict": u.verdict, "latest": u.latest })),
            "auto_update": self.auto_update,
        })
    }
}

/// The heartbeat WIRE body — `snapshot_body` with `device_id` stripped (the
/// server keys the device from the bearer, not the body). Port of the
/// `const { device_id:_omit, ...liveness } = local` in `sendHeartbeat`.
pub fn heartbeat_wire_body(snapshot: &Value) -> Value {
    let mut body = snapshot.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("device_id");
    }
    body
}

/// Atomically mirror the snapshot to `last-status.json` (tmp + rename), stamping
/// `written_at`. `written_at` is injected so daemon-main passes the real clock and
/// tests pin it. Port of `writeLocalStatus`.
pub fn write_last_status(path: &Path, snapshot: &Value, written_at: &str) -> std::io::Result<()> {
    let mut body = snapshot.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "written_at".to_string(),
            Value::String(written_at.to_string()),
        );
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, serde_json::to_string(&body)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_body_carries_every_field_and_device_id_null_when_absent() {
        let mut s = Status::default();
        s.set_phase(Phase::Scanning, "Scanning file 1/3");
        s.set_progress(1, 3);
        s.bump_stat("events_uploaded", 5);
        s.note_event_at("2026-07-16T10:00:00.000Z");
        s.set_update(Some(UpdateInfo {
            verdict: "upgrade_available".into(),
            latest: Some("1.2.3".into()),
        }));

        let body = s.snapshot_body(None, "9.9.9", "machine-abc");
        assert_eq!(body["status"], json!("scanning"));
        assert_eq!(body["message"], json!("Scanning file 1/3"));
        assert_eq!(body["progress_done"], json!(1));
        assert_eq!(body["progress_total"], json!(3));
        assert_eq!(body["stats"]["events_uploaded"], json!(5));
        assert_eq!(body["last_event_at"], json!("2026-07-16T10:00:00.000Z"));
        assert_eq!(body["daemon_version"], json!("9.9.9"));
        assert_eq!(body["machine_id"], json!("machine-abc"));
        assert_eq!(body["update"]["verdict"], json!("upgrade_available"));
        assert_eq!(body["update"]["latest"], json!("1.2.3"));
        assert_eq!(body["device_id"], Value::Null); // absent → null
    }

    #[test]
    fn heartbeat_body_strips_device_id_that_last_status_keeps() {
        let s = Status::default();
        let snap = s.snapshot_body(Some("dev-1"), "9.9.9", "m");
        assert_eq!(snap["device_id"], json!("dev-1")); // last-status keeps it
        let wire = heartbeat_wire_body(&snap);
        assert!(wire.get("device_id").is_none()); // heartbeat strips it
        assert_eq!(wire["status"], json!("starting"));
    }

    #[test]
    fn bump_stat_accumulates() {
        let mut s = Status::default();
        assert_eq!(s.bump_stat("batches", 1), 1);
        assert_eq!(s.bump_stat("batches", 2), 3);
        assert_eq!(s.stats["batches"], json!(3));
    }

    #[test]
    fn write_last_status_round_trips_atomically_with_written_at() {
        let dir = std::env::temp_dir().join(format!("modelstat-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("last-status.json");
        let s = Status::default();
        let snap = s.snapshot_body(Some("dev-1"), "9.9.9", "m");
        write_last_status(&path, &snap, "2026-07-16T10:00:00.000Z").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let read: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(read["device_id"], json!("dev-1"));
        assert_eq!(read["written_at"], json!("2026-07-16T10:00:00.000Z"));
        assert!(!path.with_extension("json.tmp").exists()); // tmp was renamed away
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_upload_gauge_counts_down_and_never_lingers() {
        let mut s = Status::default();
        assert!(s.uploading.is_none(), "quiet by default");

        // Cloud shape: one session per batch.
        s.start_upload_set(3, 3);
        let now = chrono::Utc::now().timestamp_millis();
        let u = s.uploading.clone().expect("in flight");
        assert_eq!((u.uploads, u.sessions), (3, 3));
        assert!(
            (u.since_ms - now).abs() < 5_000,
            "the clock starts with the set"
        );

        s.finish_one_upload();
        let u = s.uploading.clone().unwrap();
        assert_eq!((u.uploads, u.sessions), (2, 2));
        assert_eq!(u.since_ms, s.uploading.as_ref().unwrap().since_ms);
        s.finish_one_upload();
        s.finish_one_upload();
        assert!(
            s.uploading.is_none(),
            "the last commit clears it — a stuck '1 session uploading' is a lie"
        );

        // Local shape: one batch carrying several sessions.
        s.start_upload_set(1, 7);
        s.finish_one_upload();
        assert!(s.uploading.is_none());

        // A held fan-out says the remainder is not in flight.
        s.start_upload_set(4, 4);
        s.start_upload_set(0, 0);
        assert!(s.uploading.is_none());

        // Going idle takes the gauge with it.
        s.start_upload_set(2, 2);
        s.clear_busy();
        assert!(s.uploading.is_none());
    }

    #[test]
    fn snapshot_carries_the_upload_gauge_and_omits_it_when_quiet() {
        let mut s = Status::default();
        let body = s.snapshot_body(None, "daemon-1.0.0", "m1");
        assert_eq!(
            body["uploading"],
            Value::Null,
            "quiet → null, not a zero row"
        );

        s.start_upload_set(2, 5);
        let body = s.snapshot_body(None, "daemon-1.0.0", "m1");
        assert_eq!(body["uploading"]["uploads"], json!(2));
        assert_eq!(body["uploading"]["sessions"], json!(5));
        assert!(body["uploading"]["since_ms"].as_i64().unwrap() > 0);
    }
}
