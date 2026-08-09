//! Turning a relative placement into minutes — pure, no I/O, no model.
//!
//! The numbers come from the repo, not from a judge and not from a vendor
//! table: [`estimate`] reads `relative_position_0_1` as a QUANTILE of this
//! repo's own human-authored `active_minutes` distribution. A `0.8` placement
//! on a team whose hard PRs run four hours means four hours; on a team whose
//! hard PRs run three days it means three days. The model never had to know.
//!
//! Two rules keep it honest:
//!
//! * **Thin baselines do not get a number.** Below [`MIN_ANCHORS`] usable
//!   anchors the result is [`Confidence::Insufficient`] with a deliberately
//!   3×-either-side interval. A confident median drawn from four samples is
//!   worse than no answer, because it looks like an answer.
//! * **Width is earned.** The interval starts at the repo's own observed
//!   spread and widens with anchor scarcity and judge uncertainty. It is never
//!   a fixed ±30%.
//!
//! One consequence is worth stating plainly: a repo that squash-merges
//! everything has no effort ground truth at all. A squash merge has a single
//! parent, so there is no branch commit range to cluster into work sessions,
//! so every anchor arrives with `active_minutes: None` however many human PRs
//! the repo has merged. Such a repo gets [`Confidence::Insufficient`] and a
//! size-only interval — NOT an effort figure back-derived from `span_ms`.
//! Wall-clock is the measure this whole engine exists to replace, and it does
//! not become a measure of effort by being the only number left.

use modelstat_wire::AnchorPr;
use serde::{Deserialize, Serialize};

use crate::diff::DiffFeatures;
use crate::judge::JudgedFeatures;

/// Usable human anchors below which no confident estimate is produced.
pub const MIN_ANCHORS: usize = 5;

/// Ceiling on any reported minute value (~10 work-weeks). Past this the answer
/// is "nobody knows", and an unbounded extrapolation only hides that.
const MAX_MINUTES: f64 = 100_000.0;

/// Half-width, in log space, of the [`Confidence::Insufficient`] interval:
/// `ln(3)` ⇒ `[centre/3, centre*3]`.
const INSUFFICIENT_SPREAD: f64 = 3.0;

/// Floor and ceiling on the repo-derived base half-width. The floor stops a
/// freakishly uniform anchor set from claiming ±5% precision; the ceiling stops
/// one outlier PR from making every interval useless.
const BASE_W_MIN: f64 = 0.35;
const BASE_W_MAX: f64 = 1.6;

/// Scarcity multiplier is `1 + SCARCITY_K / n`: 2.2× at five anchors, 1.3× at
/// twenty, 1.12× at fifty. Anchors are the whole evidence base, so their count
/// is the dominant term.
const SCARCITY_K: f64 = 6.0;

/// Judge uncertainty (`0..=1`) adds up to this much width.
const UNCERTAINTY_GAIN: f64 = 0.5;

/// Overall cap on the half-width (`p90/p10 ≈ 20×`).
const W_MAX: f64 = 1.5;

/// Uncertainty assumed when there is no judge at all — the size prior knows
/// churn and nothing else.
const FALLBACK_UNCERTAINTY: f64 = 1.0;

/// Points needed before the log-log regression is trusted over a plain median.
const MIN_REGRESSION_POINTS: usize = 3;

/// Slope clamp for `ln(minutes) ~ ln(1 + churn)`. Negative slopes ("bigger PRs
/// are quicker") are an artefact of a small sample, never a finding; above 1.5
/// the fit has latched onto one huge PR.
const SLOPE_MIN: f64 = 0.0;
const SLOPE_MAX: f64 = 1.5;

/// With no anchors at all: a flat "~an hour per hundred lines, floor a quarter
/// hour, ceiling a working day".
// ponytail: a literal industry rule of thumb, and the only number in this crate
// not derived from the repo. It exists so a first-run repo gets *something*;
// the moment five anchors land it is never consulted again.
const NO_ANCHOR_BASE_MINUTES: f64 = 15.0;
const NO_ANCHOR_MINUTES_PER_LINE: f64 = 0.6;
const NO_ANCHOR_CEILING: f64 = 480.0;

/// How much the baseline can be trusted. Ordered — `Insufficient < Good` — so
/// a degraded path can cap it with [`Ord::min`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Fewer than [`MIN_ANCHORS`] usable human anchors. Read the interval, not
    /// the median.
    Insufficient,
    Low,
    Moderate,
    Good,
}

impl Confidence {
    fn from_anchor_count(n: usize) -> Self {
        match n {
            0..=4 => Confidence::Insufficient,
            5..=11 => Confidence::Low,
            12..=24 => Confidence::Moderate,
            _ => Confidence::Good,
        }
    }
}

/// The answer. Five integers and an enum — no paths, no source, no messages,
/// nothing derived from a commit body. This is the only `Serialize` type the
/// crate exposes, and that is deliberate: it is the only one that could ever be
/// safe to transmit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortEstimate {
    pub p10_minutes: u32,
    pub p50_minutes: u32,
    pub p90_minutes: u32,
    /// Usable human anchors this was calibrated on. Ships with the estimate so
    /// a reader can discount it without having to trust `confidence` alone.
    pub anchor_n: usize,
    pub confidence: Confidence,
}

/// Human-authored anchors' measured effort, ascending. AI-assisted PRs are
/// excluded: they are the thing being measured, so folding them into the
/// baseline would quietly define AI speedup as zero.
fn human_minutes(anchors: &[AnchorPr]) -> Vec<f64> {
    let mut v: Vec<f64> = anchors
        .iter()
        .filter(|a| !a.ai_assisted)
        .filter_map(|a| a.active_minutes.filter(|m| *m > 0).map(f64::from))
        .collect();
    v.sort_by(f64::total_cmp);
    v
}

/// Type-7 quantile with linear interpolation. `sorted` must be non-empty.
fn quantile(sorted: &[f64], q: f64) -> f64 {
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

fn clamp_minutes(x: f64) -> f64 {
    if x.is_finite() {
        x.clamp(1.0, MAX_MINUTES)
    } else {
        1.0
    }
}

fn round_u32(x: f64) -> u32 {
    clamp_minutes(x).round() as u32
}

/// Assemble the estimate from a centre and a log-space half-width.
///
/// Working in log space is what guarantees `0 < p10 ≤ p50 ≤ p90`: the interval
/// is multiplicative, so it cannot cross zero however wide it gets. The
/// `.max()` chain re-establishes the ordering after rounding, where a narrow
/// interval could otherwise collapse (`p10=0.6, p50=1.0` both round to 1).
fn build(centre: f64, half_width: f64, anchor_n: usize, confidence: Confidence) -> EffortEstimate {
    let w = half_width.max(0.0);
    let p50 = clamp_minutes(centre);
    let p10 = round_u32(p50 * (-w).exp());
    let p50r = round_u32(p50).max(p10);
    let p90 = round_u32(p50 * w.exp()).max(p50r);
    EffortEstimate {
        p10_minutes: p10,
        p50_minutes: p50r,
        p90_minutes: p90,
        anchor_n,
        confidence,
    }
}

/// The interval half-width for a repo with `samples.len()` anchors and a judge
/// of the given uncertainty.
fn half_width(samples: &[f64], uncertainty: f64) -> f64 {
    // Start from what this repo's own PRs actually do: the p10..p90 ratio of
    // the anchor distribution, halved into a log half-width.
    let lo = quantile(samples, 0.1).max(1.0);
    let hi = quantile(samples, 0.9).max(lo);
    let base = ((hi / lo).ln() / 2.0).clamp(BASE_W_MIN, BASE_W_MAX);
    let scarcity = 1.0 + SCARCITY_K / samples.len() as f64;
    let judge = 1.0 + UNCERTAINTY_GAIN * uncertainty.clamp(0.0, 1.0);
    (base * scarcity * judge).min(W_MAX)
}

/// Deliberately wide, explicitly unconfident.
fn insufficient(centre: f64, anchor_n: usize) -> EffortEstimate {
    build(
        centre,
        INSUFFICIENT_SPREAD.ln(),
        anchor_n,
        Confidence::Insufficient,
    )
}

/// Place a judged PR on this repo's own effort distribution.
///
/// With fewer than [`MIN_ANCHORS`] usable human anchors the judge's placement
/// is ignored entirely — there is nothing to place it *on* — and the result
/// falls back to [`size_prior`] at [`Confidence::Insufficient`].
pub fn estimate(
    anchors: &[AnchorPr],
    judged: &JudgedFeatures,
    target: &DiffFeatures,
) -> EffortEstimate {
    let samples = human_minutes(anchors);
    if samples.len() < MIN_ANCHORS {
        return insufficient(size_prior(target, anchors), samples.len());
    }
    let centre = quantile(&samples, judged.relative_position_0_1);
    build(
        centre,
        half_width(&samples, judged.uncertainty()),
        samples.len(),
        Confidence::from_anchor_count(samples.len()),
    )
}

/// The no-judge path: centre the interval on [`size_prior`] directly.
///
/// The prior is already in minutes and already bounded by what the repo has
/// actually done, so it is used as-is rather than re-projected onto the
/// anchors' empirical distribution. Round-tripping it through the CDF would
/// snap every answer back inside the observed range — a PR seven times larger
/// than anything in the baseline would be reported as costing exactly as much
/// as the largest anchor, which is the one thing we know it does not.
///
/// Capped at [`Confidence::Low`] however many anchors there are. Churn is a
/// weak predictor of effort — that is the entire reason the judge exists — so a
/// size-only answer never gets to look well-supported.
pub fn estimate_from_size(anchors: &[AnchorPr], target: &DiffFeatures) -> EffortEstimate {
    let samples = human_minutes(anchors);
    let centre = size_prior(target, anchors);
    if samples.len() < MIN_ANCHORS {
        return insufficient(centre, samples.len());
    }
    build(
        centre,
        half_width(&samples, FALLBACK_UNCERTAINTY),
        samples.len(),
        Confidence::from_anchor_count(samples.len()).min(Confidence::Low),
    )
}

/// Minutes predicted from churn alone, by log-log OLS over the repo's human
/// anchors: `ln(minutes) = a + b·ln(1 + added + deleted)`. Pure.
///
/// Logs, not raw lines, because effort-vs-size is sublinear and both axes are
/// heavy-tailed — a linear fit is dictated by the single biggest PR, and can
/// predict negative minutes. Logs also make the prediction positive by
/// construction.
///
/// The target is measured by RAW churn even though [`DiffFeatures`] knows which
/// lines were generated: an [`AnchorPr`] carries no path information, so the
/// anchors could not be discounted the same way, and discounting only one side
/// of a regression biases every prediction downward.
pub fn size_prior(target: &DiffFeatures, anchors: &[AnchorPr]) -> f64 {
    let churn = target.churn() as f64;
    let x = (churn + 1.0).ln();
    let pts: Vec<(f64, f64)> = anchors
        .iter()
        .filter(|a| !a.ai_assisted)
        .filter_map(|a| {
            let m = a.active_minutes.filter(|m| *m > 0)?;
            let anchor_churn = a.lines_added.saturating_add(a.lines_deleted) as f64;
            Some(((anchor_churn + 1.0).ln(), f64::from(m).ln()))
        })
        .collect();

    if pts.is_empty() {
        return (NO_ANCHOR_BASE_MINUTES + NO_ANCHOR_MINUTES_PER_LINE * churn)
            .clamp(NO_ANCHOR_BASE_MINUTES, NO_ANCHOR_CEILING);
    }

    let n = pts.len() as f64;
    let ybar = pts.iter().map(|p| p.1).sum::<f64>() / n;
    if pts.len() < MIN_REGRESSION_POINTS {
        // Too few points to see a slope; the geometric mean of what we have is
        // the honest answer, and it ignores `churn` rather than pretending to.
        return clamp_minutes(ybar.exp());
    }

    let xbar = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let sxx: f64 = pts.iter().map(|p| (p.0 - xbar).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - xbar) * (p.1 - ybar)).sum();
    if sxx <= 1e-12 {
        return clamp_minutes(ybar.exp());
    }
    let b = (sxy / sxx).clamp(SLOPE_MIN, SLOPE_MAX);
    let a = ybar - b * xbar;
    let pred = (a + b * x).exp();

    // Never extrapolate wildly past what the repo has actually done.
    let observed: Vec<f64> = pts.iter().map(|p| p.1.exp()).collect();
    let lo = observed.iter().copied().fold(f64::INFINITY, f64::min) * 0.5;
    let hi = observed.iter().copied().fold(0.0_f64, f64::max) * 3.0;
    clamp_minutes(pred.clamp(lo.min(hi), hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{anchor, judged};

    /// Twenty human anchors, 30..600 active minutes, churn tracking minutes.
    fn baseline(n: u64) -> Vec<AnchorPr> {
        (1..=n)
            .map(|i| anchor(i, (i as u32).min(40), i * 25, Some(i as u32 * 30)))
            .collect()
    }

    #[test]
    fn interval_is_ordered_and_positive_everywhere() {
        let anchors = baseline(20);
        let target = DiffFeatures::default();
        for n_anchors in [0, 1, 4, 5, 12, 20] {
            let set = baseline(n_anchors);
            for rel in [0.0, 0.01, 0.25, 0.5, 0.75, 0.99, 1.0] {
                for (nov, boil) in [(0.0, 1.0), (0.5, 0.5), (1.0, 0.0)] {
                    let e = estimate(&set, &judged(rel, nov, boil), &target);
                    assert!(
                        e.p10_minutes <= e.p50_minutes && e.p50_minutes <= e.p90_minutes,
                        "n={n_anchors} rel={rel}: {e:?}"
                    );
                    assert!(e.p10_minutes >= 1, "{e:?}");
                }
            }
        }
        // And on the degenerate distribution where every anchor is identical.
        let flat: Vec<AnchorPr> = (1..=8).map(|i| anchor(i, 3, 100, Some(90))).collect();
        let e = estimate(&flat, &judged(0.5, 0.5, 0.5), &target);
        assert!(e.p10_minutes < e.p50_minutes && e.p50_minutes < e.p90_minutes, "{e:?}");
        assert_eq!(e.p50_minutes, 90);
        let _ = anchors;
    }

    #[test]
    fn p50_is_monotone_in_relative_position() {
        let anchors = baseline(20);
        let target = DiffFeatures::default();
        let mut last = 0u32;
        for step in 0..=20 {
            let rel = step as f64 / 20.0;
            let e = estimate(&anchors, &judged(rel, 0.4, 0.4), &target);
            assert!(
                e.p50_minutes >= last,
                "rel={rel} gave {} after {last}",
                e.p50_minutes
            );
            last = e.p50_minutes;
        }
        // And it actually spans the repo's range rather than hugging the median.
        assert_eq!(estimate(&anchors, &judged(0.0, 0.4, 0.4), &target).p50_minutes, 30);
        assert_eq!(estimate(&anchors, &judged(1.0, 0.4, 0.4), &target).p50_minutes, 600);
    }

    #[test]
    fn a_squash_only_repo_has_no_effort_ground_truth() {
        // The erpc shape: 13 human-authored anchors, every one squash-merged,
        // so every `active_minutes` is None even though `span_ms` is present.
        let squashed: Vec<AnchorPr> = (1..=13)
            .map(|i| AnchorPr {
                span_ms: Some(15_512_000),
                ..anchor(i, 20, i * 200, None)
            })
            .collect();
        let target = DiffFeatures {
            lines_added: 8_967,
            lines_deleted: 2_604,
            ..Default::default()
        };
        for e in [
            estimate(&squashed, &judged(0.8, 0.6, 0.2), &target),
            estimate_from_size(&squashed, &target),
        ] {
            assert_eq!(e.confidence, Confidence::Insufficient, "{e:?}");
            assert_eq!(e.anchor_n, 0, "wall-clock anchors are not effort anchors: {e:?}");
            assert!(e.p10_minutes < e.p50_minutes && e.p50_minutes < e.p90_minutes, "{e:?}");
            assert!(e.p90_minutes >= e.p10_minutes * 8, "{e:?}");
        }
        // The judge is never consulted either — there is nothing to place against.
        assert!(crate::judge::reference_anchors(&squashed).is_empty());
    }

    #[test]
    fn fewer_than_five_anchors_never_returns_a_confident_number() {
        let target = DiffFeatures::default();
        for n in 0..MIN_ANCHORS as u64 {
            let e = estimate(&baseline(n), &judged(0.5, 0.2, 0.8), &target);
            assert_eq!(e.confidence, Confidence::Insufficient, "n={n}: {e:?}");
            assert_eq!(e.anchor_n, n as usize);
            // 3× either side of the centre, so nobody reads the median as fact.
            assert!(
                e.p90_minutes >= e.p10_minutes * 8,
                "n={n} interval too tight: {e:?}"
            );
        }
        // Five is the first count that earns a real answer.
        let e = estimate(&baseline(5), &judged(0.5, 0.2, 0.8), &target);
        assert_eq!(e.confidence, Confidence::Low);
        assert_eq!(e.anchor_n, 5);
    }

    #[test]
    fn ai_assisted_anchors_are_not_a_baseline() {
        let ai: Vec<AnchorPr> = (1..=20)
            .map(|i| AnchorPr {
                ai_assisted: true,
                ..anchor(i, 5, i * 25, Some(i as u32 * 30))
            })
            .collect();
        let e = estimate(&ai, &judged(0.5, 0.5, 0.5), &DiffFeatures::default());
        assert_eq!(e.confidence, Confidence::Insufficient);
        assert_eq!(e.anchor_n, 0);
    }

    #[test]
    fn anchors_without_measured_effort_are_skipped() {
        let mut set = baseline(6);
        set.extend((100..110).map(|i| anchor(i, 5, 500, None)));
        let e = estimate(&set, &judged(0.5, 0.5, 0.5), &DiffFeatures::default());
        assert_eq!(e.anchor_n, 6);
    }

    #[test]
    fn confidence_rises_with_the_anchor_count() {
        let target = DiffFeatures::default();
        let conf = |n| estimate(&baseline(n), &judged(0.5, 0.5, 0.5), &target).confidence;
        assert_eq!(conf(4), Confidence::Insufficient);
        assert_eq!(conf(8), Confidence::Low);
        assert_eq!(conf(15), Confidence::Moderate);
        assert_eq!(conf(30), Confidence::Good);
    }

    #[test]
    fn scarcity_and_judge_uncertainty_widen_the_interval() {
        let target = DiffFeatures::default();
        let width = |set: &[AnchorPr], nov, boil| {
            let e = estimate(set, &judged(0.5, nov, boil), &target);
            e.p90_minutes as f64 / e.p10_minutes as f64
        };
        let many = baseline(40);
        let few = baseline(6);
        assert!(
            width(&few, 0.5, 0.5) > width(&many, 0.5, 0.5),
            "scarcity must widen"
        );
        // Novel + zero boilerplate is the least predictable combination.
        assert!(
            width(&many, 1.0, 0.0) > width(&many, 0.0, 1.0),
            "judge uncertainty must widen"
        );
    }

    #[test]
    fn size_prior_recovers_a_known_power_law() {
        // minutes = 2 * churn^0.8, churn 40..4000.
        let anchors: Vec<AnchorPr> = (1..=25)
            .map(|i| {
                let churn = 40 * i;
                let minutes = (2.0 * (churn as f64).powf(0.8)).round() as u32;
                anchor(i, 5, churn / 2, Some(minutes))
            })
            .collect();
        let target = DiffFeatures {
            lines_added: 400,
            lines_deleted: 400,
            ..Default::default()
        };
        let expected = 2.0 * 800f64.powf(0.8);
        let got = size_prior(&target, &anchors);
        assert!(
            (got - expected).abs() / expected < 0.2,
            "size_prior {got:.1} vs {expected:.1}"
        );
    }

    #[test]
    fn size_prior_degrades_without_blowing_up() {
        let target = DiffFeatures {
            lines_added: 300,
            lines_deleted: 100,
            ..Default::default()
        };
        // No anchors: the flat rule of thumb, bounded.
        let none = size_prior(&target, &[]);
        assert!((15.0..=480.0).contains(&none), "{none}");
        // Two anchors: their geometric mean, not a slope from two points.
        let two = size_prior(&target, &[anchor(1, 2, 10, Some(40)), anchor(2, 2, 900, Some(160))]);
        assert!((two - 80.0).abs() < 1.0, "{two}");
        // A perverse anchor set (more lines, less time) cannot produce a
        // negative slope, so the prediction stays inside the observed range.
        let perverse: Vec<AnchorPr> = (1..=8)
            .map(|i| anchor(i, 3, i * 200, Some(600 - i as u32 * 60)))
            .collect();
        let p = size_prior(&target, &perverse);
        assert!((60.0..=1800.0).contains(&p), "{p}");
    }

    #[test]
    fn size_fallback_is_capped_at_low_confidence() {
        let anchors = baseline(40);
        let target = DiffFeatures {
            lines_added: 300,
            lines_deleted: 200,
            ..Default::default()
        };
        let e = estimate_from_size(&anchors, &target);
        assert_eq!(e.confidence, Confidence::Low, "{e:?}");
        assert_eq!(e.anchor_n, 40);
        assert!(e.p10_minutes <= e.p50_minutes && e.p50_minutes <= e.p90_minutes);
        // Wider than the judged path over the same anchors, because it knows less.
        let judged_e = estimate(&anchors, &judged(0.5, 0.3, 0.7), &target);
        let ratio = |e: &EffortEstimate| e.p90_minutes as f64 / e.p10_minutes as f64;
        assert!(ratio(&e) > ratio(&judged_e), "{e:?} vs {judged_e:?}");
        // Thin baseline still overrides.
        assert_eq!(
            estimate_from_size(&baseline(3), &target).confidence,
            Confidence::Insufficient
        );
    }

    #[test]
    fn size_fallback_may_exceed_the_largest_anchor() {
        // baseline(20) tops out at 600 minutes for 1000 lines of churn. A PR
        // eight times larger than anything in the baseline must not be reported
        // as costing exactly what the largest anchor cost — the regression is
        // allowed to extrapolate (bounded at 3× the biggest observed anchor).
        let anchors = baseline(20);
        let huge = DiffFeatures {
            lines_added: 6_000,
            lines_deleted: 2_000,
            ..Default::default()
        };
        let e = estimate_from_size(&anchors, &huge);
        assert!(e.p50_minutes > 600, "{e:?}");
        assert!(e.p50_minutes <= 1_800, "bounded at 3x the largest anchor: {e:?}");
        assert!(e.p10_minutes <= e.p50_minutes && e.p50_minutes <= e.p90_minutes);
        // …and a small PR still lands well below it.
        let small = DiffFeatures {
            lines_added: 20,
            lines_deleted: 5,
            ..Default::default()
        };
        assert!(estimate_from_size(&anchors, &small).p50_minutes < e.p50_minutes);
    }

    #[test]
    fn quantile_interpolates_between_order_statistics() {
        let s = [10.0, 20.0, 45.0, 90.0, 400.0];
        assert_eq!(quantile(&s, 0.0), 10.0);
        assert_eq!(quantile(&s, 1.0), 400.0);
        assert_eq!(quantile(&s, 0.5), 45.0);
        // pos = 0.13 * 4 = 0.52, between s[0] and s[1].
        assert!((quantile(&s, 0.13) - 15.2).abs() < 1e-9);
        // Out-of-range quantiles clamp instead of indexing past the end.
        assert_eq!(quantile(&s, -3.0), 10.0);
        assert_eq!(quantile(&s, 7.0), 400.0);
        assert_eq!(quantile(&[7.0], 0.9), 7.0);
        // Monotone in q, which is what makes p50 monotone in placement.
        let mut prev = f64::MIN;
        for step in 0..=100 {
            let v = quantile(&s, step as f64 / 100.0);
            assert!(v >= prev, "q={step}");
            prev = v;
        }
    }

    #[test]
    fn estimate_is_serializable_and_holds_only_numbers() {
        let e = estimate(&baseline(20), &judged(0.6, 0.5, 0.5), &DiffFeatures::default());
        let json = serde_json::to_value(e).unwrap();
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "p10_minutes",
                "p50_minutes",
                "p90_minutes",
                "anchor_n",
                "confidence"
            ]
        );
        assert_eq!(json["confidence"], serde_json::json!("moderate"));
    }
}
