//! Tier 1 — **relative effort units**. Always available, never hours.
//!
//! A unit is dimensionless and repo-local: `1.0` is this repo's median
//! human-authored PR, `2.0` is "about twice that", and neither number means a
//! duration. That restraint is not modesty, it is the only honest shape left
//! after measuring what local git actually knows.
//!
//! ## Why there is no minutes figure here
//!
//! The obvious ground truth — `AnchorPr::active_minutes`, mined by clustering a
//! PR's own commit timestamps into sittings — was measured against change size
//! on three real repositories and does not survive contact:
//!
//! | repo      | n  | Spearman ρ (active_minutes ↔ change size) |
//! |-----------|----|-------------------------------------------|
//! | erpc      | 27 | 0.11                                      |
//! | prism     | 58 | 0.24                                      |
//! | modelstat | 87 | 0.12                                      |
//!
//! Lines-of-code↔effort, the punchline of a hundred estimation papers, is about
//! 0.30. The interval clustering also saturates: on erpc the p50 and the p90 are
//! both exactly the 120-minute session ceiling, i.e. the statistic is mostly
//! reporting its own parameter. `active_minutes` stays on the wire as an
//! observation and drives **nothing** in this crate. If you are about to route
//! it into an hours estimate, that is the third time; read the table again.
//!
//! Real hours need real labels, which is [`crate::calibrate`] and Tier 2.
//!
//! ## The score
//!
//! One log-space sum, exponentiated against the repo's own median:
//!
//! ```text
//!   core    = 0.55·ln(1+weighted_churn) + 0.20·ln(1+files)
//!   target  = core + 0.15·max(0, ln(1+hunks) − ln(1+files)) + 0.10·ln(n_langs)
//!   judged  = 0.5·target + 0.5·quantile(anchor_cores, relative_position)
//!             + 0.5·(novelty − 0.5) − 0.5·(boilerplate − 0.5)
//!   units   = exp(score − median(anchor_cores))
//! ```
//!
//! Log space because effort-vs-size is sublinear and both axes are heavy
//! tailed; exponentiating a difference of logs makes `units` a ratio, which is
//! the only thing "relative" can honestly mean.
//!
//! An [`AnchorPr`] carries only `files_changed` and line counts, so the anchor
//! population is scored on `core` alone. The three target-only terms are all
//! non-negative refinements, and the class weighting below is one-directional:
//! it can push a generated-heavy PR DOWN the ranking, never up.
//!
//! The term weights sum to 1.0, so a PR that is `X`× larger in *every*
//! dimension at once is `X` units — the scheme is linear at its ceiling and
//! sublinear everywhere real work lives.
//!
//! ## What it does on a real repository
//!
//! Scored over 150 real commits with the population scored against itself
//! (every anchor `active_minutes: None`, i.e. the ordinary squash-merge case):
//!
//! ```text
//!   units    p05 0.17  p25 0.30  p50 1.14  p75 5.43  p95 11.4  max 122
//! ```
//!
//! Two things to read there. The median is `1.14`, not exactly `1.0`, because
//! real targets carry the scatter and language terms that anchors structurally
//! cannot — a PR of median SHAPE scores exactly `1.0` (there is a test), but
//! the median real PR is a little more scattered than its line counts alone
//! suggest. And the tail is long on purpose: the `max` there is a genuine
//! 42,000-line bulk change across hundreds of files, and a scoring scheme that
//! compressed it toward the median would be lying about the easy case to look
//! calm about the hard one.

use modelstat_wire::AnchorPr;
use serde::Serialize;

use crate::diff::DiffFeatures;
use crate::judge::JudgedFeatures;

/// How much of a line of churn is plausibly work, by [`crate::PathClass`].
///
/// Generated churn is not free — someone ran the generator, reviewed the
/// diff — but a 5,000-line lockfile regeneration is two orders of magnitude
/// away from 5,000 hand-written lines, and a scheme that cannot say so ranks
/// every dependency bump above every bug fix.
const W_TEST: f64 = 0.8;
const W_CONFIG: f64 = 0.5;
const W_DOC: f64 = 0.2;
const W_GENERATED: f64 = 0.02;

/// Log-space term weights. They sum to 1.0, which is what keeps `units` a ratio
/// of comparable magnitude rather than a number whose scale depends on how many
/// features happened to be available.
const K_CHURN: f64 = 0.55;
const K_FILES: f64 = 0.20;
const K_SCATTER: f64 = 0.15;
const K_LANG: f64 = 0.10;

/// How much of the judged score is the judge's placement rather than the
/// measured shape. Half: the judge sees structure the counts cannot express,
/// and is also a language model.
const JUDGE_BLEND: f64 = 0.5;

/// Judge adjustments, applied as log-space offsets from the neutral `0.5`.
/// Novel work is where estimates go wrong; boilerplate is where they go right.
const K_NOVELTY: f64 = 0.5;
const K_BOILERPLATE: f64 = 0.5;

/// A repo with no mined anchors still gets units, measured against a nominal
/// median PR rather than nothing.
// ponytail: the only two numbers in this crate not derived from the repo. They
// exist so a first-run repo produces a score instead of a hole; the moment one
// human anchor lands they are never consulted again.
const NOMINAL_MEDIAN_CHURN: f64 = 200.0;
const NOMINAL_MEDIAN_FILES: f64 = 5.0;

/// Clamps on the reported ratio, as log-space bounds applied before `exp` so a
/// pathological diff cannot produce an infinity.
const LN_UNITS_MIN: f64 = -4.6; // ≈ 0.01×
const LN_UNITS_MAX: f64 = 6.9; //  ≈ 1000×

/// Tier 1. Dimensionless, repo-relative, and always available.
///
/// `units` is a ratio against this repo's median human-authored PR:
/// `1.0` is typical, `3.0` is three times the typical PR's apparent size and
/// structure. It is **not** hours, minutes, or dollars, and there is no
/// conversion factor hiding in the crate — see [`crate::calibrate`] for the
/// only path to a duration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EffortUnits {
    pub units: f64,
    /// Where the target lands in the human-anchor population, `0..=1`.
    /// **Meaningless when `anchor_n == 0`**, where it is reported as `0.5`
    /// because there is no population to rank against.
    pub percentile_vs_human_anchors: f64,
    /// Whether a [`crate::Scorer`] answered. `false` means the score is churn,
    /// files, hunks and languages only.
    pub judged: bool,
    /// Human-authored anchors the normalization used. The honesty dial: at
    /// `0` the percentile is noise and `units` is measured against a nominal
    /// PR rather than this team's.
    pub anchor_n: usize,
}

/// Type-7 quantile with linear interpolation. `sorted` must be non-empty.
pub(crate) fn quantile(sorted: &[f64], q: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let pos = q.clamp(0.0, 1.0) * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(n - 1);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

/// Churn discounted by what kind of file it landed in. Pure.
///
/// Source churn is the remainder: [`crate::diff::features_from`] attributes
/// every file's churn to exactly one class and tracks the other four, so
/// `churn − (test + config + doc + generated)` is what a human plainly typed.
fn weighted_churn(f: &DiffFeatures) -> f64 {
    let churn = f.churn();
    let classified = f
        .test_lines
        .saturating_add(f.config_lines)
        .saturating_add(f.doc_lines)
        .saturating_add(f.generated_lines)
        .min(churn);
    let source = (churn - classified) as f64;
    source
        + W_TEST * f.test_lines as f64
        + W_CONFIG * f.config_lines as f64
        + W_DOC * f.doc_lines as f64
        + W_GENERATED * f.generated_lines as f64
}

/// The anchor-comparable half of the score: the only two features an
/// [`AnchorPr`] carries.
fn core(weighted_churn: f64, files: f64) -> f64 {
    K_CHURN * (1.0 + weighted_churn.max(0.0)).ln() + K_FILES * (1.0 + files.max(0.0)).ln()
}

/// Human-authored anchors' core scores, ascending.
///
/// Selected by AUTHORSHIP alone. The predecessor of this function also required
/// `active_minutes.is_some()`, which on a squash-merging repo is never true —
/// erpc has 50 human anchors and 0 with usable clustering — so the whole
/// baseline was empty on exactly the repos it was built for.
fn anchor_cores(anchors: &[AnchorPr]) -> Vec<f64> {
    let mut v: Vec<f64> = anchors
        .iter()
        .filter(|a| !a.ai_assisted)
        .map(|a| {
            let churn = a.lines_added.saturating_add(a.lines_deleted) as f64;
            core(churn, a.files_changed as f64)
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v
}

/// Empirical CDF of `sorted` at `x`, ties at their midpoint. `0.5` on an empty
/// population — there is nothing to rank against, and `anchor_n` says so.
fn percentile(sorted: &[f64], x: f64) -> f64 {
    if sorted.is_empty() {
        return 0.5;
    }
    let below = sorted.iter().filter(|v| **v < x).count() as f64;
    let equal = sorted.iter().filter(|v| **v == x).count() as f64;
    ((below + 0.5 * equal) / sorted.len() as f64).clamp(0.0, 1.0)
}

/// Score one PR in relative effort units. Pure, total, always answers.
pub fn effort_units(
    target: &DiffFeatures,
    judged: Option<&JudgedFeatures>,
    anchors: &[AnchorPr],
) -> EffortUnits {
    let pop = anchor_cores(anchors);
    let baseline = if pop.is_empty() {
        core(NOMINAL_MEDIAN_CHURN, NOMINAL_MEDIAN_FILES)
    } else {
        quantile(&pop, 0.5)
    };

    let files = target.files_changed as f64;
    let scatter = ((1.0 + target.hunks as f64).ln() - (1.0 + files).ln()).max(0.0);
    let langs = (target.languages.len().max(1) as f64).ln();
    let shape = core(weighted_churn(target), files) + K_SCATTER * scatter + K_LANG * langs;

    let score = match judged {
        Some(j) => {
            let placed = if pop.is_empty() {
                baseline
            } else {
                quantile(&pop, j.relative_position_0_1)
            };
            (1.0 - JUDGE_BLEND) * shape + JUDGE_BLEND * placed
                + K_NOVELTY * (j.novelty_0_1 - 0.5)
                - K_BOILERPLATE * (j.boilerplate_fraction_0_1 - 0.5)
        }
        None => shape,
    };
    let score = if score.is_finite() { score } else { baseline };

    EffortUnits {
        units: (score - baseline).clamp(LN_UNITS_MIN, LN_UNITS_MAX).exp(),
        percentile_vs_human_anchors: percentile(&pop, score),
        judged: judged.is_some(),
        anchor_n: pop.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::features_from;
    use crate::tests_support::{anchor, judged};

    /// Twelve human anchors, all 5 files / 200 churn — so the population median
    /// core is exactly `core(200, 5)` and a target of that shape is 1.0 units.
    fn flat_anchors(n: u64) -> Vec<AnchorPr> {
        (1..=n).map(|i| anchor(i, 5, 100, None)).collect()
    }

    #[test]
    fn the_median_human_anchor_is_one_unit() {
        // 100 added + 100 deleted across 5 source files: the anchors' own shape.
        let numstat = "20\t20\tsrc/a.rs\n20\t20\tsrc/b.rs\n20\t20\tsrc/c.rs\n\
                       20\t20\tsrc/d.rs\n20\t20\tsrc/e.rs\n";
        let u = effort_units(&features_from(numstat, ""), None, &flat_anchors(12));
        assert!((u.units - 1.0).abs() < 1e-9, "{u:?}");
        assert_eq!(u.anchor_n, 12);
        assert!(!u.judged);
    }

    #[test]
    fn units_are_monotonic_in_churn() {
        let anchors = flat_anchors(12);
        let mut last = 0.0;
        for lines in [10u64, 50, 200, 1_000, 5_000, 20_000] {
            let u = effort_units(
                &features_from(&format!("{lines}\t0\tsrc/a.rs\n"), ""),
                None,
                &anchors,
            );
            assert!(u.units > last, "{lines} lines gave {u:?}, not > {last}");
            last = u.units;
        }
    }

    #[test]
    fn monotonic_in_churn_with_a_judge_too() {
        let anchors = flat_anchors(12);
        let j = judged(0.5, 0.5, 0.5);
        let small = effort_units(&features_from("50\t0\tsrc/a.rs\n", ""), Some(&j), &anchors);
        let big = effort_units(&features_from("5000\t0\tsrc/a.rs\n", ""), Some(&j), &anchors);
        assert!(big.units > small.units, "{big:?} vs {small:?}");
        assert!(big.judged && small.judged);
    }

    #[test]
    fn a_five_thousand_line_lockfile_scores_below_a_two_hundred_line_change() {
        let anchors = flat_anchors(12);
        let lockfile = effort_units(
            &features_from("5000\t0\tpnpm-lock.yaml\n", ""),
            None,
            &anchors,
        );
        let logic = effort_units(
            &features_from(
                "100\t20\tsrc/consensus/vote.rs\n50\t10\tsrc/consensus/quorum.rs\n\
                 15\t5\tsrc/lib.rs\n",
                "",
            ),
            None,
            &anchors,
        );
        assert!(
            lockfile.units < logic.units,
            "5k of lockfile ({lockfile:?}) must not outrank 200 lines of logic ({logic:?})"
        );
        // And it is not a hair's breadth — the discount is the whole point.
        assert!(lockfile.units < 0.75 * logic.units, "{lockfile:?} vs {logic:?}");
    }

    #[test]
    fn docs_and_tests_are_discounted_but_not_erased() {
        let anchors = flat_anchors(12);
        let of = |path: &str| effort_units(&features_from(&format!("400\t0\t{path}\n"), ""), None, &anchors).units;
        let (src, test, config, doc, generated) = (
            of("src/a.rs"),
            of("tests/a_test.rs"),
            of("Cargo.toml"),
            of("README.md"),
            of("Cargo.lock"),
        );
        assert!(src > test && test > config && config > doc && doc > generated,
            "{src} {test} {config} {doc} {generated}");
        // Even a pure lockfile PR is worth something — someone ran the thing.
        assert!(generated > 0.0);
    }

    #[test]
    fn percentile_is_the_position_in_the_anchor_population() {
        // Twenty anchors of strictly increasing churn, one file each.
        let anchors: Vec<AnchorPr> = (1..=20).map(|i| anchor(i, 1, i * 50, None)).collect();
        // A one-source-file target with churn between anchor 15 (1500) and 16
        // (1600) outranks exactly 15 of the 20.
        let u = effort_units(&features_from("1550\t0\tsrc/a.rs\n", ""), None, &anchors);
        assert_eq!(u.anchor_n, 20);
        assert!((u.percentile_vs_human_anchors - 0.75).abs() < 1e-9, "{u:?}");

        let below_all = effort_units(&features_from("1\t0\tsrc/a.rs\n", ""), None, &anchors);
        assert_eq!(below_all.percentile_vs_human_anchors, 0.0);
        let above_all = effort_units(&features_from("99999\t0\tsrc/a.rs\n", ""), None, &anchors);
        assert_eq!(above_all.percentile_vs_human_anchors, 1.0);
    }

    #[test]
    fn ai_assisted_anchors_are_not_the_baseline() {
        let mut anchors = flat_anchors(12);
        for a in &mut anchors {
            a.ai_assisted = true;
        }
        let u = effort_units(&features_from("200\t0\tsrc/a.rs\n", ""), None, &anchors);
        assert_eq!(u.anchor_n, 0, "AI PRs are the thing being measured, not the ruler");
        assert_eq!(u.percentile_vs_human_anchors, 0.5);
    }

    #[test]
    fn no_anchors_still_produces_units_against_the_nominal_median() {
        let u = effort_units(&features_from("200\t0\tsrc/a.rs\n", ""), None, &[]);
        assert_eq!(u.anchor_n, 0);
        assert!(u.units.is_finite() && u.units > 0.0, "{u:?}");
        // 200 churn in one file is a shade under the nominal 200-churn/5-file PR.
        assert!(u.units > 0.5 && u.units < 1.0, "{u:?}");
    }

    #[test]
    fn scatter_and_language_spread_raise_the_score() {
        let anchors = flat_anchors(12);
        let numstat = "100\t0\tsrc/a.rs\n";
        let tight = effort_units(&features_from(numstat, "@@ -1,1 +1,2 @@\n"), None, &anchors);
        let scattered = effort_units(
            &features_from(numstat, &"@@ -1,1 +1,2 @@\n".repeat(30)),
            None,
            &anchors,
        );
        assert!(scattered.units > tight.units, "{scattered:?} vs {tight:?}");

        let one_lang = effort_units(&features_from("60\t0\tsrc/a.rs\n60\t0\tsrc/b.rs\n", ""), None, &anchors);
        let four_lang = effort_units(
            &features_from("60\t0\tsrc/a.rs\n60\t0\tsrc/b.go\n", ""),
            None,
            &anchors,
        );
        assert!(four_lang.units > one_lang.units, "{four_lang:?} vs {one_lang:?}");
    }

    #[test]
    fn the_judge_moves_the_score_in_the_direction_it_points() {
        let anchors: Vec<AnchorPr> = (1..=20).map(|i| anchor(i, 3, i * 40, None)).collect();
        let target = features_from("100\t0\tsrc/a.rs\n", "");
        let low = effort_units(&target, Some(&judged(0.05, 0.2, 0.8)), &anchors);
        let high = effort_units(&target, Some(&judged(0.95, 0.9, 0.1)), &anchors);
        assert!(high.units > low.units, "{high:?} vs {low:?}");
        // Both are still anchored to the repo: neither runs away.
        assert!(high.units < 20.0 && low.units > 0.01, "{high:?} {low:?}");
    }

    #[test]
    fn pathological_inputs_stay_finite() {
        let anchors = flat_anchors(6);
        let huge = features_from(&format!("{}\t{}\tsrc/a.rs\n", u64::MAX, u64::MAX), "");
        let u = effort_units(&huge, Some(&judged(1.0, 1.0, 0.0)), &anchors);
        assert!(u.units.is_finite() && u.units <= LN_UNITS_MAX.exp(), "{u:?}");
        let empty = effort_units(&DiffFeatures::default(), None, &anchors);
        assert!(empty.units.is_finite() && empty.units > 0.0, "{empty:?}");
    }
}
