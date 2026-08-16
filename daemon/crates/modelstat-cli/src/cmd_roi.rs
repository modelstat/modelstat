//! `modelstat roi` — what merged in this repo, and what the AI spend beside it
//! was, measured on this device.
//!
//! Everything here is local: the repo's own git history, the change primitives
//! [`modelstat_work::diff_features`] reads off each merge, and the session
//! totals joined to PRs by [`modelstat_work::attribution`]. No network, no
//! server, no forge token.
//!
//! ## This command reports primitives. It does not score anyone.
//!
//! There is no composite number here, and adding one would be a regression.
//! Every figure printed is a count of something that happened — merged PRs, who
//! signed them, files changed, lines added and deleted, sessions,
//! input-equivalent tokens, active time — and every one of them is traceable to
//! the rows that produced it.
//!
//! The alternative was built and deleted: a weighted blend of churn, files,
//! hunks and languages, normalised to a repo median and cross-fitted to
//! hand-written labels. It read as data and behaved as a verdict. Two things
//! killed it. **Different teams value different things** — the weights were our
//! opinion imposed on the customer, and no weighting is defensible for all of
//! them. And **a blended number cannot be audited**: asked "how do I know this
//! is right?", the only honest answer was to explain the weights, which is a
//! conversation about our judgement rather than about their work. Tokens and
//! time and lines are concrete; we show them and stop.
//!
//! Ranking is the reader's, not ours: [`Sort`] orders by one column the user
//! named, and the default (`recent`) is merge order, which asserts nothing.
//!
//! ## The two refusals this command still enforces
//!
//! * **No dollars without `--usd-per-mtok`.** The daemon deliberately holds no
//!   price table (pricing is a server concern, and a stale table quietly
//!   invents money). Tokens are counted locally and exactly; the rate is the
//!   user's to supply.
//! * **Unknown is never zero.** A PR whose diff git could not read prints `—`
//!   for its change primitives, not `0` — "changed nothing" and "we could not
//!   look" are different facts, and only one of them flatters the reader.
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
//! [`modelstat_work::attribution`]) and every raw class stays visible: the
//! rollup prints the mix directly beneath the equivalent, and `--json` carries
//! both plus the weights themselves. A derived number never replaces a measured
//! one here — it sits next to it.
//!
//! ## What the time figures are
//!
//! `active_ms` is the union of five-minute windows around a session's event
//! timestamps (`modelstat_work::attribution::active_ms`), so a lunch break in
//! the middle of a session is not billed as work. It is apportioned to PRs by
//! the same weights the token split uses, which is why time and tokens can
//! never disagree about which PR a session belongs to. It sits BESIDE the
//! shipped work and is never multiplied into it: this command states what was
//! spent and what landed, and refuses to claim the one caused the other.
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
//! machine, not just this repo's — and carries time as well as tokens, because
//! spend nobody can place is the one figure a reader must never have to infer.
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

use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::git_anchors::select_anchor_commits;
use modelstat_parsers::git_outcome::parse_git_log;
use modelstat_parsers::{mine_repo_anchors, AnchorConfig};
use modelstat_work::attribution::{
    self, PrSpend, TokenMix, W_CACHE_READ, W_CACHE_WRITE, W_INPUT, W_OUTPUT,
};
use modelstat_work::diff_features;
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

// ── flags ───────────────────────────────────────────────────────────────

/// Which measured column the table is ordered by.
///
/// Every variant names a quantity that was counted. There is deliberately no
/// "best" / "impact" / "efficiency" ordering: that would be a composite by
/// another name, and choosing its weights is the customer's business.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Merge order, newest first — git's own order, asserting nothing.
    #[default]
    Recent,
    Pr,
    Files,
    Added,
    Deleted,
    /// `added + deleted`.
    Lines,
    Sessions,
    /// Input-equivalent tokens.
    Tokens,
    /// Active time.
    Active,
}

impl Sort {
    /// The `--sort` vocabulary, in help-text order.
    pub const NAMES: [&'static str; 9] = [
        "recent", "pr", "files", "added", "deleted", "lines", "sessions", "tokens", "active",
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "recent" => Self::Recent,
            "pr" => Self::Pr,
            "files" => Self::Files,
            "added" => Self::Added,
            "deleted" => Self::Deleted,
            "lines" => Self::Lines,
            "sessions" => Self::Sessions,
            "tokens" => Self::Tokens,
            "active" => Self::Active,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

#[derive(Debug)]
pub struct RoiOpts {
    pub repo: String,
    pub days: u32,
    pub limit: usize,
    pub sort: Sort,
    pub json: bool,
    /// Dollars per million tokens. `None` — the default — means this command
    /// prints no money at all.
    pub usd_per_mtok: Option<f64>,
}

/// `--flag value` / `--flag=value`, the shape `cmd_admin::flag_value` uses.
fn flag_value(args: &[String], name: &str) -> Option<String> {
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
fn parse_flag<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
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
            let rate: f64 = v.parse().map_err(|_| {
                format!("modelstat roi: --usd-per-mtok expects a number, got `{v}`")
            })?;
            if !rate.is_finite() || rate <= 0.0 {
                return Err("modelstat roi: --usd-per-mtok must be a positive number".into());
            }
            Some(rate)
        }
    };
    let sort = match flag_value(args, "--sort") {
        None => Sort::default(),
        Some(v) => Sort::parse(&v).ok_or_else(|| {
            format!(
                "modelstat roi: --sort expects one of {}, got `{v}`",
                Sort::NAMES.join(" | ")
            )
        })?,
    };
    Ok(RoiOpts {
        repo: flag_value(args, "--repo").unwrap_or_else(|| ".".to_string()),
        days: parse_flag(args, "--days", 30u32)?,
        limit: parse_flag(args, "--limit", 20usize)?,
        sort,
        json: args.iter().any(|a| a == "--json"),
        usd_per_mtok,
    })
}

/// `modelstat roi --help`. Says what the command measures and, just as loudly,
/// what it refuses to compute — a reader who came looking for a productivity
/// score should find out here rather than from a column that isn't there.
pub fn help_text() -> String {
    format!(
        "usage: modelstat roi [--repo PATH] [--days N] [--limit N] [--sort KEY] [--json]\n\
        \x20                    [--usd-per-mtok RATE]\n\
        \n\
        What merged in this repository, and what the AI spend beside it was. Every\n\
        figure is a count of something that happened: merged PRs, who authored them\n\
        (the tools sign their own commits), files changed, lines added and deleted,\n\
        sessions, input-equivalent tokens, and active time.\n\
        \n\
        This reports measured quantities. It does not score anyone. There is no\n\
        composite, no productivity index and no ranking of people — different teams\n\
        value different things, and a blended number is a verdict you cannot audit.\n\
        Tokens and time sit BESIDE what shipped; they are never multiplied into a\n\
        claim about hours saved. Sort by a column you chose and read it yourself.\n\
        \n\
        \x20 --repo PATH        repository to read (default: the current directory's)\n\
        \x20 --days N           merge window, in days (default: 30)\n\
        \x20 --limit N          rows printed (default: 20). The rollup always covers\n\
        \x20                    the whole window, whatever this is set to.\n\
        \x20 --sort KEY         {}\n\
        \x20                    (default: recent — git's own merge order)\n\
        \x20 --json             machine form: the same figures, with an explicit null\n\
        \x20                    wherever nothing was measured\n\
        \x20 --usd-per-mtok R   price the input-equivalent tokens at R dollars per\n\
        \x20                    million. No rate, no dollars — this device holds no\n\
        \x20                    price table, and a stale one invents money.\n\
        \n\
        Local only: this repo's git history and the tool session logs already on this\n\
        machine. No network, no server, no forge token. Spend that could not be placed\n\
        against a PR is reported, never hidden.\n",
        Sort::NAMES.join(" | ")
    )
}

// ── the view the renderers consume (pure, no git, no clock) ─────────────

/// One merged PR's row. Everything already resolved — the renderers do
/// arithmetic and formatting only.
///
/// The three change primitives are `Option` for one reason: git could not read
/// that merge's diff. `None` renders as `—` everywhere and is excluded from
/// every sum, because a zero here would enter the rollup as a PR that changed
/// nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pr_number: u64,
    pub ai_assisted: bool,
    pub merged_at: String,
    pub files_changed: Option<u32>,
    pub lines_added: Option<u64>,
    pub lines_deleted: Option<u64>,
    /// The raw per-class counts, never collapsed: a reader must be able to see
    /// what the equivalent was derived from.
    pub mix: TokenMix,
    /// `mix` weighted into input-equivalents — the comparable figure, and the
    /// one the table and any dollars are built on.
    pub equiv_tokens: f64,
    pub sessions: u32,
    /// Union of five-minute activity windows across the contributing sessions,
    /// apportioned by the same weights that split the tokens.
    pub active_ms: u64,
    /// How firm this row's token figure is: high when the session's changed
    /// files overlapped the PR's, low when all that linked them was a mention
    /// of the number. [`is_weak`] reads it; the table marks it.
    pub attribution_confidence: f64,
}

impl Row {
    /// `added + deleted`, or `None` when the diff was unreadable.
    pub fn churn(&self) -> Option<u64> {
        Some(self.lines_added?.saturating_add(self.lines_deleted?))
    }
}

/// Everything `roi` learned, in the shape both renderers read.
#[derive(Debug, Clone)]
pub struct RoiView {
    pub slug: String,
    pub days: u32,
    /// EVERY merged PR in the window, already sorted. The rollup covers all of
    /// them; `limit` decides only how many the table prints. Keeping one
    /// population is what stops the header and the rollup describing different
    /// sets of PRs in the same breath.
    pub rows: Vec<Row>,
    pub limit: usize,
    pub sort: Sort,
    pub unattributed: TokenMix,
    pub unattributed_active_ms: u64,
    pub unattributed_sessions: u32,
    pub sessions_scanned: u32,
    pub usd_per_mtok: Option<f64>,
    /// Whether the token join found any tool logs at all, so "0 tokens" can be
    /// told apart from "nothing to read".
    pub spend_available: bool,
}

/// Column sums over one set of rows. Nothing here is weighted, scaled or
/// blended: each field is the sum, the count or the mean of a measured column,
/// and a reader can reproduce every one of them from the table above it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupTotals {
    pub prs: usize,
    /// How many of `prs` had a readable diff. Below `prs` the change primitives
    /// cover only part of the group, and the rollup says so.
    pub diffs_read: usize,
    /// `None` when NO row in the group had a readable diff — never `0`.
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_deleted: Option<u64>,
    /// Raw classes, summed. Kept whole so the rollup can show the reader what
    /// the equivalent came from rather than asking them to trust it.
    pub mix: TokenMix,
    pub equiv_tokens: f64,
    pub sessions: u32,
    pub active_ms: u64,
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
    /// Only ever `Some` when the user supplied a rate.
    pub usd: Option<f64>,
}

/// The rollup: the same columns, summed three ways.
///
/// AI and human are reported SIDE BY SIDE rather than as one number with the
/// other filtered out. Which of the two matters is the reader's call, and
/// showing only one of them would be the first half of a verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct Totals {
    pub ai: GroupTotals,
    pub human: GroupTotals,
    pub all: GroupTotals,
}

impl RoiView {
    /// The prefix `--limit` prints. Ordering happened before the cut, so this
    /// is the top of whatever column `--sort` named.
    pub fn shown(&self) -> &[Row] {
        &self.rows[..self.limit.min(self.rows.len())]
    }

    pub fn totals(&self) -> Totals {
        let ai: Vec<&Row> = self.rows.iter().filter(|r| r.ai_assisted).collect();
        let human: Vec<&Row> = self.rows.iter().filter(|r| !r.ai_assisted).collect();
        let all: Vec<&Row> = self.rows.iter().collect();
        Totals {
            ai: group_totals(&ai, self.usd_per_mtok),
            human: group_totals(&human, self.usd_per_mtok),
            all: group_totals(&all, self.usd_per_mtok),
        }
    }
}

// ── pure arithmetic ─────────────────────────────────────────────────────

/// Sum one group's columns. Pure, and the only place a total is computed.
pub fn group_totals(rows: &[&Row], usd_per_mtok: Option<f64>) -> GroupTotals {
    let mix = rows.iter().fold(TokenMix::default(), |mut m, r| {
        m.input = m.input.saturating_add(r.mix.input);
        m.output = m.output.saturating_add(r.mix.output);
        m.cache_creation = m.cache_creation.saturating_add(r.mix.cache_creation);
        m.cache_read = m.cache_read.saturating_add(r.mix.cache_read);
        m.reasoning = m.reasoning.saturating_add(r.mix.reasoning);
        m
    });
    // Summed off the rows, not recomputed from `mix`, so the rollup is exactly
    // the column above it.
    let equiv = unsign_zero(rows.iter().map(|r| r.equiv_tokens).sum());
    let read: Vec<&&Row> = rows.iter().filter(|r| r.files_changed.is_some()).collect();
    // `None` rather than `0` when nothing was readable: an unread diff is not
    // an empty one, and summing over an empty set would say it was.
    let sum = |f: &dyn Fn(&Row) -> u64| {
        (!read.is_empty()).then(|| read.iter().map(|r| f(r)).sum::<u64>())
    };
    GroupTotals {
        prs: rows.len(),
        diffs_read: read.len(),
        files_changed: sum(&|r| u64::from(r.files_changed.unwrap_or(0))),
        lines_added: sum(&|r| r.lines_added.unwrap_or(0)),
        lines_deleted: sum(&|r| r.lines_deleted.unwrap_or(0)),
        mix,
        equiv_tokens: equiv,
        sessions: rows.iter().map(|r| r.sessions).sum(),
        active_ms: rows.iter().map(|r| r.active_ms).sum(),
        confidence: weighted_confidence(rows),
        weak_share: weak_volume_share(rows),
        usd: usd_per_mtok.map(|rate| usd_for_tokens(equiv, rate)),
    }
}

/// `-0.0` → `0.0`, everything else untouched. `-0.0 == 0.0` is true, so this
/// is a display fix, not an arithmetic one: `impl Sum for f64` folds from
/// `-0.0`, so an empty group's equivalent arrives negative.
pub fn unsign_zero(x: f64) -> f64 {
    if x == 0.0 {
        0.0
    } else {
        x
    }
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

/// Order `rows` by one measured column, descending, ties keeping merge order.
///
/// `Recent` is a no-op because the caller hands these over in git's
/// first-parent order already — newest first. An unreadable diff sorts LAST on
/// the columns it has no value for (`Option::None < Some(_)`), so a PR we could
/// not measure never leads a table ordered by size.
pub fn sort_rows(rows: &mut [Row], sort: Sort) {
    match sort {
        Sort::Recent => {}
        Sort::Pr => rows.sort_by_key(|r| std::cmp::Reverse(r.pr_number)),
        Sort::Files => rows.sort_by_key(|r| std::cmp::Reverse(r.files_changed)),
        Sort::Added => rows.sort_by_key(|r| std::cmp::Reverse(r.lines_added)),
        Sort::Deleted => rows.sort_by_key(|r| std::cmp::Reverse(r.lines_deleted)),
        Sort::Lines => rows.sort_by_key(|r| std::cmp::Reverse(r.churn())),
        Sort::Sessions => rows.sort_by_key(|r| std::cmp::Reverse(r.sessions)),
        Sort::Tokens => rows.sort_by(|a, b| b.equiv_tokens.total_cmp(&a.equiv_tokens)),
        Sort::Active => rows.sort_by_key(|r| std::cmp::Reverse(r.active_ms)),
    }
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

/// Active time, at the precision the measurement actually supports: whole
/// minutes. Zero is an em-dash — no time was attributed, which is not the same
/// as no time being spent.
pub fn fmt_ms(ms: u64) -> String {
    match ms {
        0 => "—".to_string(),
        ms if ms >= 3_600_000 => format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000),
        ms if ms >= 60_000 => format!("{}m", ms / 60_000),
        _ => "<1m".to_string(),
    }
}

/// A count git could not read renders as `—`, never as `0`.
fn fmt_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "—".to_string(), |v| v.to_string())
}

/// Sessions in a table cell. Zero is an em-dash, matching the token and time
/// cells beside it: nothing joined to this PR. Printing `0` there would read as
/// "no AI touched this", which is a claim, not a count — and on a device with
/// no tool logs at all it would be a false one.
fn fmt_sessions(n: u32) -> String {
    fmt_opt((n > 0).then_some(n))
}

/// `+340/−85`, exact. Not abbreviated: lines are the primitive, and `+1k`
/// throws away the digits somebody would go and check.
fn fmt_lines(added: Option<u64>, deleted: Option<u64>) -> String {
    match (added, deleted) {
        (Some(a), Some(d)) => format!("+{a}/−{d}"),
        _ => "—".to_string(),
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
const LABEL_W: usize = 20;

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

/// One authorship group as the same columns the table prints, summed.
///
/// Every segment is a sum of a column above it — there is nothing here a reader
/// cannot re-add by hand, which is the whole point.
fn fmt_group(g: &GroupTotals) -> String {
    let mut s = format!(
        "{} PR{} · {} files · {} · {} session{} · {} · {}",
        g.prs,
        if g.prs == 1 { "" } else { "s" },
        fmt_opt(g.files_changed),
        fmt_lines(g.lines_added, g.lines_deleted),
        g.sessions,
        if g.sessions == 1 { "" } else { "s" },
        fmt_equiv(g.equiv_tokens),
        fmt_ms(g.active_ms),
    );
    // Dollars only ever from a rate the user typed, and never against nothing:
    // `$0.00` under an em-dash reads as "this work was free", when it means
    // nothing was attributed to it.
    if let Some(usd) = g.usd.filter(|_| g.equiv_tokens > 0.0) {
        s.push_str(&format!(" · {}", fmt_usd(usd)));
    }
    // Partial coverage is stated, not averaged away: a change total over three
    // of five PRs is a different quantity from one over all five.
    if g.diffs_read < g.prs {
        s.push_str(&format!(
            "   ({} of {} diffs readable)",
            g.diffs_read, g.prs
        ));
    }
    s
}

// ── the human form ──────────────────────────────────────────────────────

/// Render the whole human output. Pure: same view in, same string out.
///
/// The one conditional column is `usd`, and it exists only inside
/// `if let Some(rate) = view.usd_per_mtok` — so a row cannot print a number the
/// header did not announce.
pub fn render_human(view: &RoiView) -> String {
    let t = view.totals();
    let mut out = String::new();
    let usd_rate = view.usd_per_mtok;
    let shown = view.shown();

    // Header: the split, and whether the table is the whole window or a slice.
    // The AI/human counts are the WINDOW's — `totals()` runs over every row,
    // and `--limit` never reaches them — so the two halves always sum to the
    // total this same sentence announces.
    let cut = if shown.len() < view.rows.len() {
        format!(
            ", showing {} of {} by {}",
            shown.len(),
            view.rows.len(),
            view.sort.name()
        )
    } else {
        String::new()
    };
    out.push_str(&format!(
        "{} — {} AI-assisted PRs, {} human-authored, {}d{cut}\n",
        view.slug, t.ai.prs, t.human.prs, view.days,
    ));

    if shown.is_empty() {
        out.push_str("\n  (no merged PRs in this window)\n");
    } else {
        // The unnamed one-char column after `tokens` is the match-strength
        // mark — a header for it would be wider than the mark. Token classes
        // stay off the table: four more columns would turn one row per PR into
        // a spreadsheet, and the classes live in the rollup, where the sum is
        // what a reader audits.
        let mut head = format!(
            "\n  {:>6}  {:<5} {:>6} {:>14} {:>8} {:>9}  {:>8}",
            "PR", "who", "files", "lines", "sessions", "tokens", "active"
        );
        if usd_rate.is_some() {
            head.push_str(&format!(" {:>9}", "usd"));
        }
        out.push_str(head.trim_end());
        out.push('\n');

        for r in shown {
            let mut line = format!(
                "  {:>6}  {:<5} {:>6} {:>14} {:>8} {:>9}{} {:>8}",
                format!("#{}", r.pr_number),
                if r.ai_assisted { "AI" } else { "human" },
                fmt_opt(r.files_changed),
                fmt_lines(r.lines_added, r.lines_deleted),
                fmt_sessions(r.sessions),
                fmt_equiv(r.equiv_tokens),
                // Abuts the number so the mark reads as belonging to it, and
                // the digits stay column-aligned whether marked or not.
                if is_weak(r.equiv_tokens, r.attribution_confidence) {
                    '~'
                } else {
                    ' '
                },
                fmt_ms(r.active_ms),
            );
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
            out.push_str(line.trim_end());
            out.push('\n');
        }
        // One legend line, and only where there is a token figure to qualify:
        // with nothing attributed anywhere, the mark never appears and
        // explaining it is noise.
        if shown.iter().any(|r| r.equiv_tokens > 0.0) {
            out.push_str(&format!(
                "  ~ = weak attribution (confidence ≤ {WEAK_CONFIDENCE:.2}): mention-only, with \
                 little or no changed-file overlap behind it\n"
            ));
        }
    }

    // Rollup. Every line below covers the WHOLE window, never the printed
    // slice — one population, whatever `--limit` says.
    out.push('\n');
    kv(&mut out, "AI-assisted:", &fmt_group(&t.ai));
    kv(&mut out, "human-authored:", &fmt_group(&t.human));
    // Directly beneath the equivalents, so the derived figure and the measured
    // classes it came from are never far apart.
    kv(&mut out, "raw mix (all):", &fmt_raw_mix(&t.all.mix));
    // The join is inferred, so say how firm it is — and how much of the volume
    // rests on the weak kind — before anyone quotes the numbers above.
    if let Some(c) = t.all.confidence {
        let weak = match t.all.weak_share {
            // `w` is a ratio of f64 sums, so a share that is zero to the printed
            // precision can arrive as a tiny negative from rounding — clamp before
            // formatting so it never renders as `-0%`.
            Some(w) => format!(
                " — {:.0}% of volume from weak (mention-only) matches",
                (w * 100.0).max(0.0)
            ),
            None => String::new(),
        };
        kv(
            &mut out,
            "attribution:",
            &format!("{c:.2} mean confidence{weak}"),
        );
        // Past half, the headline spend figure is mostly a guess about which
        // sessions produced these PRs, and a reader who quotes it should know
        // that before they do.
        if t.all.weak_share.is_some_and(|w| w > 0.5) {
            kv(
                &mut out,
                "caution:",
                "most of this spend is inferred from PR mentions, not file overlap \
                 — treat the per-PR token and time figures as indicative",
            );
        }
    }
    // Device-wide, NOT this repo: the session scan reads every tool log on the
    // machine. Labelling it as repo-scoped would understate the leftovers by
    // however many repos the person also works in. Tokens AND time, because
    // spend nobody can place is exactly the figure a reader must not have to
    // reconstruct.
    if view.spend_available {
        let mut un = format!(
            "{} · {} across {} of {} sessions (device-wide), raw {}",
            fmt_equiv(view.unattributed.equiv_tokens()),
            fmt_ms(view.unattributed_active_ms),
            view.unattributed_sessions,
            view.sessions_scanned,
            fmt_count(view.unattributed.raw_total()),
        );
        if let Some(rate) = usd_rate.filter(|_| view.unattributed.equiv_tokens() > 0.0) {
            un.push_str(&format!(
                " · {}",
                fmt_usd(usd_for_tokens(view.unattributed.equiv_tokens(), rate))
            ));
        }
        kv(&mut out, "unattributed:", &un);
    } else {
        kv(
            &mut out,
            "unattributed:",
            "— (no tool session logs found on this device)",
        );
    }

    // Why the two token figures differ, stated once, from the constants
    // themselves so the prose cannot drift from the arithmetic.
    out.push_str(&format!(
        "  note: cache reads bill at roughly a tenth of fresh input \
         (cache-write {W_CACHE_WRITE}×, cache-read {W_CACHE_READ}×, output {W_OUTPUT}×); \
         weighting them is what makes token counts comparable across session lengths.\n"
    ));
    // The refusal, said out loud rather than left as an absence somebody fills
    // in with an assumption.
    out.push_str(
        "  note: these are measured quantities, not a score — tokens and time sit beside \
         what shipped and are never combined into one number.\n",
    );
    out
}

// ── the machine form ────────────────────────────────────────────────────

/// The `--json` document. `usd` and the change primitives are always PRESENT
/// and `null` when unavailable, so a consumer must see the absence rather than
/// infer it from a missing key (which reads as an older schema).
pub fn render_json(view: &RoiView) -> Value {
    let t = view.totals();
    let prs: Vec<Value> = view
        .shown()
        .iter()
        .map(|r| {
            json!({
                "pr_number": r.pr_number,
                "ai_assisted": r.ai_assisted,
                "merged_at": r.merged_at,
                // Null, never 0: git could not read that merge's diff.
                "files_changed": r.files_changed,
                "lines_added": r.lines_added,
                "lines_deleted": r.lines_deleted,
                // Measured classes and the derived figure, always together: a
                // consumer can re-weight the mix itself, and can see that the
                // equivalent is a normalisation rather than a new measurement.
                "mix": r.mix,
                "raw_total": r.mix.raw_total(),
                "equiv_tokens": r.equiv_tokens,
                "session_count": r.sessions,
                "active_ms": r.active_ms,
                "attribution_confidence": r.attribution_confidence,
                "usd": view.usd_per_mtok.map(|rate| usd_for_tokens(r.equiv_tokens, rate)),
            })
        })
        .collect();

    json!({
        "repo": view.slug,
        "days": view.days,
        "sort": view.sort.name(),
        // The WINDOW, before `--limit`: `prs` above is a slice, and a consumer
        // that counted the split off it would describe a different population
        // from the one `totals` describes.
        "window": {
            "merged_prs": view.rows.len(),
            "ai_prs": t.ai.prs,
            "human_prs": t.human.prs,
            "shown": view.shown().len(),
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
        "prs": prs,
        // Three sums of the same columns. There is no fourth key blending them.
        "totals": {
            "ai": group_json(&t.ai),
            "human": group_json(&t.human),
            "all": group_json(&t.all),
            // Device-wide, not repo-scoped: the session scan reads every tool
            // log on this machine, across every repo.
            "unattributed_device": {
                "mix": view.unattributed,
                "raw_total": view.unattributed.raw_total(),
                "equiv_tokens": view.unattributed.equiv_tokens(),
                "active_ms": view.unattributed_active_ms,
            },
            "unattributed_sessions_device": view.unattributed_sessions,
            "sessions_scanned_device": view.sessions_scanned,
        },
    })
}

fn group_json(g: &GroupTotals) -> Value {
    json!({
        "prs": g.prs,
        // How much of the group the change primitives actually cover. A
        // consumer that divides by `prs` without reading this gets a per-PR
        // average over a denominator the numerator does not span.
        "diffs_read": g.diffs_read,
        "files_changed": g.files_changed,
        "lines_added": g.lines_added,
        "lines_deleted": g.lines_deleted,
        "mix": g.mix,
        "raw_total": g.mix.raw_total(),
        "equiv_tokens": g.equiv_tokens,
        "session_count": g.sessions,
        "active_ms": g.active_ms,
        "attribution_confidence": g.confidence,
        // What share of `equiv_tokens` rests on a mention-only match. The mean
        // alone cannot say: it averages the guess away.
        "attribution_weak_volume_share": g.weak_share,
        "usd": g.usd,
    })
}

// ── the impure half: git and the token join ─────────────────────────────

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

/// One merged PR as git names it, before anything is read off its diff.
struct MergedPr {
    pr_number: u64,
    merge_sha: String,
    merged_at: String,
}

/// Every merged PR the first-parent walk reaches, newest first, one row per PR
/// number. Empty when the repo cannot be read.
///
/// Reuses the parsers' own pure readers ([`parse_git_log`],
/// [`select_anchor_commits`]) so this command and the anchor miner agree on
/// which commits are merges and which PR each names.
fn merged_prs(repo: &str) -> Vec<MergedPr> {
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

pub fn cmd_roi(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }
    let opts = match parse_roi_opts(args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    let Some(repo) = resolve_repo_root(Some(&opts.repo)) else {
        eprintln!(
            "modelstat roi: `{}` is not inside a git repository",
            opts.repo
        );
        return ExitCode::FAILURE;
    };

    // The AI/human classification, and the slug spend is keyed on. `None` means
    // no remote slug (nothing to key spend on) or git failed outright.
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

    // Tokens and time, joined to PRs by the sibling crate. Best-effort by
    // contract: an empty summary is what "no tool logs on this device" looks
    // like.
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
    // Every PR in the window gets a row, and its diff is read whether or not
    // `--limit` will print it: the rollup describes the window, and `--sort`
    // cannot order by a column it has not measured. A PR whose diff will not
    // read keeps its row with `None` primitives — dropping it would delete a
    // merge that demonstrably happened.
    let mut rows: Vec<Row> = merged_prs(&repo)
        .iter()
        .filter(|p| within_days(&p.merged_at, opts.days, now))
        .map(|p| {
            let d = diff_features(&repo, &p.merge_sha);
            let spend = by_pr.get(&p.pr_number);
            Row {
                pr_number: p.pr_number,
                ai_assisted: !human.contains(&p.pr_number),
                merged_at: p.merged_at.clone(),
                files_changed: d.as_ref().map(|d| d.files_changed),
                lines_added: d.as_ref().map(|d| d.lines_added),
                lines_deleted: d.as_ref().map(|d| d.lines_deleted),
                mix: spend.map_or_else(TokenMix::default, |s| s.mix),
                equiv_tokens: spend.map_or(0.0, |s| s.equiv_tokens),
                sessions: spend.map_or(0, |s| s.session_count),
                active_ms: spend.map_or(0, |s| s.active_ms),
                attribution_confidence: spend.map_or(0.0, |s| s.attribution_confidence),
            }
        })
        .collect();
    sort_rows(&mut rows, opts.sort);

    let view = RoiView {
        slug: mined.slug.clone(),
        days: opts.days,
        rows,
        limit: opts.limit,
        sort: opts.sort,
        unattributed: spend.unattributed,
        unattributed_active_ms: spend.unattributed_active_ms,
        unattributed_sessions: spend.unattributed_sessions,
        sessions_scanned: spend.sessions_scanned,
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

    fn row(pr: u64, ai: bool, tokens: u64) -> Row {
        let mix = mix_of(tokens);
        Row {
            pr_number: pr,
            ai_assisted: ai,
            merged_at: "2026-08-01T00:00:00Z".into(),
            files_changed: Some(4),
            lines_added: Some(120),
            lines_deleted: Some(30),
            equiv_tokens: mix.equiv_tokens(),
            mix,
            sessions: 2,
            active_ms: 45 * 60_000,
            attribution_confidence: 0.9,
        }
    }

    /// A row whose diff git could not read — every change primitive absent.
    fn unread(mut r: Row) -> Row {
        r.files_changed = None;
        r.lines_added = None;
        r.lines_deleted = None;
        r
    }

    fn view(rows: Vec<Row>) -> RoiView {
        RoiView {
            slug: "org/repo".into(),
            days: 30,
            limit: 20,
            sort: Sort::Recent,
            rows,
            unattributed: mix_of(2_000_000),
            unattributed_active_ms: 90 * 60_000,
            unattributed_sessions: 4,
            sessions_scanned: 40,
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

    // ── the doctrine, read off the rendered text ────────────────────

    /// The whole point of the reshape: no blended number, in either form, ever.
    #[test]
    fn nothing_in_the_output_is_a_score() {
        let mut v = view(vec![row(1, true, 4_000_000), row(2, false, 1_000_000)]);
        v.usd_per_mtok = Some(2.5);
        let rendered = render_human(&v);
        // Everything except the trailing `note:` lines, which are allowed to
        // name the thing they are refusing to print.
        let body = rendered
            .lines()
            .filter(|l| !l.trim_start().starts_with("note:"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        let doc = render_json(&v).to_string().to_lowercase();
        for banned in [
            "unit",
            "percentile",
            "hours",
            "label",
            "calibrat",
            "score",
            "loocv",
            "judge",
            "effort",
        ] {
            assert!(!body.contains(banned), "`{banned}` survived:\n{body}");
            assert!(!doc.contains(banned), "`{banned}` survived:\n{doc}");
        }
        // And the refusal is stated, not merely enacted.
        assert!(rendered.contains("not a score"), "{rendered}");
    }

    #[test]
    fn help_says_what_is_measured_and_refuses_to_score() {
        let h = help_text();
        assert!(h.contains("does not score anyone"), "{h}");
        assert!(
            h.contains("no\ncomposite") || h.contains("no composite"),
            "{h}"
        );
        assert!(h.contains("cannot audit"), "{h}");
        // Every sort key the parser accepts is documented, and vice versa.
        for key in Sort::NAMES {
            assert!(h.contains(key), "undocumented --sort {key}:\n{h}");
            assert_eq!(Sort::parse(key).unwrap().name(), key);
        }
        assert!(h.contains("--usd-per-mtok"), "{h}");
        assert!(h.contains("No rate, no dollars"), "{h}");
    }

    // ── the primitives ──────────────────────────────────────────────

    #[test]
    fn every_primitive_reaches_the_row_and_the_rollup() {
        let text = render_human(&view(vec![row(412, true, 4_000_000)]));
        let pr = line(&text, "#412");
        assert!(pr.contains("AI"), "{pr}");
        assert!(pr.contains(" 4 "), "files missing:\n{pr}");
        assert!(pr.contains("+120/−30"), "lines missing:\n{pr}");
        assert!(pr.contains("806k eq"), "{pr}");
        assert!(pr.contains("45m"), "active time missing:\n{pr}");
        assert!(!pr.contains("4.0M"), "raw sum in the table:\n{text}");

        let head = line(&text, "sessions");
        for col in [
            "PR", "who", "files", "lines", "sessions", "tokens", "active",
        ] {
            assert!(head.contains(col), "no `{col}` column:\n{head}");
        }
        assert!(!head.contains("cache"), "class columns leaked:\n{head}");

        // The rollup is the same columns, summed.
        let ai = line(&text, "AI-assisted:");
        assert!(ai.contains("1 PR ·"), "{ai}");
        assert!(ai.contains("4 files"), "{ai}");
        assert!(ai.contains("+120/−30"), "{ai}");
        assert!(ai.contains("2 sessions"), "{ai}");
        assert!(ai.contains("806k eq"), "{ai}");
        assert!(ai.contains("45m"), "{ai}");
        // Both groups always, side by side — never one with the other hidden.
        assert!(line(&text, "human-authored:").contains("0 PRs"), "{text}");
    }

    #[test]
    fn an_unreadable_diff_is_a_dash_never_a_zero() {
        let v = view(vec![
            unread(row(7, true, 1_000_000)),
            row(8, true, 1_000_000),
        ]);
        let text = render_human(&v);
        let pr = line(&text, "#7");
        assert!(pr.contains('—'), "unread diff rendered as a number:\n{pr}");
        assert!(!pr.contains(" 0 "), "{pr}");
        // The sum covers only what was readable, and says so.
        let ai = line(&text, "AI-assisted:");
        assert!(ai.contains("4 files"), "summed an unread diff:\n{ai}");
        assert!(ai.contains("(1 of 2 diffs readable)"), "{ai}");

        // Nothing readable at all ⇒ null, not zero, in both forms.
        let none = view(vec![unread(row(7, true, 0))]);
        let t = none.totals();
        assert_eq!(t.ai.files_changed, None);
        assert_eq!(t.ai.lines_added, None);
        assert!(render_json(&none)["prs"][0]["files_changed"].is_null());
        assert!(render_json(&none)["totals"]["ai"]["lines_deleted"].is_null());
        assert!(line(&render_human(&none), "AI-assisted:").contains("— files"));
    }

    #[test]
    fn totals_split_ai_from_human_and_report_both() {
        let v = view(vec![
            row(1, true, 1_000_000),
            row(2, true, 3_000_000),
            row(3, false, 9_000_000),
        ]);
        let t = v.totals();
        assert_eq!((t.ai.prs, t.human.prs, t.all.prs), (2, 1, 3));
        assert_eq!(t.ai.files_changed, Some(8));
        assert_eq!(t.ai.lines_added, Some(240));
        assert_eq!(t.human.lines_deleted, Some(30));
        assert_eq!(t.ai.active_ms, 90 * 60_000);
        // Every raw class survives the rollup, undiminished...
        assert_eq!(t.ai.mix.raw_total(), 4_000_000);
        assert_eq!(t.ai.mix.cache_read, 3_692_000);
        // ...beside the equivalent, which is ~5× smaller.
        assert!(close(t.ai.equiv_tokens, 4.0 * EQUIV_PER_RAW_M), "{t:?}");
        assert!(close(t.all.equiv_tokens, 13.0 * EQUIV_PER_RAW_M), "{t:?}");
        // No rate supplied ⇒ no money anywhere in the rollup.
        assert_eq!(t.ai.usd, None);
    }

    #[test]
    fn active_time_renders_at_the_precision_it_was_measured() {
        assert_eq!(fmt_ms(0), "—");
        assert_eq!(fmt_ms(30_000), "<1m");
        assert_eq!(fmt_ms(45 * 60_000), "45m");
        assert_eq!(fmt_ms(2 * 3_600_000 + 14 * 60_000), "2h 14m");
        assert_eq!(fmt_ms(3_600_000), "1h 0m");
    }

    // ── sorting is the reader's, and it happens before the cut ──────

    #[test]
    fn sort_orders_by_the_named_column_and_limit_cuts_afterwards() {
        let mut small = row(1, true, 1_000_000);
        small.files_changed = Some(1);
        small.lines_added = Some(10);
        small.active_ms = 60_000;
        small.sessions = 1;
        let mut big = row(2, true, 9_000_000);
        big.files_changed = Some(90);
        big.lines_added = Some(900);
        big.active_ms = 9 * 3_600_000;
        big.sessions = 9;

        // `recent` is git's order, untouched — the default asserts nothing.
        let mut rows = vec![small.clone(), big.clone()];
        sort_rows(&mut rows, Sort::Recent);
        assert_eq!(rows[0].pr_number, 1);

        for (key, want) in [
            (Sort::Files, 2),
            (Sort::Added, 2),
            (Sort::Lines, 2),
            (Sort::Sessions, 2),
            (Sort::Tokens, 2),
            (Sort::Active, 2),
            (Sort::Pr, 2),
        ] {
            let mut rows = vec![small.clone(), big.clone()];
            sort_rows(&mut rows, key);
            assert_eq!(rows[0].pr_number, want, "{key:?} ordered wrong");
        }

        // A PR whose diff would not read never leads a size-ordered table.
        let mut rows = vec![unread(small.clone()), big.clone()];
        sort_rows(&mut rows, Sort::Files);
        assert_eq!(rows[0].pr_number, 2);
        assert_eq!(rows[1].files_changed, None);

        // The cut happens after the sort, so `--limit 1 --sort tokens` shows
        // the biggest row, not the first one git happened to list.
        let mut rows = vec![small, big];
        sort_rows(&mut rows, Sort::Tokens);
        let mut v = view(rows);
        v.limit = 1;
        v.sort = Sort::Tokens;
        let text = render_human(&v);
        assert!(text.contains("#2"), "{text}");
        assert!(!text.lines().any(|l| l.starts_with("  #1")), "{text}");
        assert!(text.contains("showing 1 of 2 by tokens"), "{text}");
    }

    #[test]
    fn the_rollup_covers_the_window_however_small_the_limit() {
        let rows: Vec<Row> = (0..9)
            .map(|i| row(100 + i, i % 2 == 0, 1_000_000))
            .collect();
        let header_at = |limit: usize| {
            let mut v = view(rows.clone());
            v.limit = limit;
            let text = render_human(&v);
            (
                text.lines().next().unwrap().to_string(),
                line(&text, "AI-assisted:").to_string(),
            )
        };
        let (six_head, six_ai) = header_at(6);
        let (all_head, all_ai) = header_at(100);

        assert!(
            six_head.contains("5 AI-assisted PRs, 4 human-authored"),
            "{six_head}"
        );
        // `--limit` moves `showing` and nothing else — not the header split,
        // not the rollup. One population, whatever is printed.
        assert_eq!(
            six_head.split_once(", 30d").unwrap().0,
            all_head.split_once(", 30d").unwrap().0
        );
        assert_eq!(
            six_ai, all_ai,
            "--limit reached the rollup:\n{six_ai}\n{all_ai}"
        );
        assert!(six_head.contains("showing 6 of 9"), "{six_head}");
        assert!(!all_head.contains("showing"), "{all_head}");
    }

    // ── how firm each row's tokens are ──────────────────────────────

    #[test]
    fn confidence_is_weighted_by_tokens_not_by_row() {
        let mut big = row(1, true, 9_000_000);
        big.attribution_confidence = 0.3;
        let mut small = row(2, true, 1_000_000);
        small.attribution_confidence = 0.8;
        // Row-mean would be 0.55; the 9M-token row is what the total rests on.
        let t = view(vec![big, small]).totals();
        assert!((t.all.confidence.unwrap() - 0.35).abs() < 1e-9, "{t:?}");

        // Nothing attributed ⇒ nothing to be confident about.
        assert_eq!(view(vec![row(1, true, 0)]).totals().all.confidence, None);
        assert_eq!(weighted_confidence(&[]), None);
    }

    #[test]
    fn a_weak_row_is_marked_and_one_legend_line_says_what_the_mark_means() {
        let mut weak = row(41, true, 4_000_000);
        weak.attribution_confidence = 0.2;
        let text = render_human(&view(vec![weak, row(42, true, 4_000_000)]));
        assert!(
            line(&text, "#41").contains("806k eq~"),
            "unmarked weak row:\n{text}"
        );
        assert!(
            !line(&text, "#42").contains('~'),
            "marked a strong row:\n{text}"
        );
        // Exactly one legend line, and it names the threshold from the constant.
        let legend: Vec<&str> = text
            .lines()
            .filter(|l| l.trim_start().starts_with("~ ="))
            .collect();
        assert_eq!(legend.len(), 1, "{text}");
        assert!(legend[0].contains("confidence ≤ 0.30"), "{}", legend[0]);

        // The threshold is inclusive: 0.30 is the mention-only score.
        let mut edge = row(43, true, 4_000_000);
        edge.attribution_confidence = WEAK_CONFIDENCE;
        assert!(line(&render_human(&view(vec![edge])), "#43").contains("806k eq~"));

        // Nothing attributed is not a weak match — and with no token figure
        // anywhere, the mark never appears, so the legend stays away.
        let none = render_human(&view(vec![row(44, true, 0)]));
        assert!(!line(&none, "#44").contains('~'), "{none}");
        assert!(
            !none.contains("~ ="),
            "legend with nothing to qualify:\n{none}"
        );
        assert!(!is_weak(0.0, 0.0), "no tokens is not a weak attribution");
    }

    #[test]
    fn the_rollup_names_the_weak_share_and_cautions_only_past_half() {
        let mut weak = row(1, true, 1_000_000);
        weak.attribution_confidence = 0.2;
        // 1M raw weak against 4M raw strong, one shape ⇒ 20% of the volume.
        let text = render_human(&view(vec![weak.clone(), row(2, true, 4_000_000)]));
        let att = line(&text, "attribution:");
        assert!(att.contains("0.76 mean confidence"), "{att}");
        assert!(
            att.contains("20% of volume from weak (mention-only) matches"),
            "{att}"
        );
        assert!(!text.contains("caution:"), "cautioned under half:\n{text}");

        // One big guess beside a small certainty: 90% of the volume is weak.
        let mut big_weak = row(3, true, 9_000_000);
        big_weak.attribution_confidence = 0.1;
        let text = render_human(&view(vec![row(2, true, 1_000_000), big_weak]));
        assert!(
            line(&text, "attribution:").contains("90% of volume"),
            "{text}"
        );
        let caution = line(&text, "caution:");
        assert!(caution.contains("inferred from PR mentions"), "{caution}");
        assert!(caution.contains("token and time figures"), "{caution}");
        assert_eq!(text.matches("caution:").count(), 1, "{text}");

        // By VOLUME, not by row — and never a share of nothing.
        assert_eq!(view(vec![weak]).totals().all.weak_share, Some(1.0));
        assert_eq!(view(vec![row(9, true, 0)]).totals().all.weak_share, None);
        assert_eq!(weak_volume_share(&[]), None);
        assert!(!render_human(&view(Vec::new())).contains("caution:"));
    }

    // ── unattributed spend, tokens AND time ─────────────────────────

    #[test]
    fn unattributed_reports_tokens_and_time_and_is_device_wide() {
        let text = render_human(&view(vec![row(1, true, 4_000_000)]));
        let un = line(&text, "unattributed:");
        assert!(un.contains("403k eq"), "{un}");
        assert!(un.contains("1h 30m"), "no unattributed time:\n{un}");
        assert!(un.contains("4 of 40 sessions"), "{un}");
        assert!(un.contains("(device-wide)"), "{un}");
        assert!(un.contains("raw 2.0M"), "{un}");

        // No logs at all is said outright, never rendered as a clean zero.
        let mut none = view(vec![row(1, true, 0)]);
        none.spend_available = false;
        let text = render_human(&none);
        let un = line(&text, "unattributed:");
        assert!(un.contains("no tool session logs"), "{un}");
    }

    // ── dollars, only ever from an explicit rate ────────────────────

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
    fn no_rate_means_no_dollar_figure_anywhere() {
        let text = render_human(&view(vec![row(1, true, 4_000_000)]));
        assert!(!text.contains('$'), "money without a rate:\n{text}");
        assert!(!text.contains("usd"), "{text}");
        assert!(render_json(&view(vec![row(1, true, 4_000_000)]))["usd_per_mtok"].is_null());
    }

    #[test]
    fn a_rate_turns_on_dollars_and_prices_only_the_equivalent() {
        let mut v = view(vec![row(1, true, 4_000_000), row(2, false, 4_000_000)]);
        v.usd_per_mtok = Some(2.5);
        let text = render_human(&v);
        // 806_200 eq at $2.50/Mtok. The raw 4.0M would have said $10.00.
        assert!(text.contains("$2.02"), "806k eq at $2.50/Mtok:\n{text}");
        assert!(!text.contains("$10.00"), "priced the raw total:\n{text}");
        // Both groups priced, and the unplaced spend too — a dollar figure that
        // omits the leftovers understates the bill.
        assert!(line(&text, "AI-assisted:").contains("$2.02"), "{text}");
        assert!(line(&text, "human-authored:").contains("$2.02"), "{text}");
        assert!(line(&text, "unattributed:").contains("$1.01"), "{text}");
    }

    #[test]
    fn a_rate_with_nothing_attributed_refuses_to_call_the_work_free() {
        let mut v = view(vec![row(1, true, 0)]);
        v.usd_per_mtok = Some(3.0);
        let text = render_human(&v);
        assert!(!text.contains("$0.00"), "priced nothing as free:\n{text}");
        // The per-row cell agrees with the rollup line above it.
        assert!(
            !text.lines().any(|l| l.contains("#1") && l.contains('$')),
            "{text}"
        );
        assert!(!line(&text, "AI-assisted:").contains('$'), "{text}");
    }

    // ── the equivalent, and the raw mix beside it ───────────────────

    #[test]
    fn the_rollup_shows_the_raw_mix_the_equivalent_came_from() {
        let text = render_human(&view(vec![row(1, true, 4_000_000)]));
        let raw = line(&text, "raw mix (all):");
        assert!(raw.contains("32k fresh"), "{raw}");
        assert!(raw.contains("260k cache-write"), "{raw}");
        assert!(raw.contains("3.7M cache-read"), "{raw}");
        assert!(raw.contains("16k out"), "{raw}");
        assert!(raw.contains("(raw total 4.0M)"), "{raw}");
        // And one line says why the equivalent and the raw total differ.
        assert!(
            text.contains("cache reads bill at roughly a tenth"),
            "{text}"
        );
        assert!(text.contains("cache-read 0.1×"), "{text}");
    }

    #[test]
    fn empty_window_still_renders_a_rollup_and_invents_nothing() {
        let text = render_human(&view(Vec::new()));
        assert!(text.contains("no merged PRs in this window"), "{text}");
        assert!(
            text.contains("0 AI-assisted PRs, 0 human-authored"),
            "{text}"
        );
        assert!(!text.contains('$'), "{text}");
        // The mix line still prints, as zeros: an em-dash inside a sum is not
        // a number. An empty f64 sum folds from -0.0, which is nonsense to show.
        assert!(
            line(&text, "raw mix (all):").contains("(raw total 0)"),
            "{text}"
        );
        assert!(!text.contains("-0.0"), "{text}");
        assert_eq!(view(Vec::new()).totals().ai.equiv_tokens.to_string(), "0");
    }

    // ── the machine form ────────────────────────────────────────────

    #[test]
    fn json_carries_the_primitives_the_split_and_explicit_nulls() {
        let mut weak = row(1, true, 1_000_000);
        weak.attribution_confidence = 0.2;
        let mut v = view(vec![weak, row(2, false, 4_000_000)]);
        v.limit = 1;
        v.sort = Sort::Tokens;
        let doc = render_json(&v);

        assert_eq!(doc["sort"], "tokens");
        // The window, not the slice `prs` carries.
        assert_eq!(doc["window"]["merged_prs"], 2);
        assert_eq!(doc["window"]["ai_prs"], 1);
        assert_eq!(doc["window"]["human_prs"], 1);
        assert_eq!(doc["window"]["shown"], 1);

        let pr = &doc["prs"][0];
        assert_eq!(pr["files_changed"], 4);
        assert_eq!(pr["lines_added"], 120);
        assert_eq!(pr["lines_deleted"], 30);
        assert_eq!(pr["session_count"], 2);
        assert_eq!(pr["active_ms"], 45 * 60_000);
        // Every class by name — nothing collapsed, nothing replaced.
        assert_eq!(pr["mix"]["cache_read"], 923_000);
        assert_eq!(pr["raw_total"], 1_000_000);
        assert!(close(pr["equiv_tokens"].as_f64().unwrap(), EQUIV_PER_RAW_M));
        assert!(close(pr["attribution_confidence"].as_f64().unwrap(), 0.2));
        // Present-and-null, not absent — a consumer must SEE the absence.
        assert!(pr["usd"].is_null());
        assert!(pr.as_object().unwrap().contains_key("usd"));

        // Three sums of the same columns, and nothing blending them.
        let t = &doc["totals"];
        assert_eq!(t["ai"]["prs"], 1);
        assert_eq!(t["human"]["prs"], 1);
        assert_eq!(t["all"]["files_changed"], 8);
        assert_eq!(t["all"]["diffs_read"], 2);
        assert_eq!(t["all"]["active_ms"], 90 * 60_000);
        assert!(close(
            t["all"]["attribution_weak_volume_share"].as_f64().unwrap(),
            0.2
        ));
        assert_eq!(t["human"]["attribution_weak_volume_share"], 0.0);

        // The device-wide leftovers carry the same pair, plus their time.
        let un = &t["unattributed_device"];
        assert_eq!(un["raw_total"], 2_000_000);
        assert_eq!(un["active_ms"], 90 * 60_000);
        assert!(
            close(un["equiv_tokens"].as_f64().unwrap(), 403_100.0),
            "{un}"
        );

        // The weights are published, so the equivalent is re-derivable and a
        // consumer on another provider can see these are Anthropic-family.
        assert_eq!(doc["equiv_weights"]["input"], 1.0);
        assert_eq!(doc["equiv_weights"]["cache_creation"], 1.25);
        assert_eq!(doc["equiv_weights"]["cache_read"], 0.1);
        assert_eq!(doc["equiv_weights"]["output"], 5.0);
        assert!(close(
            doc["weak_confidence_threshold"].as_f64().unwrap(),
            0.3
        ));

        // Nothing attributed ⇒ an explicit null, not a zero share that would
        // read as "none of this is guesswork".
        let empty = render_json(&view(vec![row(1, true, 0)]));
        assert!(empty["totals"]["ai"]["attribution_weak_volume_share"].is_null());
        assert!(empty["totals"]["ai"]
            .as_object()
            .unwrap()
            .contains_key("attribution_weak_volume_share"));

        // Valid JSON, round-trips.
        serde_json::from_str::<Value>(&serde_json::to_string(&doc).unwrap()).unwrap();
    }

    // ── flags ───────────────────────────────────────────────────────

    #[test]
    fn flags_default_and_parse() {
        let none = parse_roi_opts(&[]).unwrap();
        assert_eq!((none.repo.as_str(), none.days, none.limit), (".", 30, 20));
        assert_eq!(none.sort, Sort::Recent);
        assert!(!none.json && none.usd_per_mtok.is_none());

        let a: Vec<String> = [
            "--repo",
            "/tmp/x",
            "--days",
            "7",
            "--limit=3",
            "--json",
            "--sort",
            "active",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = parse_roi_opts(&a).unwrap();
        assert_eq!(
            (o.repo.as_str(), o.days, o.limit, o.json),
            ("/tmp/x", 7, 3, true)
        );
        assert_eq!(o.sort, Sort::Active);
    }

    #[test]
    fn a_bad_flag_value_is_an_error_not_a_silent_default() {
        let a: Vec<String> = vec!["--days".into(), "thirty".into()];
        assert!(parse_roi_opts(&a).unwrap_err().contains("--days"));
        let a: Vec<String> = vec!["--usd-per-mtok".into(), "0".into()];
        assert!(parse_roi_opts(&a).unwrap_err().contains("positive"));
        let a: Vec<String> = vec!["--usd-per-mtok".into(), "-3".into()];
        assert!(parse_roi_opts(&a).is_err());
        // A sort key nobody measures names the ones we do, rather than falling
        // back to an order the user did not ask for.
        let a: Vec<String> = vec!["--sort".into(), "impact".into()];
        let err = parse_roi_opts(&a).unwrap_err();
        assert!(err.contains("impact") && err.contains("tokens"), "{err}");
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
        // Lines are exact — `+1k` throws away the digits somebody checks.
        assert_eq!(fmt_lines(Some(1_204), Some(340)), "+1204/−340");
        assert_eq!(fmt_lines(None, None), "—");
        assert_eq!(fmt_opt(Some(12u32)), "12");
        assert_eq!(fmt_opt(None::<u32>), "—");
    }
}
