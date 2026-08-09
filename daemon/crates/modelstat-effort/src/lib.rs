//! On-device PR effort estimation, calibrated on the repo's own history.
//!
//! The question this answers is "how long would a human have taken on this?",
//! and the only defensible answer is one drawn from how long humans on THIS
//! team actually took. So the pipeline is:
//!
//! ```text
//!   git show ──▶ DiffFeatures ─┐
//!                              ├─▶ Scorer ──▶ JudgedFeatures ─┐
//!   repo's human AnchorPrs ────┘   (relative placement only)   ├─▶ EffortEstimate
//!                              └────────────────────────────────┘
//!                                   (size_prior when no judge)
//! ```
//!
//! * [`diff`] reads the commit locally. Paths are classified and dropped.
//! * [`judge`] asks an INJECTED [`Scorer`] to place the PR among 5–8 of the
//!   repo's real human PRs. It never asks for hours, and this crate never opens
//!   a socket.
//! * [`calibrate`] maps that placement onto the anchors' measured
//!   `active_minutes`, widening the interval for anchor scarcity and judge
//!   uncertainty — and refusing to sound confident below five anchors.
//!
//! Privacy: the only [`serde::Serialize`] types here are [`EffortEstimate`] and
//! [`Confidence`] — five integers and an enum. [`DiffFeatures`] and
//! [`JudgedFeatures`] deliberately do not implement it, so no source text,
//! path, or commit message can reach a wire through this crate.

pub mod calibrate;
pub mod diff;
pub mod judge;

pub use calibrate::{
    estimate, estimate_from_size, size_prior, Confidence, EffortEstimate, MIN_ANCHORS,
};
pub use diff::{classify_path, diff_features, parse_numstat, DiffFeatures, PathClass};
pub use judge::{build_prompt, parse_reply, JudgedFeatures, Scorer};

use modelstat_wire::AnchorPr;

/// Estimate the human effort one merged PR represents, in minutes, as an
/// interval calibrated on `anchors`.
///
/// `None` only when the local repo cannot be read (bad `cwd`, unknown sha, git
/// timeout) — never for a missing or broken judge, which degrades to
/// [`size_prior`] instead.
pub fn estimate_pr_effort(
    cwd: &str,
    merge_sha: &str,
    anchors: &[AnchorPr],
    scorer: Option<&dyn Scorer>,
) -> Option<EffortEstimate> {
    let target = diff::diff_features(cwd, merge_sha)?;
    Some(estimate_with(&target, anchors, scorer))
}

/// The judge-then-calibrate half of [`estimate_pr_effort`], split out so the
/// whole decision path is exercisable without a repo on disk.
pub fn estimate_with(
    target: &DiffFeatures,
    anchors: &[AnchorPr],
    scorer: Option<&dyn Scorer>,
) -> EffortEstimate {
    match scorer.and_then(|s| judge::judge(s, target, anchors)) {
        Some(j) => calibrate::estimate(anchors, &j, target),
        None => calibrate::estimate_from_size(anchors, target),
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use crate::judge::JudgedFeatures;
    use modelstat_wire::AnchorPr;

    /// A human-authored anchor with symmetric churn (`lines` added AND
    /// deleted, so `churn == 2 * lines`).
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
        (1..=n)
            .map(|i| anchor(i, (i as u32).min(30), i * 30, Some(i as u32 * 25)))
            .collect()
    }

    const GOOD_REPLY: &str = r#"{"category":"feature","novelty_0_1":0.6,
        "boilerplate_fraction_0_1":0.2,"risk_domains":["consensus"],
        "relative_position_0_1":0.75}"#;

    #[test]
    fn fake_scorer_drives_the_whole_path_with_no_network() {
        let anchors = repo_anchors(20);
        let target = diff::features_from(NUMSTAT, DIFF);
        let prompt_seen = RefCell::new(String::new());
        let scorer = |p: &str| -> Option<String> {
            prompt_seen.borrow_mut().push_str(p);
            Some(GOOD_REPLY.to_string())
        };

        let e = estimate_with(&target, &anchors, Some(&scorer));

        // 0.75 of a 25..500-minute distribution, interpolated: s[14]=375,
        // s[15]=400, pos = 0.75*19 = 14.25 ⇒ 381.25.
        assert_eq!(e.p50_minutes, 381);
        assert_eq!(e.anchor_n, 20);
        assert_eq!(e.confidence, Confidence::Moderate);
        assert!(e.p10_minutes < e.p50_minutes && e.p50_minutes < e.p90_minutes, "{e:?}");

        let prompt = prompt_seen.borrow();
        assert!(prompt.contains("ref 1: files=1 +30/-30 commits=4 active=25min"), "{prompt}");
        assert!(prompt.contains("files=3 +244/-50"), "{prompt}");
        assert!(prompt.contains("churn by kind: test=70 config=4 docs=0 generated=0 other=220"));
        for leak in ["vote.rs", "consensus/", "quorum", "threshold", "Voter"] {
            assert!(!prompt.contains(leak), "prompt leaked {leak:?}:\n{prompt}");
        }
    }

    #[test]
    fn a_missing_broken_or_silent_judge_degrades_to_the_size_prior() {
        let anchors = repo_anchors(20);
        let target = diff::features_from(NUMSTAT, DIFF);
        let silent = |_: &str| -> Option<String> { None };
        let broken = |_: &str| -> Option<String> { Some("I'd rather not.".to_string()) };

        let expected = estimate_from_size(&anchors, &target);
        for scorer in [None, Some(&silent as &dyn Scorer), Some(&broken as &dyn Scorer)] {
            let e = estimate_with(&target, &anchors, scorer);
            assert_eq!(e, expected, "every degraded path lands on the size prior");
            assert_eq!(e.confidence, Confidence::Low);
            assert!(e.p10_minutes <= e.p50_minutes && e.p50_minutes <= e.p90_minutes);
        }
    }

    #[test]
    fn a_thin_baseline_is_insufficient_even_with_a_perfect_judge() {
        let target = diff::features_from(NUMSTAT, DIFF);
        let scorer = |_: &str| -> Option<String> { Some(GOOD_REPLY.to_string()) };
        let e = estimate_with(&target, &repo_anchors(4), Some(&scorer));
        assert_eq!(e.confidence, Confidence::Insufficient);
        assert_eq!(e.anchor_n, 4);
        assert!(e.p90_minutes >= e.p10_minutes * 8, "{e:?}");
    }

    #[test]
    fn unreadable_repo_is_none_not_a_panic() {
        assert!(estimate_pr_effort("/nope-modelstat-effort", "HEAD", &repo_anchors(20), None).is_none());
    }
}
