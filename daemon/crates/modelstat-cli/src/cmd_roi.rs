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
//! `total_tokens` is every token the session spent —
//! [`modelstat_effort::attribution`]'s five buckets are disjoint, and
//! `input_tokens` therefore INCLUDES cache writes and cache reads, which
//! dominate on a long agent session. So the table's `tokens` column is total
//! spend, not fresh prompt tokens, and `--usd-per-mtok` prices all of it at one
//! rate: a blended figure, and the reason the rate is the user's to choose
//! rather than a table this device carries.
//!
//! The session→PR join is reference-based (a branch name, a pasted PR URL), so
//! the rollup publishes its own token-weighted confidence next to the totals.
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

use modelstat_effort::attribution::{self, PrSpend};
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
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tokens: u64,
    pub sessions: u32,
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
    pub unattributed_tokens: u64,
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
    pub tokens: u64,
    pub sessions: u32,
    /// `None` when there is no effort to divide by — never `0.0`, which would
    /// read as "free".
    pub tokens_per_unit: Option<f64>,
    pub hours: Option<f64>,
    pub hours_per_mtok: Option<f64>,
    pub usd: Option<f64>,
    /// Token-weighted mean of the rows' `attribution_confidence`. `None` when
    /// nothing was attributed. Printed because the join is reference-based, not
    /// git-certain: a single-PR session scores 0.6 and a split one 0.3, so
    /// reading these token figures as exact would overstate what they are.
    pub confidence: Option<f64>,
}

impl RoiView {
    pub fn totals(&self) -> Totals {
        let ai: Vec<&Row> = self.rows.iter().filter(|r| r.ai_assisted).collect();
        // `impl Sum for f64` folds from `-0.0`, so an empty AI set sums to
        // negative zero and renders as `-0.00 effort units`. Real, and a
        // nonsense thing to show a user.
        let units: f64 = unsign_zero(ai.iter().map(|r| r.units).sum());
        let tokens: u64 = ai.iter().map(|r| r.tokens).sum();
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
            tokens,
            sessions,
            tokens_per_unit: tokens_per_unit(tokens, units),
            hours,
            hours_per_mtok: hours.and_then(|h| per_mtok(h, tokens)),
            usd: self.usd_per_mtok.map(|rate| usd_for_tokens(tokens, rate)),
            confidence: weighted_confidence(&ai),
        }
    }
}

// ── pure arithmetic ─────────────────────────────────────────────────────

/// Tokens spent per unit of effort delivered. `None` when the denominator is
/// zero or unusable — dividing by no effort yields infinity, and printing that
/// as an efficiency figure would be worse than printing nothing.
pub fn tokens_per_unit(tokens: u64, units: f64) -> Option<f64> {
    (units.is_finite() && units > 0.0).then(|| tokens as f64 / units)
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

/// `value` per million tokens. `None` when no tokens were attributed.
pub fn per_mtok(value: f64, tokens: u64) -> Option<f64> {
    (tokens > 0).then(|| value / (tokens as f64 / 1e6))
}

/// Dollars for `tokens` at `usd_per_mtok`. The only place money is ever
/// derived, and it is only ever called with a rate the user typed.
pub fn usd_for_tokens(tokens: u64, usd_per_mtok: f64) -> f64 {
    tokens as f64 / 1e6 * usd_per_mtok
}

/// Token-weighted mean attribution confidence. Weighted by tokens, not by row,
/// because one 300M-token PR joined on a pasted URL says more about how much of
/// the total is guesswork than nine small PRs joined on a branch name.
pub fn weighted_confidence(rows: &[&Row]) -> Option<f64> {
    let tokens: u64 = rows.iter().map(|r| r.tokens).sum();
    (tokens > 0).then(|| {
        rows.iter()
            .map(|r| r.attribution_confidence * r.tokens as f64)
            .sum::<f64>()
            / tokens as f64
    })
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
    let shown = if view.window_total > view.rows.len() {
        format!(", showing {} of {}", view.rows.len(), view.window_total)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "{} — {} AI-assisted PRs, {} human-authored, {}d{shown}  \
         (baseline: {} human anchors)\n",
        view.slug, t.ai_prs, t.human_prs, view.days, view.anchor_n
    ));

    if view.rows.is_empty() {
        out.push_str("\n  (no merged PRs in this window)\n");
    } else {
        let mut head = format!("\n  {:>6}  {:<5} {:>7} {:>5} {:>9}", "PR", "who", "units", "pct", "tokens");
        if hours_on {
            head.push_str(&format!(" {:>7}", "hours"));
        }
        if usd_rate.is_some() {
            head.push_str(&format!(" {:>9}", "usd"));
        }
        out.push_str(&head);
        out.push('\n');

        for r in &view.rows {
            let mut line = format!(
                "  {:>6}  {:<5} {:>7.2} {:>4.0}% {:>9}",
                format!("#{}", r.pr_number),
                if r.ai_assisted { "AI" } else { "human" },
                r.units,
                r.percentile * 100.0,
                fmt_tokens(r.tokens),
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
                    match r.tokens {
                        0 => "—".to_string(),
                        n => fmt_usd(usd_for_tokens(n, rate)),
                    }
                ));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }

    // Rollup.
    out.push('\n');
    out.push_str(&format!(
        "  AI PRs:            {}  ({:.2} effort units)\n",
        t.ai_prs, t.units
    ));
    out.push_str(&format!(
        "  tokens:            {} across {} session{}\n",
        fmt_tokens(t.tokens),
        t.sessions,
        if t.sessions == 1 { "" } else { "s" }
    ));
    // Two different blanks, and the difference matters: no effort to divide by
    // versus no tokens to divide.
    match t.tokens_per_unit {
        _ if t.tokens == 0 => {
            out.push_str("  tokens per unit:   — (no tokens attributed to these PRs)\n")
        }
        Some(tpu) => out.push_str(&format!(
            "  tokens per unit:   {}\n",
            fmt_tokens(tpu.round() as u64)
        )),
        None => out.push_str("  tokens per unit:   — (no AI effort in this window)\n"),
    }
    // Money exists only inside this binding — there is no other path to a `$`.
    if let (Some(rate), Some(usd)) = (usd_rate, t.usd) {
        if t.tokens == 0 {
            // `$0.00` against no attributed tokens reads as "the AI work was
            // free". It was not measured, which is a different claim.
            out.push_str("  spend:             — (no tokens attributed to these PRs)\n");
        } else {
            out.push_str(&format!("  spend:             {}\n", fmt_usd(usd)));
            if let Some(per_unit) = t.tokens_per_unit {
                out.push_str(&format!(
                    "  cost per unit:     {}\n",
                    fmt_usd(usd_for_tokens(per_unit.round() as u64, rate))
                ));
            }
        }
    }
    // The join is reference-based, so say how firm it is before anyone quotes
    // the number above.
    if let Some(c) = t.confidence {
        out.push_str(&format!(
            "  attribution:       {c:.2} mean confidence (session→PR references)\n"
        ));
    }
    // Device-wide, NOT this repo: the session scan reads every tool log on the
    // machine. Labelling it as repo-scoped would understate the denominator by
    // however many repos the person also works in.
    if view.spend_available {
        out.push_str(&format!(
            "  unattributed:      {} across {} of {} sessions (device-wide)\n",
            fmt_tokens(view.unattributed_tokens),
            view.unattributed_sessions,
            view.sessions_scanned
        ));
    } else {
        out.push_str("  unattributed:      — (no tool session logs found on this device)\n");
    }

    // Tier 2, and only Tier 2.
    match (&view.calibration, t.hours) {
        (Some(cal), Some(hours)) => {
            out.push_str(&format!(
                "  hours:             {}  ± {:.0}% (LOOCV, n={})\n",
                fmt_hours(hours),
                cal.median_abs_pct_error(),
                cal.n()
            ));
            match t.hours_per_mtok {
                Some(hpm) => out.push_str(&format!("  hours per 1M tok:  {hpm:.2}\n")),
                None => out.push_str("  hours per 1M tok:  — (no tokens attributed)\n"),
            }
        }
        _ => {
            let example = view.rows.iter().find(|r| r.ai_assisted).map(|r| r.pr_number);
            if let Some(hint) = labels_hint(view.label_count, example) {
                out.push_str(&format!("  {hint}\n"));
            }
        }
    }
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
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "total_tokens": r.tokens,
                "session_count": r.sessions,
                "attribution_confidence": r.attribution_confidence,
                "hours": r.hours_p50.map(|p50| json!({
                    "p10": r.hours_p10,
                    "p50": p50,
                    "p90": r.hours_p90,
                })),
                "usd": view.usd_per_mtok.map(|rate| usd_for_tokens(r.tokens, rate)),
            })
        })
        .collect();

    json!({
        "repo": view.slug,
        "days": view.days,
        "anchor_n": view.anchor_n,
        "spend_available": view.spend_available,
        "usd_per_mtok": view.usd_per_mtok,
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
            "total_tokens": t.tokens,
            "session_count": t.sessions,
            "tokens_per_effort_unit": t.tokens_per_unit,
            "attribution_confidence": t.confidence,
            // Device-wide, not repo-scoped: the session scan reads every tool
            // log on this machine, across every repo.
            "unattributed_tokens_device": view.unattributed_tokens,
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
    let rows: Vec<Row> = in_window
        .into_iter()
        .take(opts.limit)
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
                input_tokens: spend.map_or(0, |s| s.input_tokens),
                output_tokens: spend.map_or(0, |s| s.output_tokens),
                tokens: spend.map_or(0, |s| s.total_tokens),
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
        unattributed_tokens: spend.unattributed_tokens,
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

    fn row(pr: u64, ai: bool, units: f64, tokens: u64) -> Row {
        Row {
            pr_number: pr,
            ai_assisted: ai,
            merged_at: "2026-08-01T00:00:00Z".into(),
            units,
            percentile: 0.5,
            judged: false,
            input_tokens: tokens / 2,
            output_tokens: tokens - tokens / 2,
            tokens,
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
            rows,
            unattributed_tokens: 500_000,
            unattributed_sessions: 4,
            sessions_scanned: 40,
            label_count: 3,
            calibration: None,
            usd_per_mtok: None,
            spend_available: true,
        }
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
        assert_eq!(tokens_per_unit(3_000_000, 1.5), Some(2_000_000.0));
        assert_eq!(tokens_per_unit(3_000_000, 0.0), None);
        assert_eq!(tokens_per_unit(0, 2.0), Some(0.0));
        assert_eq!(tokens_per_unit(10, f64::NAN), None);
    }

    #[test]
    fn per_mtok_scales_by_millions_and_refuses_zero_tokens() {
        assert_eq!(per_mtok(4.0, 2_000_000), Some(2.0));
        assert_eq!(per_mtok(4.0, 0), None);
    }

    #[test]
    fn usd_is_tokens_times_rate_per_million() {
        assert!((usd_for_tokens(2_500_000, 3.0) - 7.5).abs() < 1e-9);
        assert_eq!(usd_for_tokens(0, 3.0), 0.0);
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
        assert_eq!(t.tokens, 4_000_000);
        assert_eq!(t.tokens_per_unit, Some(1_000_000.0));
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
        assert_eq!(v.totals().usd, Some(10.0));
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
        assert!(text.contains("$10.00"), "4M tokens at $2.50/Mtok:\n{text}");
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
        assert!(text.contains("hours per 1M tok"), "{text}");
        // The unlock line is gone once it is unlocked.
        assert!(!text.contains("modelstat label"), "{text}");
    }

    #[test]
    fn empty_window_still_renders_a_rollup_and_refuses_to_divide() {
        let text = render_human(&view(Vec::new()));
        assert!(text.contains("no merged PRs in this window"), "{text}");
        assert!(text.contains("tokens per unit:   —"), "{text}");
        assert!(!text.contains('$'));
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
        assert_eq!(doc["totals"]["tokens_per_effort_unit"], 2_000_000.0);
        // Valid JSON, round-trips.
        let text = serde_json::to_string(&doc).unwrap();
        serde_json::from_str::<Value>(&text).unwrap();
    }

    #[test]
    fn json_fills_hours_and_usd_once_they_are_earned() {
        let mut v = with_hours(view(vec![row(1, true, 2.0, 4_000_000)]));
        v.usd_per_mtok = Some(2.5);
        let doc = render_json(&v);
        assert!(doc["prs"][0]["hours"]["p50"].as_f64().unwrap() > 0.0);
        assert_eq!(doc["prs"][0]["usd"], 10.0);
        assert_eq!(doc["totals"]["usd"], 10.0);
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
    }
}
