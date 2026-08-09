//! `modelstat roi` — what the AI token spend bought, measured on this device.
//!
//! Everything here is local: the repo's own git history, the effort estimator
//! ([`modelstat_effort`]), the session token totals joined to PRs
//! ([`modelstat_effort::attribution`]), and hand-written labels from
//! [`LabelStore`]. No network, no server, no forge token.
//!
//! ## The two refusals this command exists to enforce
//!
//! * **No hours without a [`Calibration`].** Units are dimensionless and
//!   repo-relative; turning them into a duration needs at least
//!   [`MIN_LABELS`] hand-labelled PRs. Below that the hours column is not
//!   rendered at all — not blank, not "≈", not a default — and the run ends
//!   with the one line that says how to unlock it. See [`render_human`].
//! * **No dollars without `--usd-per-mtok`.** The daemon deliberately holds no
//!   price table (pricing is a server concern, and a stale table quietly
//!   invents money). Tokens are counted locally and exactly; the rate is the
//!   user's to supply.
//!
//! Both refusals live in the pure renderer, so they are testable without a
//! repo: [`render_human`] takes a [`RoiView`] and returns a `String`.
//!
//! ## What the token figures are
//!
//! The five buckets a [`TokenMix`] carries are disjoint, and on a real agent
//! session they are wildly lopsided: measured over 1,606 turns on this device,
//! cache reads were 92.3% of raw tokens. They are re-counted on every turn and
//! bill at roughly a tenth of fresh input, so a raw sum overstates
//! billable-equivalent spend by ~5× and — the worse failure — destroys
//! comparability: a PR touched during a long-context session outweighs an
//! identical PR from a short one purely on context length, not on work done.
//!
//! So the headline denominator is INPUT-EQUIVALENT tokens
//! ([`TokenMix::equiv_tokens`], weighted by the provider-family ratios named in
//! [`modelstat_effort::attribution`]) and every raw class stays visible: the
//! rollup prints the mix directly beneath the equivalent, and `--json` carries
//! both plus the weights themselves. A derived number never replaces a measured
//! one here — it sits next to it.
//!
//! `--usd-per-mtok` prices the EQUIVALENT. The weights normalise the classes
//! against each other; the rate is still the user's to supply, because this
//! device holds no price table.
//!
//! The session→PR join is not uniform in strength: a session whose changed
//! files overlap the PR's is a strong match, while one that merely MENTIONED
//! the PR number is a guess — the number only exists after the work is done,
//! so a mention usually marks review or follow-up, not authorship. So strength
//! is reported per row (a `~` on the tokens cell for anything at or below
//! [`WEAK_CONFIDENCE`]) and again in the rollup, as both a mean confidence and
//! the SHARE OF VOLUME resting on weak matches — the mean alone hides one huge
//! guess beside nine certainties.
//! `unattributed` is DEVICE-WIDE — the scan reads every tool log on the
//! machine, not just this repo's.
//!
//! ## Privacy
//!
//! What crosses into output is PR numbers, counts, and derived numbers — the
//! same class as a git remote. No diffs, no paths, no commit messages, no
//! author identity, in either the human or the `--json` form.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use modelstat_effort::attribution::{
    self, PrSpend, TokenMix, W_CACHE_READ, W_CACHE_WRITE, W_INPUT, W_OUTPUT,
};
use modelstat_effort::{calibrate_hours, estimate_pr_effort, Calibration, LabelStore, MIN_LABELS};
use modelstat_ingest::home_path;
use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::git_anchors::select_anchor_commits;
use modelstat_parsers::git_outcome::parse_git_log;
use modelstat_parsers::{mine_repo_anchors, AnchorConfig};
use modelstat_wire::AnchorPr;
use serde_json::{json, Value};

/// Ceiling on the one git call this module owns. Same 4s class as every other
/// git read on the device (`git_anchors::GIT_TIMEOUT`).
const GIT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// First-parent commits walked to enumerate merged PRs. Matches the anchor
/// miner's window so the two agree on which merges exist.
const MAX_HISTORY: &str = "2000";

/// The log format the parsers' [`parse_git_log`] expects: sha, committer date,
/// subject, body, with the US/RS separators that never occur in commit text.
/// (`git_outcome::git_log_format` is private; the format string is not.)
const LOG_FORMAT: &str = "--format=%H\u{1f}%cI\u{1f}%s\u{1f}%b\u{1e}";

/// Where hand-written effort labels live, beside `anchors.json` and
/// `state.json` in the daemon home (honors `MODELSTAT_HOME`).
pub(crate) fn labels_path() -> std::path::PathBuf {
    home_path("effort-labels.json")
}

// ── flags ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct RoiOpts {
    pub repo: String,
    pub days: u32,
    pub limit: usize,
    pub json: bool,
    /// Dollars per million tokens. `None` — the default — means this command
    /// prints no money at all.
    pub usd_per_mtok: Option<f64>,
}

/// `--flag value` / `--flag=value`, the shape `cmd_admin::flag_value` uses.
pub(crate) fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some(v.to_string());
        }
    }
    None
}

/// Parse a flag's value, or say which flag was wrong. A bad number is an error
/// rather than a silent default: `--days=thirty` returning 30 would report a
/// window nobody asked for.
fn parse_flag<T: std::str::FromStr>(
    args: &[String],
    name: &str,
    default: T,
) -> Result<T, String> {
    match flag_value(args, name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("modelstat roi: {name} expects a number, got `{v}`")),
    }
}

pub fn parse_roi_opts(args: &[String]) -> Result<RoiOpts, String> {
    let usd_per_mtok = match flag_value(args, "--usd-per-mtok") {
        None => None,
        Some(v) => {
            let rate: f64 = v
                .parse()
                .map_err(|_| format!("modelstat roi: --usd-per-mtok expects a number, got `{v}`"))?;
            if !rate.is_finite() || rate <= 0.0 {
                return Err("modelstat roi: --usd-per-mtok must be a positive number".into());
            }
            Some(rate)
        }
    };
    Ok(RoiOpts {
        repo: flag_value(args, "--repo").unwrap_or_else(|| ".".to_string()),
        days: parse_flag(args, "--days", 30u32)?,
        limit: parse_flag(args, "--limit", 20usize)?,
        json: args.iter().any(|a| a == "--json"),
        usd_per_mtok,
    })
}

// ── the view the renderers consume (pure, no git, no clock) ─────────────

/// One merged PR's row. Everything already resolved — the renderers do
/// arithmetic and formatting only.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pr_number: u64,
    pub ai_assisted: bool,
    pub merged_at: String,
    pub units: f64,
    pub percentile: f64,
    pub judged: bool,
    /// The raw per-class counts, never collapsed: a reader must be able to see
    /// what the equivalent was derived from.
    pub mix: TokenMix,
    /// `mix` weighted into input-equivalents — the comparable figure, and the
    /// one the table, the ratio and any dollars are built on.
    pub equiv_tokens: f64,
    pub sessions: u32,
    /// How firm this row's token figure is: high when the session's changed
    /// files overlapped the PR's, low when all that linked them was a mention
    /// of the number. [`is_weak`] reads it; the table marks it.
    pub attribution_confidence: f64,
    /// `Some` iff a [`Calibration`] existed. Never synthesised.
    pub hours_p50: Option<f64>,
    pub hours_p10: Option<f64>,
    pub hours_p90: Option<f64>,
}

/// Everything `roi` learned, in the shape both renderers read.
#[derive(Debug, Clone)]
pub struct RoiView {
    pub slug: String,
    pub days: u32,
    pub anchor_n: usize,
    pub rows: Vec<Row>,
    /// Merged PRs the window held BEFORE `--limit` and before any unreadable
    /// diff was dropped. The header says so when it exceeds `rows.len()`, so a
    /// truncated table cannot be read as the whole window.
    pub window_total: usize,
    /// Of those, how many were AI-assisted — a WINDOW count, taken before
    /// `--limit` cut the table. `--limit` chooses which rows are listed; it
    /// must never change what the header says the window held. Human-authored
    /// is `window_total - window_ai`, so the two always sum to the announced
    /// total.
    pub window_ai: usize,
    pub unattributed: TokenMix,
    pub unattributed_sessions: u32,
    pub sessions_scanned: u32,
    pub label_count: usize,
    /// The single source of the hours invariant: `None` here means no hours
    /// figure appears anywhere in either output form.
    pub calibration: Option<Calibration>,
    pub usd_per_mtok: Option<f64>,
    /// Whether the token join found any tool logs at all, so "0 tokens" can be
    /// told apart from "nothing to read".
    pub spend_available: bool,
}

/// The rollup, computed over the AI-assisted rows only.
///
/// AI-only is the ROI question: tokens are the numerator and the work those
/// tokens produced is the denominator. Human rows stay in the table as this
/// repo's baseline, and mixing them into the ratio would dilute exactly the
/// number the command exists to report.
#[derive(Debug, Clone, PartialEq)]
pub struct Totals {
    pub ai_prs: usize,
    pub human_prs: usize,
    pub units: f64,
    /// Raw classes, summed. Kept whole so the rollup can show the reader what
    /// the equivalent came from rather than asking them to trust it.
    pub mix: TokenMix,
    pub equiv_tokens: f64,
    pub sessions: u32,
    /// `None` when there is no effort to divide by — never `0.0`, which would
    /// read as "free".
    pub tokens_per_unit: Option<f64>,
    pub hours: Option<f64>,
    pub hours_per_mtok: Option<f64>,
    pub usd: Option<f64>,
    /// Token-weighted mean of the rows' `attribution_confidence`. `None` when
    /// nothing was attributed. Printed because the join is inferred, not
    /// git-certain — file overlap is strong evidence, a bare PR mention is
    /// weak — so reading these token figures as exact overstates what they are.
    pub confidence: Option<f64>,
    /// Share of the attributed equivalent volume whose join is mention-only
    /// ([`is_weak`]). `None` when nothing was attributed. Reported beside
    /// `confidence` because a mean hides distribution: 0.72 can be nine
    /// certain PRs, or one certain PR beside one large guess.
    pub weak_share: Option<f64>,
}

impl RoiView {
    pub fn totals(&self) -> Totals {
        let ai: Vec<&Row> = self.rows.iter().filter(|r| r.ai_assisted).collect();
        // `impl Sum for f64` folds from `-0.0`, so an empty AI set sums to
        // negative zero and renders as `-0.00 effort units`. Real, and a
        // nonsense thing to show a user.
        let units: f64 = unsign_zero(ai.iter().map(|r| r.units).sum());
        let mix = ai.iter().fold(TokenMix::default(), |mut m, r| {
            m.input = m.input.saturating_add(r.mix.input);
            m.output = m.output.saturating_add(r.mix.output);
            m.cache_creation = m.cache_creation.saturating_add(r.mix.cache_creation);
            m.cache_read = m.cache_read.saturating_add(r.mix.cache_read);
            m.reasoning = m.reasoning.saturating_add(r.mix.reasoning);
            m
        });
        // Summed off the rows, not recomputed from `mix`, so the rollup is
        // exactly the column above it.
        let equiv: f64 = unsign_zero(ai.iter().map(|r| r.equiv_tokens).sum());
        let sessions: u32 = ai.iter().map(|r| r.sessions).sum();
        // Hours exist iff every AI row carried one, which happens iff a
        // Calibration existed — the same gate, re-read off the data.
        let hours = self
            .calibration
            .is_some()
            .then(|| unsign_zero(ai.iter().filter_map(|r| r.hours_p50).sum()));
        Totals {
            ai_prs: ai.len(),
            human_prs: self.rows.len() - ai.len(),
            units,
            mix,
            equiv_tokens: equiv,
            sessions,
            tokens_per_unit: tokens_per_unit(equiv, units),
            hours,
            hours_per_mtok: hours.and_then(|h| per_mtok(h, equiv)),
            usd: self.usd_per_mtok.map(|rate| usd_for_tokens(equiv, rate)),
            confidence: weighted_confidence(&ai),
            weak_share: weak_volume_share(&ai),
        }
    }
}

// ── pure arithmetic ─────────────────────────────────────────────────────

/// Tokens spent per unit of effort delivered. `None` when the denominator is
/// zero or unusable — dividing by no effort yields infinity, and printing that
/// as an efficiency figure would be worse than printing nothing.
pub fn tokens_per_unit(equiv_tokens: f64, units: f64) -> Option<f64> {
    (units.is_finite() && units > 0.0).then(|| equiv_tokens / units)
}

/// `-0.0` → `0.0`, everything else untouched. `-0.0 == 0.0` is true, so this
/// is a display fix, not an arithmetic one.
pub fn unsign_zero(x: f64) -> f64 {
    if x == 0.0 {
        0.0
    } else {
        x
    }
}

/// `value` per million input-equivalent tokens. `None` when none were
/// attributed.
pub fn per_mtok(value: f64, equiv_tokens: f64) -> Option<f64> {
    (equiv_tokens > 0.0).then(|| value / (equiv_tokens / 1e6))
}

/// Dollars for `equiv_tokens` at `usd_per_mtok`. The only place money is ever
/// derived, and it is only ever called with a rate the user typed.
///
/// The EQUIVALENT, not the raw total: a rate quoted per million input tokens
/// applied to a raw sum that is 92% cache reads invents roughly 5× the spend.
pub fn usd_for_tokens(equiv_tokens: f64, usd_per_mtok: f64) -> f64 {
    equiv_tokens / 1e6 * usd_per_mtok
}

/// Token-weighted mean attribution confidence. Weighted by equivalent tokens,
/// not by row, because one large PR joined on a pasted URL says more about how
/// much of the total is guesswork than nine small PRs joined on a branch name.
pub fn weighted_confidence(rows: &[&Row]) -> Option<f64> {
    let equiv: f64 = rows.iter().map(|r| r.equiv_tokens).sum();
    (equiv > 0.0).then(|| {
        rows.iter()
            .map(|r| r.attribution_confidence * r.equiv_tokens)
            .sum::<f64>()
            / equiv
    })
}

/// The line between a token figure a reader can lean on and one they cannot.
///
/// [`attribution::PrSpend::attribution_confidence`] is a STRENGTH SCALE, not a
/// category: file overlap plus a PR mention scores 1.0, strong overlap alone
/// 0.9, a weak overlap band runs from just under 0.4 to 0.6, and a bare
/// mention with no overlap at all is a flat 0.3 — the "discussed it, did not
/// author it" case, which is the whole reason this join stopped trusting
/// references. Below 0.3 means the mean itself is dominated by mentions. So at
/// or below 0.3 the figure is weak evidence of who spent those tokens; above
/// it, changed files back the claim.
pub const WEAK_CONFIDENCE: f64 = 0.3;

/// Whether a row's attributed tokens rest on a weak match. A row with nothing
/// attributed is never weak — there is no figure to qualify, and the em-dash
/// already says so.
pub fn is_weak(equiv_tokens: f64, confidence: f64) -> bool {
    equiv_tokens > 0.0 && confidence <= WEAK_CONFIDENCE
}

/// Share of the attributed equivalent volume that came from weak matches.
/// `None` when nothing was attributed.
///
/// By VOLUME, not by row: the question a reader is asking is how much of the
/// spend figure above is inferred, and one 11M-token guess among nine small
/// certainties dominates that answer while being one row in ten.
pub fn weak_volume_share(rows: &[&Row]) -> Option<f64> {
    let equiv: f64 = rows.iter().map(|r| r.equiv_tokens).sum();
    (equiv > 0.0).then(|| {
        rows.iter()
            .filter(|r| is_weak(r.equiv_tokens, r.attribution_confidence))
            .map(|r| r.equiv_tokens)
            .sum::<f64>()
            / equiv
    })
}

/// The window's AI-assisted count, and the prefix `--limit` lists.
///
/// The count is taken over the WHOLE window, before the cut: `--limit` chooses
/// how many rows are printed and nothing else. Counting after it made the
/// header describe the table while the total beside it described the window —
/// two different populations, one sentence.
pub fn window_split<T>(window: &[T], limit: usize, is_ai: impl Fn(&T) -> bool) -> (usize, &[T]) {
    (
        window.iter().filter(|p| is_ai(p)).count(),
        &window[..limit.min(window.len())],
    )
}

/// Labels still needed before [`calibrate_hours`] will return anything.
pub fn labels_needed(label_count: usize) -> usize {
    MIN_LABELS.saturating_sub(label_count)
}

/// The one actionable line printed when hours are locked, or `None` once they
/// are not. `example_pr` makes the suggested command copy-pasteable.
pub fn labels_hint(label_count: usize, example_pr: Option<u64>) -> Option<String> {
    let needed = labels_needed(label_count);
    if needed == 0 {
        return None;
    }
    let pr = match example_pr {
        Some(n) => n.to_string(),
        None => "<pr>".to_string(),
    };
    Some(format!(
        "hours: locked — {needed} more label{} needed ({label_count}/{MIN_LABELS}). \
         Add one: modelstat label {pr} <minutes>",
        if needed == 1 { "" } else { "s" }
    ))
}

// ── formatting ──────────────────────────────────────────────────────────

/// `1.2M` / `845k` / `312`. Compact because the table has one row per PR and
/// raw token counts are nine digits wide.
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0 => "—".to_string(),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.0}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// Like [`fmt_tokens`], but `0` renders as `0`. The raw-mix line reads as a
/// sum, and an em-dash inside a sum is not a number.
fn fmt_count(n: u64) -> String {
    if n == 0 {
        "0".to_string()
    } else {
        fmt_tokens(n)
    }
}

/// An input-equivalent figure, tagged so it can never be misread as a raw
/// count: `806k eq`. Zero stays an em-dash — nothing was attributed.
pub fn fmt_equiv(equiv: f64) -> String {
    match equiv.round() as u64 {
        0 => "—".to_string(),
        n => format!("{} eq", fmt_tokens(n)),
    }
}

/// The raw classes as one auditable line. Reasoning folds into `out` — it
/// carries the same weight as output — so the four figures shown sum to the
/// raw total printed beside them.
fn fmt_raw_mix(m: &TokenMix) -> String {
    format!(
        "{} fresh / {} cache-write / {} cache-read / {} out  (raw total {})",
        fmt_count(m.input),
        fmt_count(m.cache_creation),
        fmt_count(m.cache_read),
        fmt_count(m.output.saturating_add(m.reasoning)),
        fmt_count(m.raw_total()),
    )
}

/// Width of the rollup's label column. One width for every line, so adding a
/// label cannot silently misalign the block.
const LABEL_W: usize = 23;

/// One `  label   value` rollup line.
fn kv(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("  {:<w$}{}\n", label, value, w = LABEL_W));
}

fn fmt_usd(v: f64) -> String {
    if v >= 100.0 {
        format!("${v:.0}")
    } else {
        format!("${v:.2}")
    }
}

fn fmt_hours(h: f64) -> String {
    if h >= 10.0 {
        format!("{h:.0}h")
    } else {
        format!("{h:.1}h")
    }
}

// ── the human form ──────────────────────────────────────────────────────

/// Render the whole human output. Pure: same view in, same string out.
///
/// The invariants are structural here, not conditional at each call site — the
/// hours column exists only inside `if view.calibration.is_some()`, and the usd
/// column only inside `if let Some(rate) = view.usd_per_mtok`. A row cannot
/// print a number the header did not announce.
pub fn render_human(view: &RoiView) -> String {
    let t = view.totals();
    let mut out = String::new();
    let hours_on = view.calibration.is_some();
    let usd_rate = view.usd_per_mtok;

    // Header: the split, how thick the baseline behind `units` is, and whether
    // the table is the whole window or a slice of it.
    //
    // The AI/human counts are the WINDOW's, never the table's. Reading them off
    // `totals()` counted only the rows `--limit` let through, so `--limit 6`
    // announced "3 AI-assisted, 3 human-authored" of a 23-PR window: three
    // numbers in one sentence, two describing a different population from the
    // third.
    let shown = if view.window_total > view.rows.len() {
        format!(", showing {} of {}", view.rows.len(), view.window_total)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "{} — {} AI-assisted PRs, {} human-authored, {}d{shown}  \
         (baseline: {} human anchors)\n",
        view.slug,
        view.window_ai,
        view.window_total.saturating_sub(view.window_ai),
        view.days,
        view.anchor_n
    ));

    if view.rows.is_empty() {
        out.push_str("\n  (no merged PRs in this window)\n");
    } else {
        // ONE token column, and it is the equivalent. Four class columns would
        // turn a per-PR table into a spreadsheet; the classes live one line
        // below the rollup's headline, where the sum is what a reader audits.
        // The unnamed one-char column after `tokens` is the match-strength
        // mark — a header for it would be wider than the mark.
        let mut head = format!(
            "\n  {:>6}  {:<5} {:>7} {:>5} {:>9} ",
            "PR", "who", "units", "pct", "tokens"
        );
        if hours_on {
            head.push_str(&format!(" {:>7}", "hours"));
        }
        if usd_rate.is_some() {
            head.push_str(&format!(" {:>9}", "usd"));
        }
        out.push_str(head.trim_end());
        out.push('\n');

        for r in &view.rows {
            let mut line = format!(
                "  {:>6}  {:<5} {:>7.2} {:>4.0}% {:>9}{}",
                format!("#{}", r.pr_number),
                if r.ai_assisted { "AI" } else { "human" },
                r.units,
                r.percentile * 100.0,
                fmt_equiv(r.equiv_tokens),
                // Abuts the number so the mark reads as belonging to it, and
                // the digits stay column-aligned whether marked or not.
                if is_weak(r.equiv_tokens, r.attribution_confidence) {
                    '~'
                } else {
                    ' '
                },
            );
            if hours_on {
                // `hours_p50` is Some for every row whenever a Calibration
                // exists, so this never renders a hole under a live column.
                line.push_str(&format!(
                    " {:>7}",
                    r.hours_p50.map(fmt_hours).unwrap_or_else(|| "—".into())
                ));
            }
            if let Some(rate) = usd_rate {
                // A `$0.00` under an em-dash token cell reads as "this PR was
                // free"; it means nothing was attributed to it. Say that once.
                line.push_str(&format!(
                    " {:>9}",
                    if r.equiv_tokens > 0.0 {
                        fmt_usd(usd_for_tokens(r.equiv_tokens, rate))
                    } else {
                        "—".to_string()
                    }
                ));
            }
            // The mark column leaves a trailing space on unmarked rows when no
            // hours/usd column follows it.
            out.push_str(line.trim_end());
            out.push('\n');
        }
        // One legend line, and only where there is a token figure to qualify:
        // with nothing attributed anywhere, the mark never appears and
        // explaining it is noise.
        if view.rows.iter().any(|r| r.equiv_tokens > 0.0) {
            out.push_str(&format!(
                "  ~ = weak attribution (confidence ≤ {WEAK_CONFIDENCE:.2}): mention-only, with \
                 little or no changed-file overlap behind it\n"
            ));
        }
    }

    // Rollup.
    out.push('\n');
    kv(
        &mut out,
        "AI PRs:",
        &format!("{}  ({:.2} effort units)", t.ai_prs, t.units),
    );
    kv(
        &mut out,
        "tokens (input-equiv):",
        &format!(
            "{} across {} session{}",
            fmt_equiv(t.equiv_tokens),
            t.sessions,
            if t.sessions == 1 { "" } else { "s" }
        ),
    );
    // Directly beneath the headline, so the derived figure and the measured
    // ones it came from are never more than one line apart.
    kv(&mut out, "raw mix:", &fmt_raw_mix(&t.mix));
    // Two different blanks, and the difference matters: no effort to divide by
    // versus no tokens to divide.
    match t.tokens_per_unit {
        _ if t.equiv_tokens <= 0.0 => kv(
            &mut out,
            "tokens per unit:",
            "— (no tokens attributed to these PRs)",
        ),
        Some(tpu) => kv(&mut out, "tokens per unit:", &fmt_equiv(tpu)),
        None => kv(
            &mut out,
            "tokens per unit:",
            "— (no AI effort in this window)",
        ),
    }
    // Money exists only inside this binding — there is no other path to a `$`.
    if let (Some(rate), Some(usd)) = (usd_rate, t.usd) {
        if t.equiv_tokens <= 0.0 {
            // `$0.00` against no attributed tokens reads as "the AI work was
            // free". It was not measured, which is a different claim.
            kv(&mut out, "spend:", "— (no tokens attributed to these PRs)");
        } else {
            kv(&mut out, "spend:", &fmt_usd(usd));
            if let Some(per_unit) = t.tokens_per_unit {
                kv(
                    &mut out,
                    "cost per unit:",
                    &fmt_usd(usd_for_tokens(per_unit, rate)),
                );
            }
        }
    }
    // The join is inferred, so say how firm it is — and how much of the volume
    // rests on the weak kind — before anyone quotes the number above.
    if let Some(c) = t.confidence {
        let weak = match t.weak_share {
            Some(w) => format!(
                " — {:.0}% of volume from weak (mention-only) matches",
                w * 100.0
            ),
            None => String::new(),
        };
        kv(&mut out, "attribution:", &format!("{c:.2} mean confidence{weak}"));
        // Past half, the headline spend figure is mostly a guess about which
        // sessions produced these PRs, and a reader who quotes it should know
        // that before they do.
        if t.weak_share.is_some_and(|w| w > 0.5) {
            kv(
                &mut out,
                "caution:",
                "most of this spend is inferred from PR mentions, not file overlap \
                 — treat the per-PR token figures as indicative",
            );
        }
    }
    // Device-wide, NOT this repo: the session scan reads every tool log on the
    // machine. Labelling it as repo-scoped would understate the denominator by
    // however many repos the person also works in.
    if view.spend_available {
        kv(
            &mut out,
            "unattributed:",
            &format!(
                "{} across {} of {} sessions (device-wide), raw {}",
                fmt_equiv(view.unattributed.equiv_tokens()),
                view.unattributed_sessions,
                view.sessions_scanned,
                fmt_count(view.unattributed.raw_total()),
            ),
        );
    } else {
        kv(
            &mut out,
            "unattributed:",
            "— (no tool session logs found on this device)",
        );
    }

    // Tier 2, and only Tier 2.
    match (&view.calibration, t.hours) {
        (Some(cal), Some(hours)) => {
            kv(
                &mut out,
                "hours:",
                &format!(
                    "{}  ± {:.0}% (LOOCV, n={})",
                    fmt_hours(hours),
                    cal.median_abs_pct_error(),
                    cal.n()
                ),
            );
            match t.hours_per_mtok {
                Some(hpm) => kv(&mut out, "hours per 1M eq:", &format!("{hpm:.2}")),
                None => kv(&mut out, "hours per 1M eq:", "— (no tokens attributed)"),
            }
        }
        _ => {
            let example = view.rows.iter().find(|r| r.ai_assisted).map(|r| r.pr_number);
            if let Some(hint) = labels_hint(view.label_count, example) {
                out.push_str(&format!("  {hint}\n"));
            }
        }
    }

    // Why the two token figures differ, stated once, from the constants
    // themselves so the prose cannot drift from the arithmetic.
    out.push_str(&format!(
        "  note: cache reads bill at roughly a tenth of fresh input \
         (cache-write {W_CACHE_WRITE}×, cache-read {W_CACHE_READ}×, output {W_OUTPUT}×); \
         weighting them is what makes PRs comparable across session lengths.\n"
    ));
    out
}

// ── the machine form ────────────────────────────────────────────────────

/// The `--json` document. `hours`/`usd` keys are always PRESENT and `null` when
/// unavailable, so a consumer must see the absence rather than infer it from a
/// missing key (which reads as an older schema).
pub fn render_json(view: &RoiView) -> Value {
    let t = view.totals();
    let prs: Vec<Value> = view
        .rows
        .iter()
        .map(|r| {
            json!({
                "pr_number": r.pr_number,
                "ai_assisted": r.ai_assisted,
                "merged_at": r.merged_at,
                "units": r.units,
                "percentile_vs_human_anchors": r.percentile,
                "judged": r.judged,
                // Measured classes and the derived figure, always together: a
                // consumer can re-weight the mix itself, and can see that the
                // equivalent is a normalisation rather than a new measurement.
                "mix": r.mix,
                "raw_total": r.mix.raw_total(),
                "equiv_tokens": r.equiv_tokens,
                "session_count": r.sessions,
                "attribution_confidence": r.attribution_confidence,
                "hours": r.hours_p50.map(|p50| json!({
                    "p10": r.hours_p10,
                    "p50": p50,
                    "p90": r.hours_p90,
                })),
                "usd": view.usd_per_mtok.map(|rate| usd_for_tokens(r.equiv_tokens, rate)),
            })
        })
        .collect();

    json!({
        "repo": view.slug,
        "days": view.days,
        "anchor_n": view.anchor_n,
        // The WINDOW, before `--limit`: `prs` below is a slice, and a consumer
        // that counted the split off `totals` would inherit exactly the bug the
        // header had.
        "window": {
            "merged_prs": view.window_total,
            "ai_prs": view.window_ai,
            "human_prs": view.window_total.saturating_sub(view.window_ai),
            "shown": view.rows.len(),
        },
        "spend_available": view.spend_available,
        "usd_per_mtok": view.usd_per_mtok,
        // Published so a consumer can reproduce the human table's `~` mark from
        // `attribution_confidence` instead of guessing where weak begins.
        "weak_confidence_threshold": WEAK_CONFIDENCE,
        // Published, not implied: these are Anthropic-family list RATIOS, not
        // prices, and a consumer on another provider needs to see them to know
        // whether the equivalent means anything for their fleet.
        "equiv_weights": {
            "input": W_INPUT,
            "cache_creation": W_CACHE_WRITE,
            "cache_read": W_CACHE_READ,
            "output": W_OUTPUT,
            "reasoning": W_OUTPUT,
        },
        "labels": {
            "count": view.label_count,
            "needed_for_hours": labels_needed(view.label_count),
            "min_labels": MIN_LABELS,
        },
        "calibration": view.calibration,
        "prs": prs,
        "totals": {
            "ai_prs": t.ai_prs,
            "human_prs": t.human_prs,
            "effort_units": t.units,
            "mix": t.mix,
            "raw_total": t.mix.raw_total(),
            "equiv_tokens": t.equiv_tokens,
            "session_count": t.sessions,
            "tokens_per_effort_unit": t.tokens_per_unit,
            "attribution_confidence": t.confidence,
            // What share of `equiv_tokens` above rests on a mention-only match.
            // The mean alone cannot say: it averages the guess away.
            "attribution_weak_volume_share": t.weak_share,
            // Device-wide, not repo-scoped: the session scan reads every tool
            // log on this machine, across every repo.
            "unattributed_device": {
                "mix": view.unattributed,
                "raw_total": view.unattributed.raw_total(),
                "equiv_tokens": view.unattributed.equiv_tokens(),
            },
            "unattributed_sessions_device": view.unattributed_sessions,
            "sessions_scanned_device": view.sessions_scanned,
            "hours": t.hours,
            "hours_per_mtok": t.hours_per_mtok,
            "usd": t.usd,
        },
    })
}

// ── the impure half: git, the estimator, the token join ─────────────────

/// Run one bounded git read. Best-effort: a spawn failure, a non-zero exit, or
/// a timeout is `None`, never a panic and never a block.
fn git(args: &[&str], cwd: &str, timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout on a helper thread: `git log -n 2000` overflows the pipe
    // buffer, and a child blocked on a full pipe never exits, so polling
    // `try_wait` alone would hang until the deadline on every large repo.
    let mut pipe = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = pipe.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(timeout) {
        Ok(text) => match child.wait() {
            Ok(status) if status.success() => Some(text),
            _ => None,
        },
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// One merged PR as git names it, before any estimation.
pub(crate) struct MergedPr {
    pub pr_number: u64,
    pub merge_sha: String,
    pub merged_at: String,
}

/// Every merged PR the first-parent walk reaches, newest first, one row per PR
/// number. Empty when the repo cannot be read.
///
/// Reuses the parsers' own pure readers ([`parse_git_log`],
/// [`select_anchor_commits`]) so this command and the anchor miner agree on
/// which commits are merges and which PR each names.
pub(crate) fn merged_prs(repo: &str) -> Vec<MergedPr> {
    let Some(log) = git(
        &["log", "--first-parent", "-n", MAX_HISTORY, LOG_FORMAT],
        repo,
        GIT_TIMEOUT,
    ) else {
        return Vec::new();
    };
    let commits = parse_git_log(&log);
    select_anchor_commits(&commits, &AnchorConfig::default())
        .into_iter()
        .map(|(pr_number, c)| MergedPr {
            pr_number,
            merge_sha: c.sha.clone(),
            merged_at: c.committed_at.clone(),
        })
        .collect()
}

/// `merged_at` within the last `days`. A timestamp git could not state is out:
/// a PR we cannot place in time cannot be shown to be in the window.
fn within_days(merged_at: &str, days: u32, now: chrono::DateTime<chrono::Utc>) -> bool {
    match chrono::DateTime::parse_from_rfc3339(merged_at.trim()) {
        Ok(ts) => (now - ts.with_timezone(&chrono::Utc)).num_days() <= i64::from(days),
        Err(_) => false,
    }
}

/// Fit this repo's units→minutes law from its hand-written labels, or `None`.
///
/// `None` for every reason the fit cannot be earned: fewer than [`MIN_LABELS`]
/// labels, labels naming PRs no longer in this history, or a PR whose diff will
/// not read. There is no fallback — that absence is the product.
pub(crate) fn build_calibration(
    repo: &str,
    slug: &str,
    anchors: &[AnchorPr],
    store: &LabelStore,
    shas: &BTreeMap<u64, String>,
) -> Option<Calibration> {
    let pairs: Vec<(f64, u32)> = store
        .labels_for_repo(slug)
        .filter_map(|(pr, label)| {
            let sha = shas.get(&pr)?;
            let report = estimate_pr_effort(repo, sha, anchors, None, None)?;
            Some((report.units.units, label.minutes))
        })
        .collect();
    calibrate_hours(&pairs)
}

pub fn cmd_roi(args: &[String]) -> ExitCode {
    let opts = match parse_roi_opts(args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    let Some(repo) = resolve_repo_root(Some(&opts.repo)) else {
        eprintln!("modelstat roi: `{}` is not inside a git repository", opts.repo);
        return ExitCode::FAILURE;
    };

    // The human baseline `units` is normalised against, plus the AI/human
    // classification. `None` means no remote slug (nothing to key spend on) or
    // git failed outright.
    let Some(mined) = mine_repo_anchors(&repo, &AnchorConfig::default()) else {
        eprintln!(
            "modelstat roi: could not mine {repo} — no `origin` remote, or git could not be read"
        );
        return ExitCode::FAILURE;
    };
    // The miner classifies candidates newest-first and keeps the human ones, so
    // for the newest merges — the only ones this window shows — "not an anchor"
    // means it saw an AI trailer.
    let human: HashSet<u64> = mined.anchors.iter().map(|a| a.pr_number).collect();

    let all_prs = merged_prs(&repo);
    let shas: BTreeMap<u64, String> = all_prs
        .iter()
        .map(|p| (p.pr_number, p.merge_sha.clone()))
        .collect();

    let store = LabelStore::load(&labels_path());
    let label_count = store.labels_for_repo(&mined.slug).count();
    let calibration = build_calibration(&repo, &mined.slug, &mined.anchors, &store, &shas);

    // Tokens, joined to PRs by the sibling module. Best-effort by contract: an
    // empty summary is what "no tool logs on this device" looks like.
    let spend = attribution::spend_by_pr(opts.days);
    let by_pr: BTreeMap<u64, &PrSpend> = spend
        .by_pr
        .iter()
        // `PrSpend.slug` is "as first seen" and matched case-insensitively
        // upstream — a forge that echoes `Org/Repo` must still join.
        .filter(|s| s.slug.eq_ignore_ascii_case(&mined.slug))
        .map(|s| (s.pr_number, s))
        .collect();

    let now = chrono::Utc::now();
    let in_window: Vec<&MergedPr> = all_prs
        .iter()
        .filter(|p| within_days(&p.merged_at, opts.days, now))
        .collect();
    let window_total = in_window.len();
    // The split is counted over the whole window; `--limit` only decides how
    // much of it is printed.
    let (window_ai, shown) = window_split(&in_window, opts.limit, |p| !human.contains(&p.pr_number));
    let rows: Vec<Row> = shown
        .iter()
        .filter_map(|p| {
            // A PR whose diff will not read is dropped, not zeroed: a row of
            // zeros would enter the rollup as free work.
            let report = estimate_pr_effort(
                &repo,
                &p.merge_sha,
                &mined.anchors,
                None,
                calibration.as_ref(),
            )?;
            let spend = by_pr.get(&p.pr_number);
            Some(Row {
                pr_number: p.pr_number,
                ai_assisted: !human.contains(&p.pr_number),
                merged_at: p.merged_at.clone(),
                units: report.units.units,
                percentile: report.units.percentile_vs_human_anchors,
                judged: report.units.judged,
                mix: spend.map_or_else(TokenMix::default, |s| s.mix),
                equiv_tokens: spend.map_or(0.0, |s| s.equiv_tokens),
                sessions: spend.map_or(0, |s| s.session_count),
                attribution_confidence: spend.map_or(0.0, |s| s.attribution_confidence),
                hours_p50: report.hours.map(|h| h.p50()),
                hours_p10: report.hours.map(|h| h.p10()),
                hours_p90: report.hours.map(|h| h.p90()),
            })
        })
        .collect();

    let view = RoiView {
        slug: mined.slug.clone(),
        days: opts.days,
        anchor_n: mined.anchors.len(),
        rows,
        window_total,
        window_ai,
        unattributed: spend.unattributed,
        unattributed_sessions: spend.unattributed_sessions,
        sessions_scanned: spend.sessions_scanned,
        label_count,
        calibration,
        usd_per_mtok: opts.usd_per_mtok,
        spend_available: spend.sessions_scanned > 0,
    };

    if opts.json {
        println!("{}", serde_json::to_string(&render_json(&view)).unwrap());
    } else {
        print!("{}", render_human(&view));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic mix at `raw` total tokens, in the class shares this device
    /// actually measured over 1,606 turns: 0.8% fresh, 0.4% output, 6.5%
    /// cache-write, 92.3% cache-read. `raw` is taken in whole millions so the
    /// shares stay integral and every expectation below can be an exact literal.
    fn mix_of(raw: u64) -> TokenMix {
        let m = raw / 1_000_000;
        TokenMix {
            input: 8_000 * m,
            output: 4_000 * m,
            cache_creation: 65_000 * m,
            cache_read: 923_000 * m,
            reasoning: 0,
        }
    }

    /// Input-equivalents for one raw million of [`mix_of`]'s shape:
    /// `8_000·1 + 4_000·5 + 65_000·1.25 + 923_000·0.1`. The ~5× gap between
    /// this and the raw million IS the defect this command used to report.
    const EQUIV_PER_RAW_M: f64 = 201_550.0;

    /// `0.1` is not exact in binary, so every equivalent carries a little float
    /// dust. Compare well inside one token.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn row(pr: u64, ai: bool, units: f64, tokens: u64) -> Row {
        let mix = mix_of(tokens);
        Row {
            pr_number: pr,
            ai_assisted: ai,
            merged_at: "2026-08-01T00:00:00Z".into(),
            units,
            percentile: 0.5,
            judged: false,
            equiv_tokens: mix.equiv_tokens(),
            mix,
            sessions: 2,
            attribution_confidence: 0.9,
            hours_p50: None,
            hours_p10: None,
            hours_p90: None,
        }
    }

    fn view(rows: Vec<Row>) -> RoiView {
        RoiView {
            slug: "org/repo".into(),
            days: 30,
            anchor_n: 50,
            window_total: rows.len(),
            window_ai: rows.iter().filter(|r| r.ai_assisted).count(),
            rows,
            unattributed: mix_of(2_000_000),
            unattributed_sessions: 4,
            sessions_scanned: 40,
            label_count: 3,
            calibration: None,
            usd_per_mtok: None,
            spend_available: true,
        }
    }

    /// The one line of `text` containing `needle`, or a failure naming what was
    /// missing — an assertion against a line that does not exist is a silent
    /// pass, and every rollup check below is exactly that shape.
    fn line<'a>(text: &'a str, needle: &str) -> &'a str {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line containing `{needle}`:\n{text}"))
    }

    /// A calibration can only be earned, never constructed — so the tests earn
    /// one, from synthetic labels that follow a clean power law.
    fn a_calibration() -> Calibration {
        let pairs: Vec<(f64, u32)> = (1..=12)
            .map(|i| {
                let units = i as f64 * 0.5;
                (units, (60.0 * units.powf(0.8)).round() as u32)
            })
            .collect();
        calibrate_hours(&pairs).expect("12 clean labels calibrate")
    }

    fn with_hours(mut v: RoiView) -> RoiView {
        let cal = a_calibration();
        for r in &mut v.rows {
            let h = modelstat_effort::estimate_hours(r.units, &cal);
            r.hours_p10 = Some(h.p10());
            r.hours_p50 = Some(h.p50());
            r.hours_p90 = Some(h.p90());
        }
        v.label_count = cal.n();
        v.calibration = Some(cal);
        v
    }

    // ── the arithmetic ──────────────────────────────────────────────

    #[test]
    fn tokens_per_unit_divides_and_refuses_zero_effort() {
        assert_eq!(tokens_per_unit(3_000_000.0, 1.5), Some(2_000_000.0));
        assert_eq!(tokens_per_unit(3_000_000.0, 0.0), None);
        assert_eq!(tokens_per_unit(0.0, 2.0), Some(0.0));
        assert_eq!(tokens_per_unit(10.0, f64::NAN), None);
    }

    #[test]
    fn per_mtok_scales_by_millions_and_refuses_zero_tokens() {
        assert_eq!(per_mtok(4.0, 2_000_000.0), Some(2.0));
        assert_eq!(per_mtok(4.0, 0.0), None);
    }

    #[test]
    fn usd_prices_the_equivalent_not_the_raw_total() {
        assert!((usd_for_tokens(2_500_000.0, 3.0) - 7.5).abs() < 1e-9);
        assert_eq!(usd_for_tokens(0.0, 3.0), 0.0);
        // 4M raw of the measured shape is 806_200 equivalent, so $3/Mtok buys
        // $2.42 of spend — not the $12.00 the raw sum used to claim.
        let raw = mix_of(4_000_000);
        assert!(close(usd_for_tokens(raw.equiv_tokens(), 3.0), 2.4186));
        assert!(close(usd_for_tokens(raw.raw_total() as f64, 3.0), 12.0));
    }

    #[test]
    fn totals_cover_ai_rows_only() {
        let v = view(vec![
            row(1, true, 2.0, 1_000_000),
            row(2, true, 2.0, 3_000_000),
            row(3, false, 10.0, 9_000_000),
        ]);
        let t = v.totals();
        assert_eq!(t.ai_prs, 2);
        assert_eq!(t.human_prs, 1);
        assert_eq!(t.units, 4.0);
        // Every raw class survives the rollup, undiminished...
        assert_eq!(t.mix.raw_total(), 4_000_000);
        assert_eq!(t.mix.cache_read, 3_692_000);
        assert_eq!(t.mix.input, 32_000);
        // ...beside the equivalent, which is ~5× smaller and is what divides.
        assert!(close(t.equiv_tokens, 4.0 * EQUIV_PER_RAW_M), "{t:?}");
        assert!(close(t.tokens_per_unit.unwrap(), EQUIV_PER_RAW_M), "{t:?}");
        // No rate supplied ⇒ no money anywhere in the rollup.
        assert_eq!(t.usd, None);
    }

    #[test]
    fn totals_price_only_the_ai_tokens_when_a_rate_is_given() {
        let mut v = view(vec![
            row(1, true, 1.0, 2_000_000),
            row(2, false, 1.0, 8_000_000),
        ]);
        v.usd_per_mtok = Some(5.0);
        // 2M raw ⇒ 403_100 equivalent ⇒ $2.02. Pre-fix this read $10.00,
        // pricing 1.8M re-counted cache reads as if they were fresh input.
        let t = v.totals();
        assert!(close(t.usd.unwrap(), 2.0155), "{t:?}");
    }

    #[test]
    fn confidence_is_weighted_by_tokens_not_by_row() {
        let mut big = row(1, true, 1.0, 9_000_000);
        big.attribution_confidence = 0.3;
        let mut small = row(2, true, 1.0, 1_000_000);
        small.attribution_confidence = 0.8;
        // Row-mean would be 0.55; the 9M-token row is what the total rests on.
        let t = view(vec![big, small]).totals();
        assert!((t.confidence.unwrap() - 0.35).abs() < 1e-9, "{t:?}");

        // Nothing attributed ⇒ nothing to be confident about.
        assert_eq!(view(vec![row(1, true, 1.0, 0)]).totals().confidence, None);
        assert_eq!(weighted_confidence(&[]), None);
    }

    #[test]
    fn the_rollup_publishes_confidence_and_scopes_unattributed_to_the_device() {
        let text = render_human(&view(vec![row(1, true, 2.0, 4_000_000)]));
        assert!(text.contains("0.90 mean confidence"), "{text}");
        assert!(text.contains("(device-wide)"), "{text}");
    }

    #[test]
    fn a_truncated_table_says_so_and_a_whole_one_stays_quiet() {
        let mut v = view(vec![row(1, true, 2.0, 4_000_000)]);
        assert!(!render_human(&v).contains("showing"), "untruncated");
        v.window_total = 34;
        assert!(render_human(&v).contains("showing 1 of 34"), "{v:?}");
    }

    // ── the header describes the window, not the table ──────────────

    #[test]
    fn window_split_counts_the_whole_window_and_cuts_only_the_slice() {
        // Five AI, four human — the shape `--limit 6` used to misreport.
        let window: Vec<bool> = (0..9).map(|i| i % 2 == 0).collect();
        let (ai_6, shown_6) = window_split(&window, 6, |ai| *ai);
        let (ai_100, shown_100) = window_split(&window, 100, |ai| *ai);
        assert_eq!((ai_6, ai_100), (5, 5));
        // Only the slice moves.
        assert_eq!(shown_6.len(), 6);
        assert_eq!(shown_100.len(), 9, "a limit past the end cannot overrun");
        assert_eq!(window_split(&window, 0, |ai| *ai).1.len(), 0);
        assert_eq!(window_split::<bool>(&[], 6, |ai| *ai), (0, &[][..]));
    }

    #[test]
    fn the_header_counts_the_window_however_small_the_limit() {
        let window: Vec<Row> = (0..9)
            .map(|i| row(100 + i, i % 2 == 0, 1.0, 1_000_000))
            .collect();

        // Exactly what `cmd_roi` does: split the window, then take the slice.
        let header_at = |limit: usize| {
            let (window_ai, shown) = window_split(&window, limit, |r| r.ai_assisted);
            let mut v = view(shown.to_vec());
            v.window_total = window.len();
            v.window_ai = window_ai;
            render_human(&v).lines().next().unwrap().to_string()
        };
        let six = header_at(6);
        let hundred = header_at(100);

        assert!(six.contains("5 AI-assisted PRs, 4 human-authored"), "{six}");
        // The counts are the window's at every limit; only `showing` moves.
        assert_eq!(
            six.split_once(", 30d").unwrap().0,
            hundred.split_once(", 30d").unwrap().0,
            "--limit changed the split:\n{six}\n{hundred}"
        );
        assert!(six.contains("showing 6 of 9"), "{six}");
        assert!(!hundred.contains("showing"), "{hundred}");
        // Pre-fix the six-row table reported its own 3/3 split beside a
        // window total of 9 — three numbers, two populations.
        assert!(!six.contains("3 AI-assisted"), "counted the table:\n{six}");
        // And the two halves still sum to the total the same line announces.
        let mut v = view(vec![row(1, true, 1.0, 0)]);
        v.window_total = 23;
        v.window_ai = 9;
        assert!(
            render_human(&v).contains("9 AI-assisted PRs, 14 human-authored"),
            "{v:?}"
        );
    }

    // ── how firm each row's tokens are ──────────────────────────────

    #[test]
    fn a_weak_row_is_marked_and_one_legend_line_says_what_the_mark_means() {
        let mut weak = row(41, true, 2.0, 4_000_000);
        weak.attribution_confidence = 0.2;
        let text = render_human(&view(vec![weak, row(42, true, 2.0, 4_000_000)]));
        assert!(line(&text, "#41").ends_with('~'), "unmarked weak row:\n{text}");
        assert!(!line(&text, "#42").contains('~'), "marked a strong row:\n{text}");
        // The mark does not cost the number its column.
        assert!(line(&text, "#41").contains("806k eq~"), "{text}");
        assert!(line(&text, "#42").contains("806k eq"), "{text}");
        // Exactly one legend line, and it names the threshold from the constant.
        let legend: Vec<&str> = text
            .lines()
            .filter(|l| l.trim_start().starts_with("~ ="))
            .collect();
        assert_eq!(legend.len(), 1, "{text}");
        assert!(legend[0].contains("confidence ≤ 0.30"), "{}", legend[0]);

        // The threshold is inclusive: 0.30 is the mention-only score.
        let mut edge = row(43, true, 2.0, 4_000_000);
        edge.attribution_confidence = WEAK_CONFIDENCE;
        assert!(line(&render_human(&view(vec![edge])), "#43").ends_with('~'));

        // Nothing attributed is not a weak match — and with no token figure
        // anywhere, the mark never appears, so the legend stays away.
        let none = render_human(&view(vec![row(44, true, 2.0, 0)]));
        assert!(!line(&none, "#44").contains('~'), "{none}");
        assert!(!none.contains("~ ="), "legend with nothing to qualify:\n{none}");
        assert!(!is_weak(0.0, 0.0), "no tokens is not a weak attribution");
    }

    #[test]
    fn the_rollup_names_the_weak_share_and_cautions_only_past_half() {
        let mut weak = row(1, true, 1.0, 1_000_000);
        weak.attribution_confidence = 0.2;
        // 1M raw weak against 4M raw strong, one shape ⇒ 20% of the volume.
        let text = render_human(&view(vec![weak.clone(), row(2, true, 1.0, 4_000_000)]));
        let att = line(&text, "attribution:");
        assert!(att.contains("0.76 mean confidence"), "{att}");
        assert!(
            att.contains("20% of volume from weak (mention-only) matches"),
            "{att}"
        );
        assert!(!text.contains("caution:"), "cautioned under half:\n{text}");

        // One big guess beside a small certainty: 90% of the volume is weak.
        let mut big_weak = row(3, true, 1.0, 9_000_000);
        big_weak.attribution_confidence = 0.1;
        let text = render_human(&view(vec![row(2, true, 1.0, 1_000_000), big_weak]));
        assert!(line(&text, "attribution:").contains("90% of volume"), "{text}");
        let caution = line(&text, "caution:");
        assert!(caution.contains("inferred from PR mentions"), "{caution}");
        assert!(caution.contains("not file overlap"), "{caution}");
        assert_eq!(text.matches("caution:").count(), 1, "{text}");

        // By VOLUME, not by row — and never a share of nothing.
        assert_eq!(view(vec![weak]).totals().weak_share, Some(1.0));
        assert_eq!(view(vec![row(9, true, 1.0, 0)]).totals().weak_share, None);
        assert_eq!(weak_volume_share(&[]), None);
        // A half-and-half split is not "mostly" — the caution stays shut.
        let mut half = row(4, true, 1.0, 4_000_000);
        half.attribution_confidence = 0.3;
        let t = view(vec![half, row(5, true, 1.0, 4_000_000)]).totals();
        assert_eq!(t.weak_share, Some(0.5));
        assert!(!render_human(&view(Vec::new())).contains("caution:"));
    }

    #[test]
    fn json_publishes_the_window_split_the_confidence_and_the_weak_share() {
        let mut weak = row(1, true, 1.0, 1_000_000);
        weak.attribution_confidence = 0.2;
        let mut v = view(vec![weak, row(2, true, 1.0, 4_000_000)]);
        v.window_total = 23;
        v.window_ai = 9;
        let doc = render_json(&v);

        // The window, not the slice `prs` carries.
        assert_eq!(doc["window"]["merged_prs"], 23);
        assert_eq!(doc["window"]["ai_prs"], 9);
        assert_eq!(doc["window"]["human_prs"], 14);
        assert_eq!(doc["window"]["shown"], 2);

        // Per-PR strength as a number, plus the threshold that turns it into
        // the table's `~`, so a consumer can reproduce the mark exactly.
        assert!(close(doc["prs"][0]["attribution_confidence"].as_f64().unwrap(), 0.2));
        assert!(close(doc["prs"][1]["attribution_confidence"].as_f64().unwrap(), 0.9));
        assert!(close(doc["weak_confidence_threshold"].as_f64().unwrap(), 0.3));
        assert!(close(
            doc["totals"]["attribution_weak_volume_share"].as_f64().unwrap(),
            0.2
        ));

        // Nothing attributed ⇒ an explicit null, not a zero share that would
        // read as "none of this is guesswork".
        let empty = render_json(&view(vec![row(1, true, 1.0, 0)]));
        assert!(empty["totals"]["attribution_weak_volume_share"].is_null());
        assert!(empty["totals"]
            .as_object()
            .unwrap()
            .contains_key("attribution_weak_volume_share"));
    }

    #[test]
    fn a_rate_with_nothing_attributed_refuses_to_call_the_work_free() {
        let mut v = view(vec![row(1, true, 2.0, 0)]);
        v.usd_per_mtok = Some(3.0);
        let text = render_human(&v);
        assert!(!text.contains("$0.00"), "priced nothing as free:\n{text}");
        assert!(text.contains("no tokens attributed"), "{text}");
        // The per-row cell agrees with the rollup.
        assert!(!text.lines().any(|l| l.contains("#1") && l.contains('$')), "{text}");
    }

    // ── the equivalent, and the raw mix beside it ───────────────────

    #[test]
    fn the_rollup_leads_with_the_equivalent_and_shows_the_raw_mix_beneath_it() {
        let text = render_human(&view(vec![row(1, true, 2.0, 4_000_000)]));

        // The headline is the equivalent — the raw 4.0M is NOT what leads.
        let head = line(&text, "tokens (input-equiv):");
        assert!(head.contains("806k eq"), "{text}");
        assert!(!head.contains("4.0M"), "raw sum in the headline:\n{text}");

        // Directly beneath: every raw class, and a total the four sum to, so a
        // reader sees both numbers and can reconcile them without the docs.
        let raw = line(&text, "raw mix:");
        assert!(raw.contains("32k fresh"), "{raw}");
        assert!(raw.contains("260k cache-write"), "{raw}");
        assert!(raw.contains("3.7M cache-read"), "{raw}");
        assert!(raw.contains("16k out"), "{raw}");
        assert!(raw.contains("(raw total 4.0M)"), "{raw}");
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains("tokens (input-equiv):"))
            .unwrap();
        assert!(lines[at + 1].contains("raw mix:"), "not adjacent:\n{text}");

        // The ratio divides the EQUIVALENT: 806_200 / 2.00 = 403_100 eq. Off
        // the raw total the same row would have read 2.0M.
        let per_unit = line(&text, "tokens per unit:");
        assert!(per_unit.contains("403k eq"), "{text}");
        assert!(!per_unit.contains("2.0M"), "divided the raw total:\n{text}");

        // And one line says why the two figures differ.
        assert!(text.contains("cache reads bill at roughly a tenth"), "{text}");
        assert!(text.contains("cache-read 0.1×"), "{text}");
    }

    #[test]
    fn the_table_column_is_the_equivalent_and_stays_one_column() {
        let text = render_human(&view(vec![row(412, true, 2.0, 4_000_000)]));
        let pr = line(&text, "#412");
        assert!(pr.contains("806k eq"), "{text}");
        assert!(!pr.contains("4.0M"), "raw sum in the table:\n{text}");
        // Narrow: one token column, not four class columns.
        let head = line(&text, "units");
        assert!(!head.contains("cache"), "class columns leaked:\n{head}");
    }

    // ── the label-count message ─────────────────────────────────────

    #[test]
    fn labels_needed_counts_down_to_the_threshold() {
        assert_eq!(labels_needed(0), MIN_LABELS);
        assert_eq!(labels_needed(MIN_LABELS - 1), 1);
        assert_eq!(labels_needed(MIN_LABELS), 0);
        assert_eq!(labels_needed(MIN_LABELS + 5), 0);
    }

    #[test]
    fn labels_hint_names_the_count_the_command_and_a_real_pr() {
        let hint = labels_hint(3, Some(412)).expect("locked below the threshold");
        assert!(hint.contains("5 more labels needed (3/8)"), "{hint}");
        assert!(hint.contains("modelstat label 412 <minutes>"), "{hint}");
        assert!(labels_hint(1, None).unwrap().contains("modelstat label <pr>"));
        assert!(labels_hint(7, None).unwrap().contains("1 more label needed"));
        assert_eq!(labels_hint(MIN_LABELS, Some(1)), None);
    }

    // ── the invariants, read off the rendered text ──────────────────

    #[test]
    fn no_calibration_means_no_hours_anywhere_and_one_actionable_line() {
        let v = view(vec![row(412, true, 2.0, 4_000_000), row(9, false, 1.0, 0)]);
        let text = render_human(&v);
        // The ONLY line allowed to say "hours" is the one saying they are
        // locked — so no column header, no per-row figure, no rollup line.
        let hours_lines: Vec<&str> = text.lines().filter(|l| l.contains("hours")).collect();
        assert_eq!(hours_lines.len(), 1, "hours leaked:\n{text}");
        assert!(hours_lines[0].contains("hours: locked"), "{text}");
        assert!(!text.to_lowercase().contains("loocv"), "{text}");
        assert!(!text.contains('$'), "{text}");
        // Exactly one line about labels, and it is actionable.
        let hint_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("modelstat label"))
            .collect();
        assert_eq!(hint_lines.len(), 1, "{text}");
        assert!(hint_lines[0].contains("modelstat label 412 <minutes>"));
    }

    #[test]
    fn no_rate_means_no_dollar_figure_anywhere() {
        let v = with_hours(view(vec![row(1, true, 2.0, 4_000_000)]));
        let text = render_human(&v);
        assert!(!text.contains('$'), "money without a rate:\n{text}");
        assert!(!text.contains("usd"), "{text}");
    }

    #[test]
    fn a_rate_turns_on_dollars_and_only_dollars() {
        let mut v = view(vec![row(1, true, 2.0, 4_000_000)]);
        v.usd_per_mtok = Some(2.5);
        let text = render_human(&v);
        // 806_200 eq at $2.50/Mtok. The raw 4.0M would have said $10.00.
        assert!(text.contains("$2.02"), "806k eq at $2.50/Mtok:\n{text}");
        assert!(!text.contains("$10.00"), "priced the raw total:\n{text}");
        // Still no hours: a rate buys money, never a calibration.
        assert!(!text.to_lowercase().contains("loocv"), "{text}");
        assert!(text.contains("modelstat label"), "{text}");
    }

    #[test]
    fn a_calibration_turns_on_hours_next_to_its_own_error() {
        let v = with_hours(view(vec![row(1, true, 2.0, 4_000_000)]));
        let text = render_human(&v);
        assert!(text.contains("hours:"), "{text}");
        assert!(text.contains("LOOCV, n=12"), "{text}");
        // Per equivalent million, like every other ratio in the rollup.
        assert!(text.contains("hours per 1M eq"), "{text}");
        // The unlock line is gone once it is unlocked.
        assert!(!text.contains("modelstat label"), "{text}");
    }

    #[test]
    fn empty_window_still_renders_a_rollup_and_refuses_to_divide() {
        let text = render_human(&view(Vec::new()));
        assert!(text.contains("no merged PRs in this window"), "{text}");
        assert!(line(&text, "tokens per unit:").contains('—'), "{text}");
        assert!(!text.contains('$'));
        // The mix line still prints, as zeros: an em-dash inside a sum is not
        // a number.
        assert!(line(&text, "raw mix:").contains("(raw total 0)"), "{text}");
        // An empty f64 sum folds from -0.0; `-0.00 effort units` is nonsense.
        assert!(text.contains("(0.00 effort units)"), "{text}");
        assert!(!text.contains("-0.00"), "{text}");
        assert_eq!(view(Vec::new()).totals().units.to_string(), "0");
        // A repo with only human PRs hits the same path.
        let human_only = render_human(&view(vec![row(1, false, 6.5, 0)]));
        assert!(!human_only.contains("-0.00"), "{human_only}");
    }

    // ── the machine form ────────────────────────────────────────────

    #[test]
    fn json_carries_explicit_nulls_for_what_is_unavailable() {
        let doc = render_json(&view(vec![row(1, true, 2.0, 4_000_000)]));
        assert!(doc["calibration"].is_null());
        assert!(doc["usd_per_mtok"].is_null());
        assert!(doc["prs"][0]["hours"].is_null(), "{doc}");
        assert!(doc["prs"][0]["usd"].is_null(), "{doc}");
        assert!(doc["totals"]["hours"].is_null());
        assert!(doc["totals"]["hours_per_mtok"].is_null());
        assert!(doc["totals"]["usd"].is_null());
        // Present-and-null, not absent — a consumer must SEE the absence.
        assert!(doc["totals"].as_object().unwrap().contains_key("hours"));
        assert!(doc["prs"][0].as_object().unwrap().contains_key("usd"));
        assert_eq!(doc["labels"]["needed_for_hours"], 5);
        assert!(close(
            doc["totals"]["tokens_per_effort_unit"].as_f64().unwrap(),
            403_100.0
        ));
        // Valid JSON, round-trips.
        let text = serde_json::to_string(&doc).unwrap();
        serde_json::from_str::<Value>(&text).unwrap();
    }

    #[test]
    fn json_carries_the_whole_raw_mix_beside_the_equivalent() {
        let doc = render_json(&view(vec![row(1, true, 2.0, 4_000_000)]));
        let pr = &doc["prs"][0];
        // Every class, by name — nothing collapsed, nothing replaced.
        assert_eq!(pr["mix"]["input"], 32_000);
        assert_eq!(pr["mix"]["output"], 16_000);
        assert_eq!(pr["mix"]["cache_creation"], 260_000);
        assert_eq!(pr["mix"]["cache_read"], 3_692_000);
        assert_eq!(pr["mix"]["reasoning"], 0);
        assert_eq!(pr["raw_total"], 4_000_000);
        assert!(close(pr["equiv_tokens"].as_f64().unwrap(), 806_200.0), "{pr}");

        let t = &doc["totals"];
        assert_eq!(t["raw_total"], 4_000_000);
        assert_eq!(t["mix"]["cache_read"], 3_692_000);
        assert!(close(t["equiv_tokens"].as_f64().unwrap(), 806_200.0), "{t}");

        // The device-wide leftovers carry the same pair, not a bare total.
        let un = &t["unattributed_device"];
        assert_eq!(un["raw_total"], 2_000_000);
        assert_eq!(un["mix"]["cache_read"], 1_846_000);
        assert!(close(un["equiv_tokens"].as_f64().unwrap(), 403_100.0), "{un}");

        // The weights are published, so the equivalent is re-derivable and a
        // consumer on another provider can see these are Anthropic-family.
        assert_eq!(doc["equiv_weights"]["input"], 1.0);
        assert_eq!(doc["equiv_weights"]["cache_creation"], 1.25);
        assert_eq!(doc["equiv_weights"]["cache_read"], 0.1);
        assert_eq!(doc["equiv_weights"]["output"], 5.0);
    }

    #[test]
    fn json_fills_hours_and_usd_once_they_are_earned() {
        let mut v = with_hours(view(vec![row(1, true, 2.0, 4_000_000)]));
        v.usd_per_mtok = Some(2.5);
        let doc = render_json(&v);
        assert!(doc["prs"][0]["hours"]["p50"].as_f64().unwrap() > 0.0);
        // Priced off the equivalent: 0.8062 Mtok × $2.50, not 4.0 × $2.50.
        assert!(close(doc["prs"][0]["usd"].as_f64().unwrap(), 2.0155), "{doc}");
        assert!(close(doc["totals"]["usd"].as_f64().unwrap(), 2.0155), "{doc}");
        assert!(doc["totals"]["hours"].as_f64().unwrap() > 0.0);
        assert_eq!(doc["calibration"]["n"], 12);
    }

    // ── flags ───────────────────────────────────────────────────────

    #[test]
    fn flags_default_and_parse() {
        let none = parse_roi_opts(&[]).unwrap();
        assert_eq!((none.repo.as_str(), none.days, none.limit), (".", 30, 20));
        assert!(!none.json && none.usd_per_mtok.is_none());

        let a: Vec<String> = ["--repo", "/tmp/x", "--days", "7", "--limit=3", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_roi_opts(&a).unwrap();
        assert_eq!((o.repo.as_str(), o.days, o.limit, o.json), ("/tmp/x", 7, 3, true));
    }

    #[test]
    fn a_bad_number_is_an_error_not_a_silent_default() {
        let a: Vec<String> = vec!["--days".into(), "thirty".into()];
        assert!(parse_roi_opts(&a).unwrap_err().contains("--days"));
        let a: Vec<String> = vec!["--usd-per-mtok".into(), "0".into()];
        assert!(parse_roi_opts(&a).unwrap_err().contains("positive"));
        let a: Vec<String> = vec!["--usd-per-mtok".into(), "-3".into()];
        assert!(parse_roi_opts(&a).is_err());
    }

    #[test]
    fn token_formatting_is_compact_and_says_nothing_when_there_is_nothing() {
        assert_eq!(fmt_tokens(0), "—");
        assert_eq!(fmt_tokens(312), "312");
        assert_eq!(fmt_tokens(845_000), "845k");
        assert_eq!(fmt_tokens(1_240_000), "1.2M");
        // Inside a sum, zero is a number.
        assert_eq!(fmt_count(0), "0");
        // Equivalents are tagged so they can never be misread as raw counts.
        assert_eq!(fmt_equiv(806_200.0), "806k eq");
        assert_eq!(fmt_equiv(0.0), "—");
    }
}
