//! The only ground truth that exists: minutes a human wrote down.
//!
//! Everything else in this crate is inferred from a diff. This module holds the
//! one input that is measured — a person saying "PR 412 took me about ninety
//! minutes" — and it is what separates [`crate::EffortUnits`] (always) from
//! [`crate::HoursEstimate`] (only with [`MIN_LABELS`] of these).
//!
//! Stored as JSON at a caller-supplied path, in the `anchors.json` idiom the
//! daemon already uses: `BTreeMap` so the file is byte-stable across writes,
//! tmp+rename so a crash cannot half-write it, and best-effort in both
//! directions — a missing or corrupt file reads as empty, a failed write is a
//! no-op. Losing labels costs re-labelling; it never costs a batch.
//!
//! Shape on disk:
//!
//! ```json
//! { "org/repo": { "412": { "minutes": 90, "labeled_at": "2026-08-09T10:00:00Z" } } }
//! ```
//!
//! Slug, PR number and an integer are all public-shape facts, so nothing here
//! is more sensitive than a git remote — but it is local-only regardless, and
//! this crate never opens a socket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Labels below which [`crate::calibrate_hours`] refuses to produce a
/// calibration, and therefore below which no hours exist anywhere in the API.
///
/// Eight is where leave-one-out stops being theatre: a two-parameter fit
/// refitted on seven points still has five degrees of freedom, so the held-out
/// prediction is genuinely out-of-sample rather than an echo. It is a floor on
/// *arithmetic*, not on trust — eight labels still buys a large
/// [`crate::Calibration::median_abs_pct_error`], which is exactly why that
/// number is published alongside every estimate.
pub const MIN_LABELS: usize = 8;

/// One human's answer for one PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// Minutes of work the labeller reports. Not wall clock, not `active_minutes`.
    pub minutes: u32,
    /// ISO-8601, supplied by the caller — this crate reads no clock, so its
    /// output is a pure function of its inputs and stays testable.
    pub labeled_at: String,
}

/// `repo_slug → pr_number → label`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LabelStore(BTreeMap<String, BTreeMap<u64, Label>>);

impl LabelStore {
    /// Read the store. A missing, unreadable, or corrupt file is an empty
    /// store: labels are an optional enrichment, and refusing to start because
    /// someone hand-edited the JSON badly would take Tier 1 down with Tier 2.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Atomic write (tmp + rename), best-effort. A failed write costs the
    /// labels added since the last successful one, never the caller's run.
    pub fn save(&self, path: &Path) {
        let Ok(text) = serde_json::to_string(self) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// Record (or overwrite) one label. Last write wins — a relabel is a
    /// correction, and keeping the stale one would quietly weight the fit
    /// toward whichever answer came first.
    pub fn add_label(&mut self, repo_slug: &str, pr_number: u64, minutes: u32, labeled_at: &str) {
        self.0.entry(repo_slug.to_string()).or_default().insert(
            pr_number,
            Label {
                minutes,
                labeled_at: labeled_at.to_string(),
            },
        );
    }

    /// This repo's labels, ascending by PR number. Empty when the repo has
    /// none — borrowed, so counting them costs nothing.
    pub fn labels_for_repo<'a>(&'a self, repo_slug: &str) -> impl Iterator<Item = (u64, &'a Label)> {
        self.0
            .get(repo_slug)
            .into_iter()
            .flatten()
            .map(|(pr, l)| (*pr, l))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "modelstat-effort-labels-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("labels.json")
    }

    #[test]
    fn round_trips_through_a_file_that_did_not_exist() {
        let path = scratch("roundtrip");
        assert_eq!(LabelStore::load(&path), LabelStore::default());

        let mut store = LabelStore::default();
        store.add_label("acme/api", 412, 90, "2026-08-09T10:00:00.000Z");
        store.add_label("acme/api", 7, 25, "2026-08-08T09:00:00.000Z");
        store.add_label("acme/web", 1, 300, "2026-08-01T09:00:00.000Z");
        store.save(&path);

        let read = LabelStore::load(&path);
        assert_eq!(read, store);
        let api: Vec<(u64, u32)> = read
            .labels_for_repo("acme/api")
            .map(|(pr, l)| (pr, l.minutes))
            .collect();
        assert_eq!(api, vec![(7, 25), (412, 90)], "ascending by PR number");
        assert_eq!(read.labels_for_repo("acme/nope").count(), 0);
        assert!(!path.with_extension("json.tmp").exists(), "tmp was renamed away");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_relabel_replaces_rather_than_accumulates() {
        let mut store = LabelStore::default();
        store.add_label("acme/api", 412, 90, "2026-08-09T10:00:00.000Z");
        store.add_label("acme/api", 412, 240, "2026-08-09T11:00:00.000Z");
        let labels: Vec<u32> = store.labels_for_repo("acme/api").map(|(_, l)| l.minutes).collect();
        assert_eq!(labels, vec![240]);
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_instead_of_exploding() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for junk in ["", "{", "not json at all", r#"{"acme/api": 7}"#, "[1,2,3]"] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(
                LabelStore::load(&path),
                LabelStore::default(),
                "corrupt payload {junk:?} must not panic or half-load"
            );
        }
        // And a good write over the corruption recovers cleanly.
        let mut store = LabelStore::default();
        store.add_label("acme/api", 1, 60, "2026-08-09T10:00:00.000Z");
        store.save(&path);
        assert_eq!(LabelStore::load(&path).labels_for_repo("acme/api").count(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unwritable_path_is_a_no_op_not_a_panic() {
        let mut store = LabelStore::default();
        store.add_label("acme/api", 1, 60, "2026-08-09T10:00:00.000Z");
        // A path whose parent cannot be created.
        store.save(Path::new("/proc/definitely-not/labels.json"));
    }

    #[test]
    fn the_on_disk_shape_is_slug_pr_minutes() {
        let mut store = LabelStore::default();
        store.add_label("acme/api", 412, 90, "2026-08-09T10:00:00.000Z");
        let json = serde_json::to_value(&store).unwrap();
        assert_eq!(json["acme/api"]["412"]["minutes"], serde_json::json!(90));
        assert_eq!(
            json["acme/api"]["412"]["labeled_at"],
            serde_json::json!("2026-08-09T10:00:00.000Z")
        );
    }
}
