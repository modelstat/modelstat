//! Tier 2 — **hours, and only against real labels**.
//!
//! [`crate::units`] produces a dimensionless ratio. Turning a ratio into a
//! duration needs a constant of proportionality, and there is exactly one
//! honest place to get it: durations a human on this team wrote down. Not
//! `active_minutes` (Spearman ρ 0.11–0.24 against change size across three real
//! repos — see [`crate::units`]), not `span_ms` (wall clock, which is the thing
//! effort exists to replace), and not a vendor table.
//!
//! So the whole module is one gate:
//!
//! * fewer than [`crate::MIN_LABELS`] labels ⇒ [`calibrate_hours`] returns
//!   `None`, and no hours exist anywhere in the API. It does not degrade to a
//!   guess. A number that looks like an estimate but is a default is worse than
//!   a blank, because a blank cannot be pasted into a slide.
//! * [`crate::MIN_LABELS`] or more ⇒ a [`Calibration`], and every hours figure
//!   is published next to that calibration's own measured error.
//!
//! ## The error is leave-one-out, not in-sample
//!
//! A two-parameter curve fitted to eight points and then scored on those same
//! eight points describes the fitting, not the predicting. So
//! [`Calibration::median_abs_pct_error`] and [`Calibration::spearman_rho`] are
//! computed by refitting `n` times, each time holding one label out and
//! predicting it.
//!
//! Measured on this module's fixtures, LOO runs **1.03–1.15× the in-sample
//! error** (n = 8..40, noise 0–0.7). That gap is real but modest, and the
//! honest reading is that the RATIO is not the interesting number — two
//! parameters simply cannot overfit very hard, and the fixtures' wobble is a
//! smooth function of the index, so part of it is genuinely learnable. The
//! interesting number is the absolute one: label sets whose units carry no
//! information report ~95% LOO error and a NEGATIVE `spearman_rho`, which is
//! the calibration saying, in the only two fields it has, that it cannot rank
//! this repo's PRs. In-sample fitting would have reported the same set at 87%
//! and left the sign of the correlation unexamined.
//!
//! The interval in [`estimate_hours`] comes from that same LOO residual spread,
//! so a calibration that predicts badly reports a wide interval — automatically,
//! and without anyone choosing a ±30%.
//!
//! ## The invariant, in types
//!
//! [`Calibration`] and [`HoursEstimate`] have private fields, no public
//! constructor, and derive `Serialize` but deliberately **not** `Deserialize`.
//! A `Calibration` therefore cannot be produced by a struct literal, by
//! `Default`, or by parsing JSON — only by handing [`calibrate_hours`] enough
//! labels. And [`estimate_hours`] takes one by reference. Together that makes
//! "hours without labels" unrepresentable rather than merely discouraged.

use serde::Serialize;

use crate::labels::MIN_LABELS;
use crate::units::quantile;

/// Clamp on the fitted exponent of `minutes = scale · units^exponent`.
///
/// A negative exponent says bigger PRs are quicker, which is a small-sample
/// artefact and never a finding; past 2.0 the fit has latched onto one large
/// outlier. Clamping is honest here because the clamp shows up in the LOO
/// error — a repo whose labels really do want a wild exponent gets a large
/// reported error rather than a silently wrong curve.
const EXPONENT_MIN: f64 = 0.0;
const EXPONENT_MAX: f64 = 2.0;

/// Bounds on any reported hours figure: a minute, and ten work-weeks.
const MIN_HOURS: f64 = 1.0 / 60.0;
const MAX_HOURS: f64 = 1_600.0;

/// Units below this are treated as this before the power law, so a degenerate
/// `0.0` cannot become `ln(0) = -inf`.
const MIN_UNITS: f64 = 1e-4;

/// Log-space half-width bounds for the p10..p90 interval. The floor stops a
/// suspiciously tidy label set from claiming ±10% predictive accuracy; the
/// ceiling stops a hopeless one from reporting an interval so wide it is
/// indistinguishable from silence (`p90/p10 ≈ 24×` at the top).
const HALF_WIDTH_MIN: f64 = 0.10;
const HALF_WIDTH_MAX: f64 = 1.6;

/// Normal-theory conversion from a median absolute deviation to an 80%
/// interval: `σ = MAD / 0.6745`, and the 10th/90th percentiles sit at
/// `±1.2816 σ`.
const MAD_TO_SIGMA: f64 = 1.0 / 0.6745;
const Z_90: f64 = 1.2816;

/// A fitted units→minutes law, plus how badly it predicts.
///
/// `minutes = scale · units^exponent`, and it exists only because someone
/// labelled at least [`crate::MIN_LABELS`] PRs by hand. Private fields with no
/// public constructor: see the module docs on the invariant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Calibration {
    scale: f64,
    exponent: f64,
    n: usize,
    median_abs_pct_error: f64,
    spearman_rho: f64,
}

impl Calibration {
    /// Minutes at one unit.
    pub fn scale(&self) -> f64 {
        self.scale
    }
    /// The power law's exponent. Typically below 1 — effort is sublinear in
    /// apparent size.
    pub fn exponent(&self) -> f64 {
        self.exponent
    }
    /// Labels the fit used.
    pub fn n(&self) -> usize {
        self.n
    }
    /// Median `|predicted − actual| / actual`, as a percent, **leave-one-out**.
    /// Publish this next to every hours figure; it is the honest half.
    pub fn median_abs_pct_error(&self) -> f64 {
        self.median_abs_pct_error
    }
    /// Rank correlation between the leave-one-out predictions and the labels.
    /// Says whether the curve gets the ORDER right, which often matters more
    /// than whether it gets the magnitude right.
    pub fn spearman_rho(&self) -> f64 {
        self.spearman_rho
    }
}

/// An hours interval. The only way to hold one is to have had a
/// [`Calibration`], because [`estimate_hours`] is the only constructor.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct HoursEstimate {
    p10: f64,
    p50: f64,
    p90: f64,
}

impl HoursEstimate {
    pub fn p10(&self) -> f64 {
        self.p10
    }
    pub fn p50(&self) -> f64 {
        self.p50
    }
    pub fn p90(&self) -> f64 {
        self.p90
    }
}

/// Least squares of `y` on `x`, returning `(intercept, clamped slope)` through
/// the centroid. `None` on fewer than two points.
///
/// A zero-variance `x` (every labelled PR the same size) is not a failure: the
/// slope is unidentifiable, so the fit degenerates to the geometric mean of the
/// labels, which is the correct answer to "these are all the same size".
fn fit(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = pts.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / nf;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / nf;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (x, y) in pts {
        let dx = x - mx;
        sxx += dx * dx;
        sxy += dx * (y - my);
    }
    let slope = if sxx > f64::EPSILON {
        (sxy / sxx).clamp(EXPONENT_MIN, EXPONENT_MAX)
    } else {
        0.0
    };
    let intercept = my - slope * mx;
    (intercept.is_finite() && slope.is_finite()).then_some((intercept, slope))
}

/// Average ranks, ties shared. `1..=n`.
fn ranks(v: &[f64]) -> Vec<f64> {
    let n = v.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| v[a].total_cmp(&v[b]));
    let mut out = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && v[idx[j + 1]] == v[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for &k in &idx[i..=j] {
            out[k] = avg;
        }
        i = j + 1;
    }
    out
}

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 2 || n != y.len() {
        return 0.0;
    }
    let nf = n as f64;
    let mx = x.iter().sum::<f64>() / nf;
    let my = y.iter().sum::<f64>() / nf;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (xi, yi) in x.iter().zip(y) {
        let (dx, dy) = (xi - mx, yi - my);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    let d = (sxx * syy).sqrt();
    if d > 0.0 {
        (sxy / d).clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Spearman rank correlation. `0.0` when either side is constant — no
/// information, which is not the same as disagreement.
fn spearman(a: &[f64], b: &[f64]) -> f64 {
    pearson(&ranks(a), &ranks(b))
}

/// Fit `minutes = scale · units^exponent` on hand-labelled PRs.
///
/// `None` below [`crate::MIN_LABELS`] usable pairs — and *usable* means a
/// positive, finite unit score and a non-zero minute label, because both axes
/// are logged. This is the gate the whole two-tier design rests on: there is no
/// other constructor for [`Calibration`], so no path to hours skips it.
pub fn calibrate_hours(units: &[(f64, u32)]) -> Option<Calibration> {
    // (ln units, ln minutes). Logs on both axes: effort-vs-size is a power law,
    // and a linear fit on heavy-tailed data is dictated by its largest point.
    let pts: Vec<(f64, f64)> = units
        .iter()
        .filter(|(u, m)| u.is_finite() && *u > 0.0 && *m > 0)
        .map(|(u, m)| (u.ln(), f64::from(*m).ln()))
        .collect();
    if pts.len() < MIN_LABELS {
        return None;
    }
    let (intercept, exponent) = fit(&pts)?;

    // Leave-one-out: refit without each point and predict it. This is the only
    // error figure that describes prediction rather than fitting.
    let mut held_out = Vec::with_capacity(pts.len());
    let mut predicted = Vec::with_capacity(pts.len());
    let mut abs_pct = Vec::with_capacity(pts.len());
    let mut rest = Vec::with_capacity(pts.len() - 1);
    for i in 0..pts.len() {
        rest.clear();
        rest.extend(pts.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| *p));
        let (a, b) = fit(&rest)?;
        let ln_pred = a + b * pts[i].0;
        let (pred, actual) = (ln_pred.exp(), pts[i].1.exp());
        if !pred.is_finite() || !actual.is_finite() || actual <= 0.0 {
            return None;
        }
        abs_pct.push((pred - actual).abs() / actual * 100.0);
        predicted.push(ln_pred);
        held_out.push(pts[i].1);
    }
    abs_pct.sort_by(f64::total_cmp);

    let scale = intercept.exp();
    scale.is_finite().then_some(Calibration {
        scale,
        exponent,
        n: pts.len(),
        median_abs_pct_error: quantile(&abs_pct, 0.5),
        // Rank correlation is invariant under the log, so the log-space
        // predictions rank exactly as the minute predictions would.
        spearman_rho: spearman(&predicted, &held_out),
    })
}

/// Half-width, in log space, of the reported interval.
///
/// Read straight off the calibration's own LOO error rather than a chosen
/// multiplier: the median absolute percent error, pulled back into log space,
/// IS the median absolute log residual, and the normal-theory constants above
/// turn a MAD into an 80% interval. A calibration that predicts badly therefore
/// reports a wide interval, and one that predicts well reports a narrow one,
/// with nobody in the loop.
fn half_width(median_abs_pct_error: f64) -> f64 {
    let mad = (1.0 + median_abs_pct_error.max(0.0) / 100.0).ln();
    if !mad.is_finite() {
        return HALF_WIDTH_MAX;
    }
    (mad * MAD_TO_SIGMA * Z_90).clamp(HALF_WIDTH_MIN, HALF_WIDTH_MAX)
}

fn clamp_hours(h: f64) -> f64 {
    if h.is_finite() {
        h.clamp(MIN_HOURS, MAX_HOURS)
    } else {
        MIN_HOURS
    }
}

/// Hours for a PR of `units`, under a calibration someone earned with labels.
///
/// The interval is multiplicative (built in log space), so `0 < p10 ≤ p50 ≤
/// p90` holds however wide it gets — it cannot cross zero, and a duration
/// interval that could would be nonsense.
pub fn estimate_hours(units: f64, cal: &Calibration) -> HoursEstimate {
    let u = if units.is_finite() { units.max(MIN_UNITS) } else { MIN_UNITS };
    let minutes = cal.scale * u.powf(cal.exponent);
    let p50 = clamp_hours(minutes / 60.0);
    let w = half_width(cal.median_abs_pct_error);
    let p10 = clamp_hours(p50 * (-w).exp()).min(p50);
    let p90 = clamp_hours(p50 * w.exp()).max(p50);
    HoursEstimate { p10, p50, p90 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `minutes = 90 · units^0.8`, perturbed by a deterministic pseudo-noise so
    /// the fit has something to be wrong about.
    fn synthetic(n: usize, noise: f64) -> Vec<(f64, u32)> {
        (0..n)
            .map(|i| {
                let units = 0.25 + i as f64 * 0.35;
                // Deterministic, sign-alternating, magnitude-varying. Not a PRNG;
                // just a fixture that is not a straight line.
                let wobble = 1.0 + noise * ((i as f64 * 2.399_963).sin());
                let minutes = (90.0 * units.powf(0.8) * wobble).max(1.0);
                (units, minutes.round() as u32)
            })
            .collect()
    }

    /// In-sample median absolute percent error of a calibration on the very
    /// points it was fitted to — the number LOO exists to replace.
    fn in_sample_error(cal: &Calibration, pts: &[(f64, u32)]) -> f64 {
        let mut e: Vec<f64> = pts
            .iter()
            .map(|(u, m)| {
                let pred = cal.scale * u.powf(cal.exponent);
                let actual = f64::from(*m);
                (pred - actual).abs() / actual * 100.0
            })
            .collect();
        e.sort_by(f64::total_cmp);
        quantile(&e, 0.5)
    }

    #[test]
    fn seven_labels_is_not_a_calibration_and_eight_is() {
        assert!(calibrate_hours(&synthetic(MIN_LABELS - 1, 0.2)).is_none());
        assert!(calibrate_hours(&synthetic(MIN_LABELS, 0.2)).is_some());
    }

    #[test]
    fn unusable_pairs_do_not_count_toward_the_threshold() {
        // Eight rows, but three are junk: zero minutes, zero units, NaN units.
        let mut pts = synthetic(5, 0.1);
        pts.push((1.0, 0));
        pts.push((0.0, 60));
        pts.push((f64::NAN, 60));
        assert_eq!(pts.len(), 8);
        assert!(
            calibrate_hours(&pts).is_none(),
            "five real labels padded to eight is still five labels"
        );
    }

    #[test]
    fn recovers_a_known_power_law() {
        let cal = calibrate_hours(&synthetic(24, 0.0)).expect("clean fit");
        assert_eq!(cal.n(), 24);
        assert!((cal.scale() - 90.0).abs() < 1.0, "scale {}", cal.scale());
        assert!((cal.exponent() - 0.8).abs() < 0.02, "exponent {}", cal.exponent());
        assert!(cal.median_abs_pct_error() < 1.0, "{}", cal.median_abs_pct_error());
        assert!(cal.spearman_rho() > 0.99, "{}", cal.spearman_rho());
    }

    #[test]
    fn loo_error_exceeds_the_in_sample_error_it_replaces() {
        let pts = synthetic(12, 0.55);
        let cal = calibrate_hours(&pts).expect("noisy fit");
        let in_sample = in_sample_error(&cal, &pts);
        assert!(
            cal.median_abs_pct_error() > in_sample,
            "LOO {:.2}% must exceed in-sample {in_sample:.2}% — otherwise the \
             reported error is describing the fit, not the prediction",
            cal.median_abs_pct_error()
        );
    }

    #[test]
    fn labels_that_carry_no_signal_report_themselves_as_useless() {
        // Minutes cycle independently of size: units predict nothing here.
        let pts: Vec<(f64, u32)> = (0..12)
            .map(|i| (0.25 + i as f64 * 0.35, [30u32, 400, 60, 700, 45, 250][i % 6]))
            .collect();
        let cal = calibrate_hours(&pts).expect("a fit still exists, it is just bad");
        assert!(
            cal.median_abs_pct_error() > 60.0,
            "an uninformative label set must LOOK uninformative: {}%",
            cal.median_abs_pct_error()
        );
        assert!(
            cal.spearman_rho() < 0.3,
            "and must not claim it can rank: rho {}",
            cal.spearman_rho()
        );
        // Which shows up where a reader will actually see it: the interval.
        let h = estimate_hours(2.0, &cal);
        assert!(h.p90() / h.p10() > 8.0, "{h:?}");
    }

    #[test]
    fn a_wilder_label_set_reports_a_wider_interval() {
        let tight = calibrate_hours(&synthetic(12, 0.05)).expect("tight");
        let loose = calibrate_hours(&synthetic(12, 0.7)).expect("loose");
        assert!(loose.median_abs_pct_error() > tight.median_abs_pct_error());
        let (t, l) = (estimate_hours(2.0, &tight), estimate_hours(2.0, &loose));
        assert!(
            l.p90() / l.p10() > t.p90() / t.p10(),
            "interval must follow the measured error, not a constant: {t:?} vs {l:?}"
        );
    }

    #[test]
    fn intervals_are_ordered_and_finite_for_any_units() {
        let cal = calibrate_hours(&synthetic(16, 0.3)).expect("fit");
        for units in [0.0, 1e-9, 0.01, 1.0, 7.5, 1e6, f64::INFINITY, f64::NAN] {
            let h = estimate_hours(units, &cal);
            assert!(h.p10() <= h.p50() && h.p50() <= h.p90(), "{units}: {h:?}");
            assert!(h.p10() > 0.0 && h.p90().is_finite(), "{units}: {h:?}");
            assert!(h.p90() <= MAX_HOURS, "{units}: {h:?}");
        }
    }

    #[test]
    fn hours_rise_with_units() {
        let cal = calibrate_hours(&synthetic(16, 0.2)).expect("fit");
        let (a, b) = (estimate_hours(1.0, &cal), estimate_hours(4.0, &cal));
        assert!(b.p50() > a.p50(), "{b:?} vs {a:?}");
    }

    #[test]
    fn identical_sized_labels_degenerate_to_their_geometric_mean() {
        // Unidentifiable slope: every PR the same size, minutes all over.
        let pts: Vec<(f64, u32)> = (0..10).map(|i| (2.0, 30 + i * 10)).collect();
        let cal = calibrate_hours(&pts).expect("degenerate but valid");
        assert_eq!(cal.exponent(), 0.0);
        // Geometric mean of 30..120 ≈ 70.6 minutes ⇒ ~1.18 h, flat in units.
        let h = estimate_hours(2.0, &cal);
        assert!((h.p50() - 70.6 / 60.0).abs() < 0.05, "{h:?}");
        assert_eq!(estimate_hours(9.0, &cal).p50(), h.p50());
    }

    #[test]
    fn the_calibration_serializes_as_five_numbers() {
        let cal = calibrate_hours(&synthetic(10, 0.2)).expect("fit");
        let json = serde_json::to_value(cal).unwrap();
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["scale", "exponent", "n", "median_abs_pct_error", "spearman_rho"]
        );
        let hours = serde_json::to_value(estimate_hours(1.0, &cal)).unwrap();
        let hkeys: Vec<&str> = hours.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(hkeys, vec!["p10", "p50", "p90"]);
    }
}
