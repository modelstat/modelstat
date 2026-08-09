//! What the AI actually spent, per pull request — the denominator of ROI.
//!
//! [`estimate_pr_effort`](crate::estimate_pr_effort) answers "what would a human
//! have paid for this PR". This module answers the other half: "what did the
//! machine pay". Both halves are read off THIS device, from files that are
//! already on it — the session transcripts the agents write as they work.
//!
//! ```text
//!   discovery::data_dir_candidates_in ──▶ transcripts (mtime >= window)
//!                                             │
//!                       claude_code / codex / pi / cursor parsers
//!                                             │
//!                                     RawEvent (tokens + references)
//!                                             │
//!                     group by session_id ──▶ dedupe_session_metadata
//!                                             │
//!             1 PR ──▶ whole mix           n PRs ──▶ even split PER CLASS
//!             0 PRs ─▶ unattributed (mix) / unattributed_sessions
//! ```
//!
//! ## Attribution is a claim, and it says how strong it is
//!
//! A session names the PRs it worked on the same way the daemon's
//! session-metadata pass learns them: the parsers mine each turn's full text for
//! public reference shapes into [`modelstat_wire::RawEvent::references`], and
//! `modelstat_parsers::dedupe_session_metadata` folds a session's blobs into one
//! ranked, deduped set. The rank (`git` > `tool` > `content` > `model`) is the
//! parsers' own; [`source_confidence`] is this module's reading of it as a
//! number, so a consumer can weigh a git-deterministic attribution against one
//! that came out of prose.
//!
//! Nothing here is a guess dressed as a measurement. A session that resolves to
//! no PR is NOT quietly dropped into the nearest one: its tokens land in
//! [`SpendSummary::unattributed`] and its existence in
//! `unattributed_sessions`, because on a real machine that number is large
//! (exploration, reading, ops work, sessions whose PR is never mentioned) and
//! hiding it would make every per-PR figure look better than it is.
//!
//! ## The denominator is input-equivalent tokens, not raw tokens
//!
//! Raw token counts are not comparable between PRs. Cache reads are 92.3% of
//! the raw volume on this machine and are re-counted every single turn, so a
//! raw sum ranks PRs by how long their conversation was rather than by how much
//! work they took. [`TokenMix::equiv_tokens`] converts the five classes into
//! fresh-input equivalents and IS the ROI denominator; [`TokenMix::raw_total`]
//! and every individual class stay right beside it, because a derived number
//! must never be the only number a reader can see. See [`W_INPUT`].
//!
//! ## Privacy
//!
//! [`PrSpend`] and [`SpendSummary`] carry a repo slug, a PR number and counts.
//! No path, no prompt, no diff, no commit message, no author. Transcript paths
//! and turn text exist inside this module for the length of one parse and are
//! dropped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use modelstat_parsers::discovery::data_dir_candidates_in;
use modelstat_parsers::{
    dedupe_session_metadata, detect_references, parse_claude_code_jsonl_streaming,
    parse_codex_rollout_streaming, parse_cursor_tracking_db, parse_pi_session_streaming,
    DetectedRefs, ParserContext,
};
use modelstat_wire::{RawEvent, TokenUsage};
use serde::Serialize;

/// The five token classes, kept apart all the way to the caller.
///
/// The parsers bucket these DISJOINTLY (see `modelstat-parsers`' codex notes),
/// so [`raw_total`](TokenMix::raw_total) is an honest sum rather than a double
/// count. Nothing upstream of this struct adds two classes together: an earlier
/// version of this module collapsed the whole input side into one counter, and
/// that single addition is precisely what made the ROI denominator wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenMix {
    /// Fresh prompt tokens — read by the model for the first time.
    pub input: u64,
    /// Completion tokens, reasoning excluded.
    pub output: u64,
    /// Cache writes: a prompt prefix persisted for later turns.
    pub cache_creation: u64,
    /// Cache reads: a prefix re-presented on a later turn — and re-counted on
    /// EVERY turn, which is why it dominates any long agent session.
    pub cache_read: u64,
    /// Reasoning tokens. Produced, so weighted as output.
    pub reasoning: u64,
}

/// One fresh input token: the unit the other three weights are expressed in.
///
/// # Why a raw token sum is the wrong ROI denominator
///
/// Measured on this device over 1,606 turns across 40 Claude Code sessions:
///
/// ```text
///   fresh input      1.4M    0.8%
///   output           0.7M    0.4%
///   cache write     11.5M    6.5%
///   cache read     162.7M   92.3%   ← re-counted on every turn
///   ──────────────────────────────
///   raw total      176.3M
/// ```
///
/// Cache reads are 92.3% of raw volume and bill at roughly a tenth of fresh
/// input, so a raw sum overstates billable-equivalent spend by ~5.1×. The worse
/// problem is comparability: a PR worked on inside a long-context session
/// outranks an identical PR from a short session purely because more context was
/// replayed each turn. That is a property of the conversation, not of the work,
/// and it makes PR-to-PR ROI meaningless.
///
/// # What these weights are — and what they are not
///
/// They are the Anthropic-family LIST RATIOS (cache write 1.25× input, cache
/// read 0.1× input, output 5× input) used as a unit conversion, so that classes
/// with genuinely different costs can be added up at all. OTHER PROVIDERS
/// DIFFER — OpenAI's cached-input discount is not 0.1×, and no vendor is obliged
/// to hold these ratios — so an equivalent computed here is comparable across
/// PRs on this device, not across vendors.
///
/// They are explicitly NOT A PRICE. This crate does not price tokens: a price is
/// a contract with a vendor that this device does not hold. Dollars stay opt-in
/// through the CLI's `--usd-per-mtok`, which the user supplies and which applies
/// to the equivalent figure.
pub const W_INPUT: f64 = 1.0;
/// A cache write costs 1.25× a fresh input token. See [`W_INPUT`].
pub const W_CACHE_WRITE: f64 = 1.25;
/// A cache read costs 0.1× a fresh input token — and is 92.3% of raw volume,
/// which is the entire reason this weighting exists. See [`W_INPUT`].
pub const W_CACHE_READ: f64 = 0.1;
/// A produced token (completion or reasoning) costs 5× a fresh input token.
/// See [`W_INPUT`].
pub const W_OUTPUT: f64 = 5.0;

impl TokenMix {
    /// Every token this device actually saw, unweighted.
    ///
    /// The honest raw figure. Kept beside [`equiv_tokens`](Self::equiv_tokens)
    /// rather than replaced by it: a reader must always be able to see both.
    #[must_use]
    pub const fn raw_total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
            .saturating_add(self.reasoning)
    }

    /// The mix in fresh-input-token equivalents — the ROI denominator.
    ///
    /// A normalization for comparability, not a price. See [`W_INPUT`].
    #[must_use]
    pub fn equiv_tokens(&self) -> f64 {
        self.input as f64 * W_INPUT
            + self.cache_creation as f64 * W_CACHE_WRITE
            + self.cache_read as f64 * W_CACHE_READ
            + (self.output as f64 + self.reasoning as f64) * W_OUTPUT
    }

    /// Add another mix, class by class. Saturating: a count that would wrap is
    /// pinned instead, since a wrapped total is a lie and a pinned one is not.
    fn add(&mut self, o: Self) {
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_creation = self.cache_creation.saturating_add(o.cache_creation);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.reasoning = self.reasoning.saturating_add(o.reasoning);
    }

    /// Share `i` of `n` when one session is split across several PRs.
    ///
    /// Every CLASS is divided on its own and carries its own remainder to the
    /// first shares, so the `n` shares sum back to `self` EXACTLY, class by
    /// class. Splitting the equivalent instead would let a PR's headline number
    /// drift from the classes printed beside it; here it cannot, because the
    /// equivalent is always recomputed from a mix that adds up.
    ///
    /// Private, and every caller has already established `n >= 1`.
    fn share(&self, i: u64, n: u64) -> Self {
        let part = |total: u64| total / n + u64::from(i < total % n);
        Self {
            input: part(self.input),
            output: part(self.output),
            cache_creation: part(self.cache_creation),
            cache_read: part(self.cache_read),
            reasoning: part(self.reasoning),
        }
    }
}

impl From<&TokenUsage> for TokenMix {
    /// Field for field. The wire type already separates the five classes; this
    /// module's job is to keep them that way.
    fn from(t: &TokenUsage) -> Self {
        Self {
            input: t.input,
            output: t.output,
            cache_creation: t.cache_creation,
            cache_read: t.cache_read,
            reasoning: t.reasoning,
        }
    }
}

/// What one pull request cost in machine tokens.
///
/// `mix` is the measurement: five disjoint classes, none of them pre-summed.
/// `equiv_tokens` is `mix.equiv_tokens()`, carried as a field because it is the
/// figure PRs are ranked and compared by — never a substitute for the classes,
/// always derivable from them.
///
/// Deliberately no dollars. Tokens are an exact local fact; a price is a
/// contract with a vendor that this device does not hold.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrSpend {
    /// `org/repo`, as first seen. Matched case-insensitively.
    pub slug: String,
    pub pr_number: u64,
    /// The raw classes, exactly as measured.
    pub mix: TokenMix,
    /// `mix.equiv_tokens()` — the ROI denominator. See [`W_INPUT`].
    pub equiv_tokens: f64,
    /// Sessions that contributed, including ones split across several PRs.
    pub session_count: u32,
    /// Token-weighted mean of the contributing sessions' confidences — how much
    /// of this figure rests on a deterministic reference rather than prose. See
    /// [`source_confidence`] and [`SPLIT_PENALTY`].
    pub attribution_confidence: f64,
}

/// Local session spend, split into what could be attributed and what could not.
///
/// `sessions_scanned` is every session considered, so
/// `unattributed_sessions / sessions_scanned` states the coverage of the
/// attributed figures instead of leaving a reader to assume it is 1.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SpendSummary {
    /// Highest [`PrSpend::equiv_tokens`] first — NOT highest raw total. The two
    /// orders differ, and that difference is the point: see [`W_INPUT`].
    pub by_pr: Vec<PrSpend>,
    /// Spend from sessions that resolved to no PR, as a full mix — so device
    /// coverage can be read in the same units as any PR.
    pub unattributed: TokenMix,
    pub unattributed_sessions: u32,
    pub sessions_scanned: u32,
}

/// A reference the repo's own git state produced — a branch, a remote, a commit.
pub const CONFIDENCE_GIT: f64 = 1.0;
/// A reference a tool invocation named (a `gh pr` call, a checkout).
pub const CONFIDENCE_TOOL: f64 = 0.8;
/// A reference mined out of turn text — a PR URL somebody pasted or wrote.
pub const CONFIDENCE_CONTENT: f64 = 0.6;
/// A reference an on-device model reported. Weakest: re-parsed free text.
pub const CONFIDENCE_MODEL: f64 = 0.4;

/// Applied when a session's tokens are split across several PRs: the split is
/// even, so no single PR's share is as trustworthy as an undivided one.
pub const SPLIT_PENALTY: f64 = 0.5;

/// The parsers' source rank (`git` > `tool` > `content` > `model`) as a weight.
///
/// Unknown sources read as `model` — the weakest — for the same reason the
/// parsers' own `source_rank` does: an unrecognised provenance is not a strong
/// one.
#[must_use]
pub fn source_confidence(source: &str) -> f64 {
    match source {
        "git" => CONFIDENCE_GIT,
        "tool" => CONFIDENCE_TOOL,
        "content" => CONFIDENCE_CONTENT,
        _ => CONFIDENCE_MODEL,
    }
}

/// Local session spend attributed to pull requests over the last `days` days.
///
/// Best-effort by construction. No agent installed, an unreadable data
/// directory, a transcript a parser chokes on, no `$HOME` — each degrades to
/// less data, never to an error and never to a panic. An empty
/// [`SpendSummary`] is a truthful answer for a machine with no session logs.
///
/// Bounded: only transcripts written within the window are opened (an older
/// file cannot hold a newer event), every parse streams in ≤256-event chunks
/// and keeps nothing but per-session counters, and events outside the window
/// are dropped as they arrive.
#[must_use]
pub fn spend_by_pr(days: u32) -> SpendSummary {
    let Some(home) = home_dir() else {
        return SpendSummary::default();
    };
    let since_ms = window_start_ms(days);
    let mut sessions: BTreeMap<String, SessionAcc> = BTreeMap::new();

    for (path, parser) in transcripts(&home, since_ms) {
        let ctx = ParserContext::new(DEVICE_ID, path).with_since_ms(Some(since_ms));
        // The events never all exist at once: each chunk is folded into the
        // per-session counters and dropped.
        let mut fold = |chunk: Vec<RawEvent>| {
            for e in &chunk {
                if in_window(&e.ts, since_ms) {
                    fold_event(&mut sessions, e);
                }
            }
        };
        let _ = match parser {
            Parser::ClaudeCode => parse_claude_code_jsonl_streaming(&ctx, &mut fold),
            Parser::Codex => parse_codex_rollout_streaming(&ctx, &mut fold),
            Parser::Pi => parse_pi_session_streaming(&ctx, &mut fold),
            // A key/value store has no streaming shape; its window is enforced
            // by the parser itself via `since_ms`.
            Parser::Cursor => parse_cursor_tracking_db(&ctx).map(|mut r| {
                fold(std::mem::take(&mut r.events));
                r
            }),
        };
    }

    finish(sessions)
}

/// [`spend_by_pr`]'s pure core: the same aggregation over events a caller
/// already holds, with no clock, no filesystem and no window (the caller's
/// slice IS the window). This is where the split, the confidence and the
/// unattributed accounting live.
#[must_use]
pub fn spend_by_pr_events(events: &[RawEvent]) -> SpendSummary {
    let mut sessions: BTreeMap<String, SessionAcc> = BTreeMap::new();
    for e in events {
        fold_event(&mut sessions, e);
    }
    finish(sessions)
}

/// One session, reduced to what attribution needs. Never holds an event.
#[derive(Default)]
struct SessionAcc {
    mix: TokenMix,
    /// The reference blobs this session's turns carried, folded once at the end.
    parts: Vec<DetectedRefs>,
}

/// Collapse a session's accumulated reference parts once they pass this many.
///
/// ponytail: a fixed memory ceiling per session — a 50k-turn transcript would
/// otherwise hold 50k blobs. `dedupe_session_metadata` is a fold, so collapsing
/// early loses nothing except applying its own 100-PR cap sooner.
const PARTS_COLLAPSE_AT: usize = 512;

/// Add one event's tokens and references to its session.
///
/// Only the channels that can yield a [`PullRequestRef`](modelstat_parsers::PullRequestRef)
/// are read. The daemon's pass also folds each event's git context and branch
/// tickets, but those produce repos and issue keys — never a PR — so they cannot
/// move an attribution and are not paid for here. (`source_confidence` still maps
/// `git`/`tool`, because a blob a caller built or replayed may carry either.)
fn fold_event(sessions: &mut BTreeMap<String, SessionAcc>, e: &RawEvent) {
    let acc = sessions.entry(e.session_id.clone()).or_default();

    if let Some(t) = &e.tokens {
        // Class for class. Summing any two of them here is what this module
        // used to do, and what it must never do again.
        acc.mix.add(TokenMix::from(t));
    }

    match &e.references {
        // The parser already mined this turn's FULL text (higher recall than the
        // excerpt). A malformed or foreign blob is skipped, as in the daemon.
        Some(r) => {
            if let Ok(refs) = serde_json::from_value::<DetectedRefs>(r.clone()) {
                acc.parts.push(refs);
            }
        }
        // Fallback for events whose parser stamped no blob (older or replayed):
        // the redacted excerpt is a subset of the text a blob would have covered.
        None => {
            if let Some(x) = e.content_excerpt.as_deref().filter(|x| !x.is_empty()) {
                acc.parts.push(detect_references(x, "content"));
            }
        }
    }

    if acc.parts.len() >= PARTS_COLLAPSE_AT {
        let m = dedupe_session_metadata(std::mem::take(&mut acc.parts));
        acc.parts.push(DetectedRefs {
            repos: m.repos,
            pull_requests: m.pull_requests,
            issues: m.issues,
        });
    }
}

/// Per-PR accumulation. `conf_weighted / weight` is the token-weighted mean
/// confidence; `conf_sum / sessions` covers the zero-token case, where every
/// weight is 0 and a weighted mean is undefined.
#[derive(Default)]
struct PrAcc {
    slug: String,
    mix: TokenMix,
    sessions: u32,
    conf_weighted: f64,
    weight: u64,
    conf_sum: f64,
}

/// Turn per-session counters into the summary: attribute, split, sort.
fn finish(sessions: BTreeMap<String, SessionAcc>) -> SpendSummary {
    let mut out = SpendSummary::default();
    // Keyed on the lowercased slug — forge slugs are case-insensitive, and two
    // spellings of one repo must not read as two repos.
    let mut acc: BTreeMap<(String, u64), PrAcc> = BTreeMap::new();

    for (_session_id, session) in sessions {
        out.sessions_scanned = out.sessions_scanned.saturating_add(1);
        let prs = session_prs(session.parts);

        if prs.is_empty() {
            out.unattributed.add(session.mix);
            out.unattributed_sessions = out.unattributed_sessions.saturating_add(1);
            continue;
        }

        // An EVEN split. File-overlap weighting would be better — a session that
        // touched nine of PR A's files and one of PR B's did not spend half its
        // tokens on each — but the overlap needs `FileRef`s, and those are built
        // by the daemon's session-metadata pass from per-repo `git --numstat`
        // reads, not by this path. Inventing a weight from what IS here (turn
        // counts, mention counts) would be a guess wearing a measurement's
        // clothes, and an even split at least states its own error honestly via
        // SPLIT_PENALTY.
        let n = prs.len() as u64;

        for (i, (slug, number, confidence)) in prs.into_iter().enumerate() {
            // Each CLASS is split on its own, and each carries its own
            // remainder to the first PRs, so the shares sum EXACTLY to what the
            // session spent: integer division would quietly evaporate up to n-1
            // tokens per class, and evaporating spend is the one rounding this
            // module must not do. Splitting the equivalent instead would let a
            // share's headline number disagree with its own classes.
            let share = session.mix.share(i as u64, n);
            let confidence = if n > 1 {
                confidence * SPLIT_PENALTY
            } else {
                confidence
            };

            let entry = acc.entry((slug.to_lowercase(), number)).or_default();
            if entry.slug.is_empty() {
                entry.slug = slug;
            }
            entry.mix.add(share);
            entry.sessions = entry.sessions.saturating_add(1);
            // Weighted by RAW volume, as before: this weight only decides how
            // loudly a session speaks in the confidence mean, and reweighting it
            // would silently change an attribution figure this change is not
            // about.
            let weight = share.raw_total();
            entry.weight = entry.weight.saturating_add(weight);
            entry.conf_weighted += confidence * weight as f64;
            entry.conf_sum += confidence;
        }
    }

    out.by_pr = acc
        .into_iter()
        .map(|((_, pr_number), a)| PrSpend {
            slug: a.slug,
            pr_number,
            mix: a.mix,
            equiv_tokens: a.mix.equiv_tokens(),
            session_count: a.sessions,
            attribution_confidence: if a.weight > 0 {
                a.conf_weighted / a.weight as f64
            } else if a.sessions > 0 {
                a.conf_sum / f64::from(a.sessions)
            } else {
                0.0
            },
        })
        .collect();
    // Equivalent spend first — a cache-read-heavy PR must not outrank a PR that
    // did more work on a shorter context. `total_cmp` because the values are
    // finite by construction and a sort must be a total order regardless.
    // Then the key, so a tie is ordered rather than arbitrary.
    out.by_pr.sort_by(|a, b| {
        b.equiv_tokens
            .total_cmp(&a.equiv_tokens)
            .then_with(|| a.slug.cmp(&b.slug))
            .then_with(|| a.pr_number.cmp(&b.pr_number))
    });
    out
}

/// The PRs one session resolved to, as `(slug, number, confidence)`.
///
/// A PR with no slug is dropped: it names a number on an unknown repo, which
/// cannot be joined to anything, and a session left with only those resolves to
/// no PR at all — honestly unattributed rather than attributed to a guess.
fn session_prs(parts: Vec<DetectedRefs>) -> Vec<(String, u64, f64)> {
    dedupe_session_metadata(parts)
        .pull_requests
        .into_iter()
        .filter_map(|pr| {
            let slug = pr.slug.filter(|s| !s.is_empty())?;
            Some((slug, pr.number, source_confidence(&pr.source)))
        })
        .collect()
}

// ── Discovery ────────────────────────────────────────────────────────────────

/// Which parser reads a discovered transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parser {
    ClaudeCode,
    Codex,
    Pi,
    Cursor,
}

/// Stamped on every parsed event's id. Local-only and never returned — the
/// parsers require a device id to build `source_event_id`, which this path
/// discards.
const DEVICE_ID: &str = "roi_local";

/// Where each JSONL agent keeps its transcripts: the source-registry agent name,
/// the subdirectory under its data directory, and how many levels below that a
/// transcript sits.
///
/// WHERE the data directory is comes from `discovery::data_dir_candidates_in` —
/// the one source registry, which honours `CLAUDE_HOME`/`CODEX_HOME`/`PI_HOME`
/// and reads a relocated directory off a running agent's own command line. A
/// second path list here would drift from it, which is exactly the bug the
/// registry exists to prevent.
///
/// The depths are exact, not maxima, and that is load-bearing for pi: its
/// `sessions/<project>/<ts>_<uuid>/` subdirectories hold a session's SUBAGENT and
/// tool logs, so descending further would count a subagent's tokens twice,
/// against both itself and its parent.
///
/// ponytail: Claude Desktop's local-agent-mode tree (a relocated `.claude` nested
/// several levels under the app's data directory) is not walked — the layout is
/// unverifiable here (this machine has the directory and no transcripts in it),
/// and an 8-deep guess through an Electron app's caches is exactly the unbounded
/// work a user-facing command must not do. Upgrade path: the daemon's
/// `discover_jobs::claude_search_roots` already searches it by shape; reuse that
/// if this crate ever sits above the daemon in the graph.
const STORES: &[(&str, &str, usize, Parser)] = &[
    // <data-dir>/projects/<encoded-cwd>/<session>.jsonl
    ("claude_code", "projects", 2, Parser::ClaudeCode),
    // <data-dir>/sessions/<y>/<m>/<d>/rollout-*.jsonl
    ("codex_cli", "sessions", 4, Parser::Codex),
    // <data-dir>/sessions/<project>/<ts>_<uuid>.jsonl
    ("pi", "sessions", 2, Parser::Pi),
];

/// Cursor's chat store: ONE global key/value DB, not per-session files.
const CURSOR_DB_RELATIVE_PATH: &str = "User/globalStorage/state.vscdb";

/// Every transcript on this device written at or after `since_ms`, deduped.
///
/// The mtime floor is the window's cheap half: a file last written before the
/// window cannot hold an event inside it, so it is never opened. Events inside
/// a file that IS in the window are still checked one by one ([`in_window`]).
fn transcripts(home: &Path, since_ms: i64) -> Vec<(String, Parser)> {
    let mut out: Vec<(String, Parser)> = Vec::new();

    for (agent, sub, depth, parser) in STORES {
        for data_dir in data_dir_candidates_in(home, agent) {
            let mut found: Vec<PathBuf> = Vec::new();
            collect_jsonl(&PathBuf::from(&data_dir).join(sub), *depth, since_ms, &mut found);
            out.extend(
                found
                    .into_iter()
                    .map(|p| (p.to_string_lossy().into_owned(), *parser)),
            );
        }
    }

    for data_dir in data_dir_candidates_in(home, "cursor") {
        let db = PathBuf::from(&data_dir).join(CURSOR_DB_RELATIVE_PATH);
        if db.is_file() {
            out.push((db.to_string_lossy().into_owned(), Parser::Cursor));
        }
    }

    // One transcript, one parse. The candidate lists overlap by design (a
    // relocated home a running process also names), and parsing a file twice
    // would double every token in it.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// `.jsonl` files EXACTLY `depth` levels below `dir`, whose mtime is at or after
/// `since_ms`. An unreadable directory yields nothing.
fn collect_jsonl(dir: &Path, depth: usize, since_ms: i64, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if depth == 1 {
            if path.extension().map(|e| e == "jsonl").unwrap_or(false)
                // A file we cannot stat is kept: dropping it would silently lose
                // spend over a metadata hiccup.
                && mtime_ms(&entry).unwrap_or(i64::MAX) >= since_ms
            {
                out.push(path);
            }
        } else if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_jsonl(&path, depth - 1, since_ms, out);
        }
    }
}

fn mtime_ms(entry: &std::fs::DirEntry) -> Option<i64> {
    let modified = entry.metadata().ok()?.modified().ok()?;
    let ms = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(ms).ok()
}

/// `$HOME` (or `%USERPROFILE%`), matching the parsers' own home probe.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Epoch-ms `days` days ago. Saturating throughout: `days: u32::MAX` asks for
/// all of history and gets it, rather than wrapping into the future.
fn window_start_ms(days: u32) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    now.saturating_sub(i64::from(days).saturating_mul(86_400_000))
}

/// Is this event's timestamp at or after the window start?
///
/// Parsed rather than compared as a string: a transcript may stamp a local
/// offset (`+02:00`), and ISO-8601 only sorts lexically within one offset. An
/// unparseable or absent timestamp cannot be placed in time and is excluded —
/// the alternative is counting a years-old turn against this month.
fn in_window(ts: &str, since_ms: i64) -> bool {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| d.timestamp_millis() >= since_ms)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_parsers::detect_event_references;
    use serde_json::json;

    /// Equality for a weighted figure. The tolerance guards against
    /// [`W_CACHE_READ`]'s binary representation (0.1 is not exact in f64), not
    /// against a wrong answer: it scales with the value and stays many orders of
    /// magnitude tighter than one token.
    #[track_caller]
    fn assert_equiv(got: f64, want: f64) {
        let tol = 1e-9 * want.abs().max(1.0);
        assert!((got - want).abs() <= tol, "equiv {got} != {want}");
    }

    /// An event with a mined `references` blob, exactly as a parser stamps one.
    fn ev(session: &str, text: &str, input: u64, output: u64) -> RawEvent {
        let mut e = bare(session, input, output);
        e.references = detect_event_references(text);
        e
    }

    /// The same, carrying the FULL five-class mix a real turn reports — on a
    /// long agent session fresh input is the rare class, not the common one.
    fn ev_mix(session: &str, text: &str, tokens: TokenUsage) -> RawEvent {
        let mut e = bare(session, 0, 0);
        e.tokens = Some(tokens);
        e.references = detect_event_references(text);
        e
    }

    /// An event carrying a hand-built blob — the way a replayed or
    /// caller-supplied reference reaches this path with a source of its own.
    fn ev_sourced(session: &str, source: &str, input: u64, output: u64) -> RawEvent {
        let mut e = bare(session, input, output);
        e.references = Some(json!({
            "repos": [],
            "pull_requests": [{"slug": "acme/api", "number": 42, "source": source, "confidence": 0.9}],
            "issues": [],
        }));
        e
    }

    fn bare(session: &str, input: u64, output: u64) -> RawEvent {
        RawEvent {
            source_event_id: format!("{session}_{input}_{output}"),
            ts: "2026-08-01T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: session.into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: Some(TokenUsage {
                input,
                output,
                ..TokenUsage::default()
            }),
            tokens_unmapped: Default::default(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            content_bytes: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    #[test]
    fn single_pr_session_attributes_every_token() {
        let s = spend_by_pr_events(&[
            ev("s1", "working on https://github.com/acme/api/pull/42", 100, 10),
            ev("s1", "still on https://github.com/acme/api/pull/42", 50, 5),
        ]);
        assert_eq!(s.by_pr.len(), 1);
        let pr = &s.by_pr[0];
        assert_eq!((pr.slug.as_str(), pr.pr_number), ("acme/api", 42));
        assert_eq!(
            pr.mix,
            TokenMix {
                input: 150,
                output: 15,
                ..TokenMix::default()
            }
        );
        assert_eq!(pr.mix.raw_total(), 165);
        // 150 fresh input at 1x + 15 output at 5x.
        assert_equiv(pr.equiv_tokens, 150.0 + 75.0);
        assert_eq!(pr.session_count, 1);
        assert_eq!(pr.attribution_confidence, CONFIDENCE_CONTENT);
        assert_eq!(s.unattributed, TokenMix::default());
        assert_eq!(s.unattributed_sessions, 0);
        assert_eq!(s.sessions_scanned, 1);
    }

    #[test]
    fn two_pr_session_splits_evenly_and_halves_confidence() {
        let s = spend_by_pr_events(&[ev(
            "s1",
            "landing https://github.com/acme/api/pull/1 then https://github.com/acme/api/pull/2",
            100,
            10,
        )]);
        assert_eq!(s.by_pr.len(), 2);
        for pr in &s.by_pr {
            assert_eq!(
                pr.mix,
                TokenMix {
                    input: 50,
                    output: 5,
                    ..TokenMix::default()
                },
                "PR {} share",
                pr.pr_number
            );
            assert_eq!(pr.mix.raw_total(), 55);
            assert_equiv(pr.equiv_tokens, 75.0);
            assert_eq!(
                pr.attribution_confidence,
                CONFIDENCE_CONTENT * SPLIT_PENALTY
            );
            assert_eq!(pr.session_count, 1);
        }
        assert_eq!(s.unattributed, TokenMix::default());
        assert_eq!(s.sessions_scanned, 1);
    }

    #[test]
    fn class_wise_split_loses_no_tokens_in_any_class() {
        // Every class carries an awkward remainder across 3 PRs. The shares must
        // sum back to the session's own spend CLASS BY CLASS, not merely in
        // total: a mix that does not add up would let `equiv_tokens` disagree
        // with the numbers printed beside it.
        let spent = TokenUsage {
            input: 101,
            output: 7,
            cache_creation: 1_000,
            cache_read: 65_537,
            reasoning: 2,
        };
        let s = spend_by_pr_events(&[ev_mix(
            "s1",
            "https://github.com/acme/api/pull/1 https://github.com/acme/api/pull/2 https://github.com/acme/api/pull/3",
            spent.clone(),
        )]);
        assert_eq!(s.by_pr.len(), 3);

        let mut summed = TokenMix::default();
        for pr in &s.by_pr {
            summed.add(pr.mix);
            // Each share's headline stays derivable from its own classes.
            assert_equiv(pr.equiv_tokens, pr.mix.equiv_tokens());
        }
        assert_eq!(
            summed,
            TokenMix::from(&spent),
            "a class drifted while splitting"
        );
        assert_eq!(summed.raw_total(), 66_647);

        let parts: f64 = s.by_pr.iter().map(|p| p.equiv_tokens).sum();
        assert_equiv(parts, TokenMix::from(&spent).equiv_tokens());
    }

    #[test]
    fn unattributed_carries_a_full_mix() {
        let unlabelled = TokenUsage {
            input: 400,
            output: 40,
            cache_creation: 4_000,
            cache_read: 90_000,
            reasoning: 9,
        };
        let s = spend_by_pr_events(&[
            ev_mix(
                "s1",
                "just reading code, no references here",
                unlabelled.clone(),
            ),
            ev("s2", "fixing https://github.com/acme/api/pull/9", 100, 10),
        ]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].pr_number, 9);
        // Not a scalar: the device's uncovered spend is readable in the same
        // classes, and the same units, as any PR's.
        assert_eq!(s.unattributed, TokenMix::from(&unlabelled));
        assert_eq!(s.unattributed.raw_total(), 94_449);
        // 400 + 4000*1.25 + 90000*0.1 + (40+9)*5
        assert_equiv(
            s.unattributed.equiv_tokens(),
            400.0 + 5_000.0 + 9_000.0 + 245.0,
        );
        assert_eq!(s.unattributed_sessions, 1);
        assert_eq!(s.sessions_scanned, 2);
    }

    #[test]
    fn confidence_follows_source_rank() {
        for (source, expected) in [
            ("git", CONFIDENCE_GIT),
            ("tool", CONFIDENCE_TOOL),
            ("content", CONFIDENCE_CONTENT),
            ("model", CONFIDENCE_MODEL),
            ("something_new", CONFIDENCE_MODEL),
        ] {
            let s = spend_by_pr_events(&[ev_sourced("s1", source, 10, 1)]);
            assert_eq!(s.by_pr.len(), 1, "{source}");
            assert_eq!(s.by_pr[0].attribution_confidence, expected, "{source}");
        }
    }

    #[test]
    fn strongest_source_in_a_session_wins() {
        // The same PR seen from prose and from a git-sourced blob: the stronger
        // provenance sets the confidence, matching the parsers' own dedupe.
        let s = spend_by_pr_events(&[
            ev("s1", "see https://github.com/acme/api/pull/42", 10, 1),
            ev_sourced("s1", "git", 10, 1),
        ]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].attribution_confidence, CONFIDENCE_GIT);
        assert_eq!(s.by_pr[0].mix.raw_total(), 22);
    }

    #[test]
    fn sessions_scanned_counts_every_session() {
        let s = spend_by_pr_events(&[
            ev("s1", "https://github.com/acme/api/pull/1", 10, 1),
            ev(
                "s2",
                "https://github.com/acme/api/pull/2 https://github.com/acme/api/pull/3",
                20,
                2,
            ),
            ev("s3", "no refs at all", 30, 3),
            ev("s4", "https://github.com/acme/api/pull/1", 40, 4),
        ]);
        assert_eq!(s.sessions_scanned, 4);
        assert_eq!(s.unattributed_sessions, 1);
        // PR 1 was worked on by two sessions, wholly by both.
        let pr1 = s.by_pr.iter().find(|p| p.pr_number == 1).unwrap();
        assert_eq!(pr1.session_count, 2);
        assert_eq!(pr1.mix.raw_total(), 11 + 44);
        // Every token is either attributed or reported as unattributed.
        let attributed: u64 = s.by_pr.iter().map(|p| p.mix.raw_total()).sum();
        assert_eq!(attributed + s.unattributed.raw_total(), 11 + 22 + 33 + 44);
    }

    #[test]
    fn every_class_survives_to_the_caller() {
        let mut e = ev("s1", "https://github.com/acme/api/pull/42", 0, 0);
        e.tokens = Some(TokenUsage {
            input: 1,
            output: 2,
            cache_creation: 4,
            cache_read: 8,
            reasoning: 16,
        });
        let s = spend_by_pr_events(&[e]);
        let pr = &s.by_pr[0];
        // Nothing was pre-summed away: all five are still individually readable.
        assert_eq!(
            pr.mix,
            TokenMix {
                input: 1,
                output: 2,
                cache_creation: 4,
                cache_read: 8,
                reasoning: 16,
            }
        );
        assert_eq!(pr.mix.raw_total(), 31);
        // 1 + 4*1.25 + 8*0.1 + (2+16)*5
        assert_equiv(pr.equiv_tokens, 1.0 + 5.0 + 0.8 + 90.0);
    }

    #[test]
    fn equiv_tokens_is_the_hand_computed_weighted_sum() {
        let mix = TokenMix {
            input: 1_000,
            output: 200,
            cache_creation: 400,
            cache_read: 8_000,
            reasoning: 50,
        };
        // By hand: 1000*1.0 + 400*1.25 + 8000*0.1 + (200+50)*5.0
        //        = 1000    + 500      + 800       + 1250       = 3550
        assert_equiv(mix.equiv_tokens(), 3_550.0);
        assert_eq!(mix.raw_total(), 9_650);
        // The four ratios live in one place, and are the documented ones.
        assert_eq!(
            (W_INPUT, W_CACHE_WRITE, W_CACHE_READ, W_OUTPUT),
            (1.0, 1.25, 0.1, 5.0)
        );
    }

    #[test]
    fn the_measured_device_mix_is_overstated_five_fold_by_raw_tokens() {
        // The mix this whole change exists for: 1,606 turns across 40 Claude
        // Code sessions on one real machine.
        let measured = TokenMix {
            input: 1_400_000,
            output: 700_000,
            cache_creation: 11_500_000,
            cache_read: 162_700_000,
            reasoning: 0,
        };
        assert_eq!(measured.raw_total(), 176_300_000);
        let raw = measured.raw_total() as f64;
        // Cache reads alone are 92.3% of the raw volume …
        assert!(
            (measured.cache_read as f64 / raw - 0.923).abs() < 0.001,
            "cache-read share drifted from the measurement"
        );
        // … and re-counting them every turn is what inflates raw by ~5x.
        assert_equiv(measured.equiv_tokens(), 35_545_000.0);
        let overstatement = raw / measured.equiv_tokens();
        assert!(
            (4.5..=5.5).contains(&overstatement),
            "raw/equiv was {overstatement}, expected ~5x"
        );
    }

    #[test]
    fn same_raw_total_ranks_by_work_not_by_context_length() {
        // Two PRs, identical raw totals. One spent it on replayed context, the
        // other on fresh input. They are not the same amount of work.
        let s = spend_by_pr_events(&[
            ev_mix(
                "s1",
                "https://github.com/acme/api/pull/1",
                TokenUsage {
                    input: 50_000,
                    cache_read: 950_000,
                    ..TokenUsage::default()
                },
            ),
            ev_mix(
                "s2",
                "https://github.com/acme/api/pull/2",
                TokenUsage {
                    input: 1_000_000,
                    ..TokenUsage::default()
                },
            ),
        ]);
        let pr1 = s.by_pr.iter().find(|p| p.pr_number == 1).unwrap();
        let pr2 = s.by_pr.iter().find(|p| p.pr_number == 2).unwrap();
        assert_eq!(pr1.mix.raw_total(), pr2.mix.raw_total());
        // 95% replayed context costs roughly a seventh of the same raw volume
        // spent on fresh input.
        assert_equiv(pr1.equiv_tokens, 145_000.0);
        assert_equiv(pr2.equiv_tokens, 1_000_000.0);
        let order: Vec<u64> = s.by_pr.iter().map(|p| p.pr_number).collect();
        assert_eq!(order, vec![2, 1], "the cache-read PR must rank far below");
    }

    #[test]
    fn sorting_by_equivalent_reverses_a_raw_ordering() {
        // PR 1 burned twice PR 2's raw tokens, all of it replayed context. Raw
        // ranks it first; equivalent ranks it last. That disagreement IS the
        // defect this module was changed to fix.
        let s = spend_by_pr_events(&[
            ev_mix(
                "s1",
                "https://github.com/acme/api/pull/1",
                TokenUsage {
                    cache_read: 2_000_000,
                    ..TokenUsage::default()
                },
            ),
            ev_mix(
                "s2",
                "https://github.com/acme/api/pull/2",
                TokenUsage {
                    input: 1_000_000,
                    ..TokenUsage::default()
                },
            ),
        ]);
        let mut by_raw = s.by_pr.clone();
        by_raw.sort_by_key(|p| std::cmp::Reverse(p.mix.raw_total()));
        assert_eq!(
            by_raw.iter().map(|p| p.pr_number).collect::<Vec<_>>(),
            vec![1, 2],
            "raw would rank the replayed-context PR first"
        );
        assert_eq!(
            s.by_pr.iter().map(|p| p.pr_number).collect::<Vec<_>>(),
            vec![2, 1],
            "by_pr must be ordered by equivalent, not raw"
        );
    }

    #[test]
    fn by_pr_is_sorted_by_equiv_tokens_descending() {
        let s = spend_by_pr_events(&[
            ev("s1", "https://github.com/acme/api/pull/1", 10, 0),
            ev("s2", "https://github.com/acme/api/pull/2", 900, 0),
            ev("s3", "https://github.com/acme/api/pull/3", 100, 0),
        ]);
        let order: Vec<u64> = s.by_pr.iter().map(|p| p.pr_number).collect();
        assert_eq!(order, vec![2, 3, 1]);
    }

    #[test]
    fn serialized_spend_shows_every_raw_class_beside_the_equivalent() {
        let s = spend_by_pr_events(&[ev_mix(
            "s1",
            "https://github.com/acme/api/pull/42",
            TokenUsage {
                input: 1,
                output: 2,
                cache_creation: 4,
                cache_read: 8,
                reasoning: 16,
            },
        )]);
        let v = serde_json::to_value(&s.by_pr[0]).unwrap();
        for class in ["input", "output", "cache_creation", "cache_read", "reasoning"] {
            assert!(
                v["mix"][class].as_u64().is_some(),
                "{class} must survive into the JSON"
            );
        }
        assert_eq!(v["mix"]["cache_read"], 8);
        assert!(v["equiv_tokens"].as_f64().is_some());
    }

    #[test]
    fn excerpt_is_the_fallback_when_no_blob_was_stamped() {
        let mut e = bare("s1", 10, 1);
        e.content_excerpt = Some("fixes https://github.com/acme/api/pull/7".into());
        let s = spend_by_pr_events(&[e]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].pr_number, 7);
        assert_eq!(s.by_pr[0].attribution_confidence, CONFIDENCE_CONTENT);
    }

    #[test]
    fn a_pr_without_a_slug_cannot_be_attributed() {
        let mut e = bare("s1", 10, 1);
        e.references = Some(json!({
            "repos": [],
            "pull_requests": [{"number": 5, "source": "content", "confidence": 0.9}],
            "issues": [],
        }));
        let s = spend_by_pr_events(&[e]);
        assert!(s.by_pr.is_empty());
        assert_eq!(s.unattributed.raw_total(), 11);
        assert_eq!(s.unattributed_sessions, 1);
    }

    #[test]
    fn one_slug_two_spellings_is_one_pr() {
        let mut lower = bare("s1", 10, 1);
        lower.references = Some(json!({
            "repos": [],
            "pull_requests": [{"slug": "acme/api", "number": 3, "source": "content", "confidence": 0.9}],
            "issues": [],
        }));
        let mut upper = bare("s2", 20, 2);
        upper.references = Some(json!({
            "repos": [],
            "pull_requests": [{"slug": "Acme/API", "number": 3, "source": "content", "confidence": 0.9}],
            "issues": [],
        }));
        let s = spend_by_pr_events(&[lower, upper]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].session_count, 2);
        assert_eq!(s.by_pr[0].mix.raw_total(), 33);
    }

    #[test]
    fn zero_token_session_still_attributes_and_keeps_confidence() {
        let mut e = ev("s1", "https://github.com/acme/api/pull/42", 0, 0);
        e.tokens = None;
        let s = spend_by_pr_events(&[e]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].mix, TokenMix::default());
        assert_eq!(s.by_pr[0].mix.raw_total(), 0);
        assert_equiv(s.by_pr[0].equiv_tokens, 0.0);
        // Weighted mean is undefined at zero weight; the plain mean stands in.
        assert_eq!(s.by_pr[0].attribution_confidence, CONFIDENCE_CONTENT);
    }

    #[test]
    fn no_events_is_an_empty_summary() {
        assert_eq!(spend_by_pr_events(&[]), SpendSummary::default());
    }

    /// Tolerant smoke test: the real machine. Must return — a CI box with no
    /// agent installed legitimately produces an empty summary — and must never
    /// panic or block. Only invariants are asserted, never counts.
    #[test]
    fn spend_by_pr_on_this_machine_returns_consistent_numbers() {
        let s = spend_by_pr(30);
        assert!(
            s.by_pr
                .windows(2)
                .all(|w| w[0].equiv_tokens >= w[1].equiv_tokens),
            "by_pr must be sorted by equiv_tokens descending"
        );
        for pr in &s.by_pr {
            assert!(!pr.slug.is_empty(), "an attributed PR always names its repo");
            assert!(pr.pr_number >= 1);
            // The ranked figure is always recomputable from the raw classes
            // published beside it — never a number a reader cannot check.
            assert_equiv(pr.equiv_tokens, pr.mix.equiv_tokens());
            assert!(pr.equiv_tokens >= 0.0);
            assert!(pr.equiv_tokens <= pr.mix.raw_total() as f64 * W_OUTPUT);
            assert!(pr.session_count >= 1);
            assert!(
                pr.attribution_confidence > 0.0 && pr.attribution_confidence <= CONFIDENCE_GIT,
                "confidence {} out of range",
                pr.attribution_confidence
            );
        }
        assert!(
            u32::try_from(s.by_pr.len()).unwrap_or(u32::MAX) <= s.sessions_scanned.saturating_mul(2)
                || s.sessions_scanned > 0,
            "attributed PRs imply scanned sessions"
        );
        assert!(s.unattributed_sessions <= s.sessions_scanned);
    }

    /// `days: 0` asks for nothing and must not walk into the future or panic.
    #[test]
    fn zero_day_window_is_safe() {
        let s = spend_by_pr(0);
        assert!(s.unattributed_sessions <= s.sessions_scanned);
    }
}
