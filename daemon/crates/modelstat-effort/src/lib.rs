//! On-device PR effort estimation that never reports a number it cannot
//! justify.
//!
//! ```text
//!   git show ──▶ DiffFeatures ─┬────────────────────────┐
//!                              │                        ▼
//!   repo's human AnchorPrs ────┼─▶ Scorer ──▶ Judged ─▶ EffortUnits   (Tier 1, always)
//!                              │                        │
//!   >= 8 hand-written labels ──┴─▶ Calibration ────────▶ HoursEstimate (Tier 2, sometimes)
//! ```
//!
//! ## Two tiers, and why the boundary is a hard one
//!
//! Local git contains no per-PR human-effort ground truth. That is a
//! measurement, not an opinion: per-PR commit clustering covers 0–17% of PRs on
//! real repositories because squash merging destroys branch history, and the
//! author-stream fallback that does reach ~100% coverage correlates with change
//! size at Spearman ρ 0.11–0.24 — below plain lines-of-code (~0.30) and
//! saturated at its own session ceiling. See [`units`] for the table.
//!
//! So the crate reports what it can measure and refuses what it cannot:
//!
//! * **Tier 1, [`EffortUnits`].** Always available, no labels needed.
//!   Dimensionless and repo-relative — `1.0` is this repo's median
//!   human-authored PR. Never call it hours, minutes, or dollars, because it
//!   isn't one.
//! * **Tier 2, [`HoursEstimate`].** Only with at least [`MIN_LABELS`] PRs a
//!   human labelled by hand ([`LabelStore`]), and always published next to the
//!   calibration's own leave-one-out error.
//!
//! Below the threshold the API does not degrade to a guess: [`EffortReport`]
//! carries `hours: None`. And the refusal is structural — [`Calibration`] and
//! [`HoursEstimate`] have private fields, no public constructor, and no
//! `Deserialize`, so the only way to hold hours is to have earned a
//! [`Calibration`] from real labels.
//!
//! ## Privacy
//!
//! The `Serialize` types are exactly the numeric report shapes:
//! [`EffortReport`], [`EffortUnits`], [`HoursEstimate`], [`Calibration`] — plus
//! [`LabelStore`], which never leaves the device. [`DiffFeatures`] and
//! [`JudgedFeatures`] deliberately do NOT implement it, so no source text,
//! path, or commit message can reach a wire through this crate. Paths are read
//! locally (the only way to tell a lockfile from a parser) and dropped.

pub mod calibrate;
pub mod diff;
pub mod judge;
pub mod labels;
pub mod units;

pub use calibrate::{calibrate_hours, estimate_hours, Calibration, HoursEstimate};
pub use diff::{classify_path, diff_features, parse_numstat, DiffFeatures, PathClass};
pub use judge::{build_prompt, parse_reply, JudgedFeatures, Scorer};
pub use labels::{Label, LabelStore, MIN_LABELS};
pub use units::{effort_units, EffortUnits};

use modelstat_wire::AnchorPr;
use serde::Serialize;

/// What one merged PR cost, at whatever fidelity the evidence supports.
///
/// `units` is always present. `hours` and `calibration` are `Some` together or
/// `None` together, and `None` is the normal case: it means nobody on this
/// device has labelled [`MIN_LABELS`] PRs yet, so the honest answer to "how
/// many hours?" is that we do not know.
///
/// Both `Option`s serialize as explicit `null` rather than being omitted. A
/// consumer must be able to SEE that hours are absent; a missing key reads as
/// an older schema, and reconstructing hours from `units` on the far side is
/// exactly the mistake the type is shaped to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EffortReport {
    pub units: EffortUnits,
    pub hours: Option<HoursEstimate>,
    pub calibration: Option<Calibration>,
}

/// Score one merged PR from the local repo.
///
/// `None` only when the repo cannot be read (bad `cwd`, unknown sha, git
/// timeout) — never for a missing or broken judge, which degrades to the
/// size-and-structure score, and never for missing labels, which degrade to
/// Tier 1 alone.
pub fn estimate_pr_effort(
    cwd: &str,
    merge_sha: &str,
    anchors: &[AnchorPr],
    scorer: Option<&dyn Scorer>,
    calibration: Option<&Calibration>,
) -> Option<EffortReport> {
    let target = diff::diff_features(cwd, merge_sha)?;
    Some(estimate_with(&target, anchors, scorer, calibration))
}

/// The judge-then-score half of [`estimate_pr_effort`], split out so the whole
/// decision path is exercisable without a repo on disk.
pub fn estimate_with(
    target: &DiffFeatures,
    anchors: &[AnchorPr],
    scorer: Option<&dyn Scorer>,
    calibration: Option<&Calibration>,
) -> EffortReport {
    let judged = scorer.and_then(|s| judge::judge(s, target, anchors));
    let units = units::effort_units(target, judged.as_ref(), anchors);
    EffortReport {
        // The invariant, in one line: hours exist iff a Calibration does.
        hours: calibration.map(|c| calibrate::estimate_hours(units.units, c)),
        calibration: calibration.copied(),
        units,
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use crate::judge::JudgedFeatures;
    use modelstat_wire::AnchorPr;

    /// A human-authored anchor with symmetric churn (`lines` added AND
    /// deleted, so `churn == 2 * lines`).
    ///
    /// `active_minutes` is still a parameter because [`AnchorPr`] still carries
    /// the field — it is an observation on the wire. Nothing in this crate
    /// reads it, and tests pass `None` unless they are asserting that.
    pub fn anchor(pr_number: u64, files: u32, lines: u64, active_minutes: Option<u32>) -> AnchorPr {
        AnchorPr {
            pr_number,
            merge_sha: format!("{pr_number:040x}"),
            merged_at: "2026-01-01T00:00:00.000Z".into(),
            files_changed: files,
            lines_added: lines,
            lines_deleted: lines,
            span_ms: Some(3_600_000),
            commit_count: Some(4),
            ai_assisted: false,
            active_minutes,
        }
    }

    pub fn judged(relative: f64, novelty: f64, boilerplate: f64) -> JudgedFeatures {
        JudgedFeatures {
            category: "feature".into(),
            novelty_0_1: novelty,
            boilerplate_fraction_0_1: boilerplate,
            risk_domains: vec![],
            relative_position_0_1: relative,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::anchor;
    use std::cell::RefCell;

    const NUMSTAT: &str = "\
180\t40\tsrc/consensus/vote.rs
60\t10\ttests/consensus.rs
4\t0\tCargo.toml
";
    const DIFF: &str = "\
diff --git a/src/consensus/vote.rs b/src/consensus/vote.rs
@@ -10,3 +10,6 @@ impl Voter {
+    let quorum = self.threshold();
-    let quorum = 3;
";

    fn repo_anchors(n: u64) -> Vec<AnchorPr> {
        (1..=n).map(|i| anchor(i, (i as u32).min(30), i * 30, None)).collect()
    }

    /// `n` labels on a plausible units→minutes law.
    fn labels(n: usize) -> Vec<(f64, u32)> {
        (0..n)
            .map(|i| {
                let u = 0.3 + i as f64 * 0.4;
                (u, (75.0 * u.powf(0.85)).round() as u32)
            })
            .collect()
    }

    const GOOD_REPLY: &str = r#"{"category":"feature","novelty_0_1":0.6,
        "boilerplate_fraction_0_1":0.2,"risk_domains":["consensus"],
        "relative_position_0_1":0.75}"#;

    #[test]
    fn seven_labels_gives_no_hours_and_eight_gives_hours() {
        let anchors = repo_anchors(20);
        let target = diff::features_from(NUMSTAT, DIFF);

        let seven = calibrate_hours(&labels(MIN_LABELS - 1));
        assert!(seven.is_none(), "seven labels must not calibrate");
        let below = estimate_with(&target, &anchors, None, seven.as_ref());
        assert!(below.hours.is_none() && below.calibration.is_none());
        // Tier 1 is unaffected — that is the point of two tiers.
        assert!(below.units.units > 0.0 && below.units.anchor_n == 20);

        let eight = calibrate_hours(&labels(MIN_LABELS)).expect("eight labels calibrate");
        let above = estimate_with(&target, &anchors, None, Some(&eight));
        let hours = above.hours.expect("eight labels must produce hours");
        assert!(hours.p10() <= hours.p50() && hours.p50() <= hours.p90(), "{hours:?}");
        assert_eq!(above.calibration.map(|c| c.n()), Some(MIN_LABELS));
        // Same PR, same units, both sides of the threshold.
        assert_eq!(above.units, below.units);
    }

    #[test]
    fn fake_scorer_drives_the_whole_path_with_no_network() {
        let anchors = repo_anchors(20);
        let target = diff::features_from(NUMSTAT, DIFF);
        let prompt_seen = RefCell::new(String::new());
        let scorer = |p: &str| -> Option<String> {
            prompt_seen.borrow_mut().push_str(p);
            Some(GOOD_REPLY.to_string())
        };

        let r = estimate_with(&target, &anchors, Some(&scorer), None);
        assert!(r.units.judged, "{r:?}");
        assert_eq!(r.units.anchor_n, 20);
        assert!(r.units.units > 0.0 && r.units.units.is_finite(), "{r:?}");
        assert!(r.hours.is_none(), "no labels, no hours — ever");

        let prompt = prompt_seen.borrow();
        assert!(prompt.contains("ref 1: files=1 +30/-30 commits=4"), "{prompt}");
        assert!(!prompt.contains("active="), "minutes must not reach the model:\n{prompt}");
        assert!(prompt.contains("files=3 +244/-50"), "{prompt}");
        assert!(prompt.contains("churn by kind: test=70 config=4 docs=0 generated=0 other=220"));
        for leak in ["vote.rs", "consensus/", "quorum", "threshold", "Voter"] {
            assert!(!prompt.contains(leak), "prompt leaked {leak:?}:\n{prompt}");
        }
    }

    #[test]
    fn a_missing_broken_or_silent_judge_degrades_to_the_unjudged_score() {
        let anchors = repo_anchors(20);
        let target = diff::features_from(NUMSTAT, DIFF);
        let silent = |_: &str| -> Option<String> { None };
        let broken = |_: &str| -> Option<String> { Some("I'd rather not.".to_string()) };

        let expected = estimate_with(&target, &anchors, None, None);
        assert!(!expected.units.judged);
        for scorer in [None, Some(&silent as &dyn Scorer), Some(&broken as &dyn Scorer)] {
            let r = estimate_with(&target, &anchors, scorer, None);
            assert_eq!(r, expected, "every degraded path lands on the same units");
        }
    }

    #[test]
    fn a_thin_baseline_still_produces_units_and_says_so() {
        let target = diff::features_from(NUMSTAT, DIFF);
        let scorer = |_: &str| -> Option<String> { Some(GOOD_REPLY.to_string()) };
        let r = estimate_with(&target, &repo_anchors(4), Some(&scorer), None);
        assert_eq!(r.units.anchor_n, 4);
        assert!(!r.units.judged, "four anchors is nothing to place against");
        assert!(r.units.units > 0.0);
    }

    #[test]
    fn the_report_serializes_as_units_hours_calibration_with_explicit_nulls() {
        let anchors = repo_anchors(20);
        let target = diff::features_from(NUMSTAT, DIFF);

        let bare = serde_json::to_value(estimate_with(&target, &anchors, None, None)).unwrap();
        let keys: Vec<&str> = bare.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["units", "hours", "calibration"]);
        assert_eq!(bare["hours"], serde_json::json!(null));
        assert_eq!(bare["calibration"], serde_json::json!(null));
        let unit_keys: Vec<&str> = bare["units"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            unit_keys,
            vec!["units", "percentile_vs_human_anchors", "judged", "anchor_n"]
        );

        let cal = calibrate_hours(&labels(MIN_LABELS)).unwrap();
        let full = serde_json::to_value(estimate_with(&target, &anchors, None, Some(&cal))).unwrap();
        let hkeys: Vec<&str> = full["hours"].as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(hkeys, vec!["p10", "p50", "p90"]);
        let ckeys: Vec<&str> = full["calibration"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            ckeys,
            vec!["scale", "exponent", "n", "median_abs_pct_error", "spearman_rho"]
        );

        // No minutes key anywhere: the wire says units or it says hours.
        let text = serde_json::to_string(&full).unwrap();
        assert!(!text.contains("minutes"), "{text}");
    }

    #[test]
    fn unreadable_repo_is_none_not_a_panic() {
        assert!(
            estimate_pr_effort("/nope-modelstat-effort", "HEAD", &repo_anchors(20), None, None)
                .is_none()
        );
    }
}
