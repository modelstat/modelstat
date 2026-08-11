//! What the machine spent on work that shipped: tokens and time, per pull
//! request.
//!
//! Two measured quantities, both read off THIS device from files that are
//! already on it — the session transcripts the agents write as they work — and
//! both attributed to merged PRs by the same evidence with the same weights:
//! [`TokenMix`], the five disjoint token classes, and [`active_ms`], the union
//! of a session's activity windows. They are reported side by side and never
//! combined, with each other or with anything the PR shipped.
//!
//! ```text
//!   discovery::data_dir_candidates_in ──▶ transcripts (mtime >= window)
//!                                             │
//!                       claude_code / codex / pi / cursor parsers
//!                                             │
//!                    RawEvent (tokens + references + cwd + ts)
//!                                             │
//!     group by session_id ──▶ token mix, active_ms, [start, end], cwds
//!                                             │
//!        git: files the window changed  ×  files each merged PR changed
//!                                             │
//!            overlap × time proximity ──▶ score ──▶ split PER CLASS
//!                                                   AND time, PROPORTIONAL
//!                                                   to the same score
//!                                             │
//!             no match ─▶ unattributed (mix + active_ms) / sessions
//! ```
//!
//! ## A session is joined to the PR it AUTHORED, not the one it mentioned
//!
//! The first cut of this module joined on PR references alone — the numbers and
//! URLs the parsers mine out of turn text into
//! [`modelstat_wire::RawEvent::references`]. That is backwards, and visibly so:
//! a PR number does not exist while the work is being done. The branch and the
//! commits come first and the PR is opened afterwards, so a reference in a
//! transcript overwhelmingly marks a session that DISCUSSED an already-open PR
//! (a review, a follow-up, "look at #1037") rather than the one that wrote it.
//! Attributing spend that way attaches the denominator to the wrong numerator,
//! and on this device it inverted the headline: AI-authored PRs showed no spend
//! while human-authored PRs showed all of it.
//!
//! The join that survives real repositories is CHANGED-FILE OVERLAP plus TIME
//! PROXIMITY. Matching the session's own commit SHAs would be simpler and does
//! not work: squash merging — which is the dominant convention, and the only one
//! some repos use — rewrites them, so a session's local shas never appear on the
//! mainline. What squashing cannot rewrite is WHICH FILES changed. A session
//! that authored a PR touched substantially the same files, shortly before that
//! PR merged, and both halves of that sentence are already readable on-device:
//!
//!   * the session side from `git log --since --until --numstat` over the
//!     session's window (plus [`COMMIT_GRACE_MS`], because work is committed a
//!     little after the talking stops) — the same read the daemon's
//!     session-metadata pass makes to build its `FileRef`s;
//!   * the PR side from a bounded `git show --numstat -m --first-parent` on the
//!     merge commit, cached per `(repo, merge sha)` for the whole call.
//!
//! [`file_overlap`] is the Jaccard index of the two sets, [`time_proximity`]
//! decays it with the gap to the merge, and the product is the match SCORE that
//! both selects the PR and weights the split. References still matter, but only
//! as a second signal layered on top: see [`CONFIDENCE_MENTION_ONLY`] for the
//! case that produced the inversion.
//!
//! Nothing here is a guess dressed as a measurement. A session that resolves to
//! no PR is NOT quietly dropped into the nearest one: its tokens and its time
//! land in [`SpendSummary::unattributed`] and
//! [`unattributed_active_ms`](SpendSummary::unattributed_active_ms), and its
//! existence in `unattributed_sessions`, because on a real machine that number
//! is large (exploration, reading, ops work, sessions whose repo is not on this
//! disk) and hiding it would make every per-PR figure look better than it is.
//!
//! ## The comparable token figure is input-equivalent, not raw
//!
//! Raw token counts are not comparable between PRs. Cache reads are 92.3% of
//! the raw volume on this machine and are re-counted every single turn, so a
//! raw sum ranks PRs by how long their conversation was rather than by how much
//! work they took. [`TokenMix::equiv_tokens`] converts the five classes into
//! fresh-input equivalents; [`TokenMix::raw_total`] and every individual class
//! stay right beside it, because a derived number must never be the only number
//! a reader can see. See [`W_INPUT`].
//!
//! ## Time is measured, not inferred
//!
//! [`active_ms`] counts activity windows, so idle gaps are excluded and a
//! session left open overnight does not bill the night. It is a count of when
//! this device was working, nothing more: it is never multiplied by a rate,
//! never compared against what a human "would have" taken, and never turned
//! into a saving. The turn-level half of the time plane —
//! `agent_working_ms`, the developer's wait ON the agent — needs message timing
//! this crate does not model, and is therefore absent rather than approximated.
//!
//! ## Privacy
//!
//! [`PrSpend`] and [`SpendSummary`] carry a repo slug, a PR number and counts.
//! No path, no prompt, no diff, no commit message, no author. Transcript paths,
//! turn text, working directories, event timestamps and the two changed-file
//! sets the join is computed from all exist inside this module for the length
//! of one call and are dropped. No type that leaves this module has a field a
//! path could be stored in.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use modelstat_parsers::discovery::data_dir_candidates_in;
use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::git_anchors::select_anchor_commits;
use modelstat_parsers::git_files::collect_files_changed;
use modelstat_parsers::git_outcome::parse_git_log;
use modelstat_parsers::{
    dedupe_session_metadata, detect_references, parse_claude_code_jsonl_streaming,
    parse_codex_rollout_streaming, parse_cursor_tracking_db, parse_pi_session_streaming,
    AnchorConfig, DetectedRefs, GitResolver, ParserContext,
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

    /// Split into one share per weight, PROPORTIONAL to the weights already
    /// sanitized by [`split_weights`].
    ///
    /// Every CLASS is apportioned on its own by largest remainder — floor each
    /// exact share, then hand the leftover units to the largest fractional
    /// parts — so the `n` shares sum back to `self` EXACTLY, class by class.
    /// Plain integer division would quietly evaporate up to `n-1` tokens per
    /// class, and evaporating spend is the one rounding this module must not
    /// do. Splitting the equivalent instead would let a share's headline number
    /// drift from the classes printed beside it; here it cannot, because the
    /// equivalent is always recomputed from a mix that adds up.
    ///
    /// The weights are the join's match scores ([`PrMatch`]), so a session that
    /// touched nine of PR A's files and one of PR B's is charged that way. An
    /// earlier version split EVENLY and said so; the even split was a
    /// placeholder for exactly this.
    ///
    /// `(w, sum)` arrive pre-sanitized rather than being derived here because
    /// `finish` apportions the same session's `active_ms` with the very same
    /// pair. Two call sites re-deriving "the same" weights is how time and
    /// tokens would eventually stop dividing a session the same way.
    fn split_with(&self, w: &[f64], sum: f64) -> Vec<Self> {
        let n = w.len();
        if n == 0 {
            return Vec::new();
        }
        let input = apportion(self.input, w, sum);
        let output = apportion(self.output, w, sum);
        let cache_creation = apportion(self.cache_creation, w, sum);
        let cache_read = apportion(self.cache_read, w, sum);
        let reasoning = apportion(self.reasoning, w, sum);
        (0..n)
            .map(|i| Self {
                input: input[i],
                output: output[i],
                cache_creation: cache_creation[i],
                cache_read: cache_read[i],
                reasoning: reasoning[i],
            })
            .collect()
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

/// The window one event opens. Every event says "somebody was working here",
/// and this is how long that claim covers.
///
/// Five minutes, matching the server's `ACTIVITY_WINDOW_MS` — the two must
/// agree or the same session reads as two different durations depending on who
/// was asked.
pub const ACTIVITY_WINDOW_MS: i64 = 5 * 60 * 1000;

/// How long a session was actually being worked on: the length of the UNION of
/// the [`ACTIVITY_WINDOW_MS`] windows its events open.
///
/// Sorted, that union has a closed form and needs no interval merging:
///
/// ```text
///   active_ms = WINDOW + Σ min(tᵢ₊₁ − tᵢ, WINDOW)
/// ```
///
/// The consequences are the point, and are the server's too:
///
/// * a burst of 40 events inside one minute counts ONCE, not 40 windows;
/// * two events three hours apart are two windows, not three hours — the gap
///   is idle, and idle is not work;
/// * a session with no placeable event is 0, not one free window.
///
/// `event_ms` is sorted in place; the caller's order is scratch. Pure
/// otherwise — no clock, no I/O.
#[must_use]
pub fn active_ms(event_ms: &mut [i64]) -> u64 {
    if event_ms.is_empty() {
        return 0;
    }
    event_ms.sort_unstable();
    let mut total = ACTIVITY_WINDOW_MS;
    for pair in event_ms.windows(2) {
        // Saturating: a transcript is untrusted input, and a wrapped duration
        // is a lie where a pinned one is merely a ceiling.
        total = total.saturating_add(pair[1].saturating_sub(pair[0]).min(ACTIVITY_WINDOW_MS));
    }
    total as u64
}

/// What one pull request cost the machine: tokens, and the time this device
/// spent working on it.
///
/// `mix` is the measurement: five disjoint classes, none of them pre-summed.
/// `equiv_tokens` is `mix.equiv_tokens()`, carried as a field because it is the
/// figure PRs are compared by — never a substitute for the classes, always
/// derivable from them.
///
/// Deliberately no dollars, and deliberately no blend of the two quantities.
/// Tokens and milliseconds are exact local facts; a price is a contract with a
/// vendor that this device does not hold, and a score that mixed them would be
/// this crate deciding what a team should value.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrSpend {
    /// `org/repo`, as first seen. Matched case-insensitively.
    pub slug: String,
    pub pr_number: u64,
    /// The raw classes, exactly as measured.
    pub mix: TokenMix,
    /// `mix.equiv_tokens()` — the comparable token figure. See [`W_INPUT`].
    pub equiv_tokens: f64,
    /// This PR's share of the contributing sessions' [`active_ms`], split by
    /// the identical weights that split `mix`. Idle gaps are already excluded.
    pub active_ms: u64,
    /// Sessions that contributed, including ones split across several PRs.
    pub session_count: u32,
    /// Token-weighted mean of the contributing matches' confidences — how much
    /// of this figure rests on a measured file overlap rather than on a PR
    /// number somebody typed.
    ///
    /// Above [`CONFIDENCE_MENTION_ONLY`] the figure is backed by file overlap;
    /// at or below it, by mentions alone. The weight is each match's own share
    /// of the spend, so a session that gave this PR a tenth of its tokens
    /// speaks a tenth as loudly — there is deliberately no further discount for
    /// a split session, which would discount that dilution twice. See
    /// [`CONFIDENCE_OVERLAP_AND_REFERENCE`] for the layers.
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
    /// Tokens from sessions that resolved to no PR, as a full mix — so device
    /// coverage can be read in the same units as any PR.
    pub unattributed: TokenMix,
    /// The same sessions' [`active_ms`]. Reported for the same reason the
    /// tokens are: time nobody can attribute is the honest size of what these
    /// per-PR figures do not cover.
    pub unattributed_active_ms: u64,
    pub unattributed_sessions: u32,
    pub sessions_scanned: u32,
}

/// A reference the repo's own git state produced — a branch, a remote, a commit.
///
/// These four rank how reliably the REFERENCE ITSELF was detected. They are no
/// longer the attribution confidence — provenance of a mention says nothing
/// about who wrote the PR, which is the whole defect this module was rebuilt to
/// fix — but they still weight the split among mention-only matches, so a
/// git-deterministic mention outweighs one a model re-read out of prose.
pub const CONFIDENCE_GIT: f64 = 1.0;
/// A reference a tool invocation named (a `gh pr` call, a checkout). See
/// [`CONFIDENCE_GIT`].
pub const CONFIDENCE_TOOL: f64 = 0.8;
/// A reference mined out of turn text — a PR URL somebody pasted or wrote. See
/// [`CONFIDENCE_GIT`].
pub const CONFIDENCE_CONTENT: f64 = 0.6;
/// A reference an on-device model reported. Weakest: re-parsed free text. See
/// [`CONFIDENCE_GIT`].
pub const CONFIDENCE_MODEL: f64 = 0.4;

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
/// directory, a transcript a parser chokes on, a repo that is not on this disk,
/// no `$HOME` — each degrades to less data, never to an error and never to a
/// panic. An empty [`SpendSummary`] is a truthful answer for a machine with no
/// session logs.
///
/// Bounded: only transcripts written within the window are opened (an older
/// file cannot hold a newer event), every parse streams in ≤256-event chunks
/// and keeps nothing but per-session counters, events outside the window are
/// dropped as they arrive, and every git read the join makes is capped, timed
/// out and memoised for the length of the call — see `RepoIndex`.
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

    finish(sessions, &mut RepoIndex::reading_git())
}

/// [`spend_by_pr`]'s pure core: the same aggregation over events a caller
/// already holds, with no clock, no filesystem and no window (the caller's
/// slice IS the window). This is where the split, the confidence and the
/// unattributed accounting live.
///
/// The file-overlap half of the join needs a repo on disk, so this path runs
/// with an offline index and every attribution it makes is mention-only. That
/// is the honest reading, not a degraded one: with no repo to measure against
/// there is no overlap to find. `join`, [`file_overlap`] and [`time_proximity`]
/// are the pure functions to exercise the measured layers.
#[must_use]
pub fn spend_by_pr_events(events: &[RawEvent]) -> SpendSummary {
    let mut sessions: BTreeMap<String, SessionAcc> = BTreeMap::new();
    for e in events {
        fold_event(&mut sessions, e);
    }
    finish(sessions, &mut RepoIndex::offline())
}

/// One session, reduced to what attribution needs. Never holds an event.
#[derive(Default)]
struct SessionAcc {
    mix: TokenMix,
    /// The reference blobs this session's turns carried, folded once at the end.
    parts: Vec<DetectedRefs>,
    /// Epoch-ms of the first and last timestamped turn: the window the
    /// changed-file capture reads, and the instant the merge-time decay is
    /// measured from.
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    /// Epoch-ms of every placeable turn, unsorted — the windows [`active_ms`]
    /// unions. Timestamps only: eight bytes a turn, and no event survives here.
    ///
    /// ponytail: unsorted and uncollapsed, so a session costs 8 bytes per turn
    /// until `finish` drops it. Upgrade path if a pathological transcript ever
    /// justifies it: sort and drop any point whose neighbours span
    /// `<= ACTIVITY_WINDOW_MS`, which is exact — a cluster inside one window
    /// contributes only its endpoints.
    stamps: Vec<i64>,
    /// The working directories the session's turns reported, deduped. LOCAL
    /// ONLY — used to find the repo on disk and never returned in any shape.
    cwds: Vec<String>,
}

/// Collapse a session's accumulated reference parts once they pass this many.
///
/// ponytail: a fixed memory ceiling per session — a 50k-turn transcript would
/// otherwise hold 50k blobs. `dedupe_session_metadata` is a fold, so collapsing
/// early loses nothing except applying its own 100-PR cap sooner.
const PARTS_COLLAPSE_AT: usize = 512;

/// Distinct working directories kept per session. An agent that wanders is
/// still one session; past a handful of directories the extra ones buy no
/// repos and cost a git resolve each.
const CWDS_PER_SESSION_MAX: usize = 4;

/// Add one event's tokens, references, window and working directory to its
/// session.
///
/// Only the channels that can yield a [`PullRequestRef`](modelstat_parsers::PullRequestRef)
/// are read for references. The daemon's pass also folds each event's git
/// context and branch tickets, but those produce repos and issue keys — never a
/// PR — so they cannot move an attribution and are not paid for here.
fn fold_event(sessions: &mut BTreeMap<String, SessionAcc>, e: &RawEvent) {
    let acc = sessions.entry(e.session_id.clone()).or_default();

    if let Some(t) = &e.tokens {
        // Class for class. Summing any two of them here is what this module
        // used to do, and what it must never do again.
        acc.mix.add(TokenMix::from(t));
    }

    // Parsed, not string-compared: a transcript may stamp a local offset, and
    // ISO-8601 only sorts lexically within one offset. A turn we cannot place in
    // time moves neither the window nor the clock — it opens no activity window
    // rather than opening one at an invented instant.
    if let Some(ms) = parse_iso_ms(&e.ts) {
        acc.start_ms = Some(acc.start_ms.map_or(ms, |s| s.min(ms)));
        acc.end_ms = Some(acc.end_ms.map_or(ms, |s| s.max(ms)));
        acc.stamps.push(ms);
    }

    if let Some(cwd) = e.cwd.as_deref().filter(|c| !c.is_empty()) {
        if acc.cwds.len() < CWDS_PER_SESSION_MAX && !acc.cwds.iter().any(|c| c == cwd) {
            acc.cwds.push(cwd.to_string());
        }
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
    active_ms: u64,
    sessions: u32,
    conf_weighted: f64,
    weight: u64,
    conf_sum: f64,
}

/// Turn per-session counters into the summary: join, split, sort.
fn finish(sessions: BTreeMap<String, SessionAcc>, repos: &mut RepoIndex) -> SpendSummary {
    let mut out = SpendSummary::default();
    // Keyed on the lowercased slug — forge slugs are case-insensitive, and two
    // spellings of one repo must not read as two repos.
    let mut acc: BTreeMap<(String, u64), PrAcc> = BTreeMap::new();

    // Learn every repo an agent has worked in on this device BEFORE joining any
    // session, so a session that ran outside a repo — the ordinary shape for a
    // harness driving subagents from a parent directory — can still be measured
    // against the repo it names. Order matters: a later session's cwd is what
    // teaches an earlier session where its repo lives.
    for session in sessions.values() {
        repos.learn(&session.cwds);
    }

    for (_session_id, mut session) in sessions {
        out.sessions_scanned = out.sessions_scanned.saturating_add(1);
        // A property of the session itself, measured before anything is known
        // about what it shipped.
        let session_active = active_ms(&mut session.stamps);
        let refs = session_prs(session.parts);
        let windows = repos.windows(&session.cwds, &refs, session.start_ms, session.end_ms);
        let matches = join(session.end_ms.unwrap_or(0), &windows, &refs);

        if matches.is_empty() {
            out.unattributed.add(session.mix);
            out.unattributed_active_ms = out.unattributed_active_ms.saturating_add(session_active);
            out.unattributed_sessions = out.unattributed_sessions.saturating_add(1);
            continue;
        }

        let scores: Vec<f64> = matches.iter().map(|m| m.score).collect();
        // Sanitized ONCE and used for both quantities. Time and tokens
        // disagreeing about which PR a session belongs to would make every
        // "tokens and time" reading of a row unanswerable.
        let (w, sum) = split_weights(&scores);
        let shares = session.mix.split_with(&w, sum);
        let time_shares = apportion(session_active, &w, sum);

        for ((m, share), active) in matches.into_iter().zip(shares).zip(time_shares) {
            let entry = acc.entry((m.slug.to_lowercase(), m.number)).or_default();
            if entry.slug.is_empty() {
                entry.slug = m.slug;
            }
            entry.mix.add(share);
            entry.active_ms = entry.active_ms.saturating_add(active);
            entry.sessions = entry.sessions.saturating_add(1);
            // Weighted by the RAW volume of THIS PR's share, which is what
            // makes a further "divided session" discount wrong: a session that
            // gave this PR a tenth of its tokens already speaks a tenth as
            // loudly here. Scaling the confidence by the share as well — as an
            // earlier cut did, and as the flat SPLIT_PENALTY before it did —
            // discounts the same dilution twice, and drags a PR whose evidence
            // is a measured file overlap down among the bare mentions. The
            // layer's confidence is a statement about EVIDENCE; how much of the
            // spend rests on it is what the weight says.
            let weight = share.raw_total();
            entry.weight = entry.weight.saturating_add(weight);
            entry.conf_weighted += m.confidence * weight as f64;
            entry.conf_sum += m.confidence;
        }
    }

    out.by_pr = acc
        .into_iter()
        .map(|((_, pr_number), a)| PrSpend {
            slug: a.slug,
            pr_number,
            mix: a.mix,
            equiv_tokens: a.mix.equiv_tokens(),
            active_ms: a.active_ms,
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

/// The PRs one session NAMED, as `(slug, number, reference confidence)`.
///
/// A mention is one of the two signals the join layers, never the whole answer
/// on its own: see `join`. The confidence is [`source_confidence`] of the
/// parsers' own rank, and is used to weight the split among mention-only
/// matches — not as the attribution confidence, which the join decides.
///
/// A PR with no slug is dropped: it names a number on an unknown repo, which
/// cannot be joined to anything, and a session left with only those resolves to
/// no PR by reference at all.
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

// ── The join: which PR did this session actually author? ─────────────────────

/// Commits land a little AFTER the talking stops, so the changed-file capture
/// extends the session's window by this much. Mirrors the daemon's own
/// `session_metadata::COMMIT_GRACE_MS`: 4h covers "commit when you're done".
pub const COMMIT_GRACE_MS: i64 = 4 * 60 * 60 * 1000;

/// How long after a session a merge can still be that session's work.
///
/// The asymmetry is the whole point. A PR is opened and merged AFTER its branch
/// is written, never before, so the candidate window runs FORWARD from the
/// session's start; two weeks covers an ordinary review cycle.
pub const JOIN_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// [`time_proximity`] at exactly [`JOIN_WINDOW_MS`]. A same-day merge is 1.0.
pub const TIME_DECAY_AT_WINDOW: f64 = 0.3;

/// Jaccard overlap below which a match is not a match.
///
/// Two large changesets sharing one incidental file — a lockfile, a shared
/// constant, the module everybody edits — is coincidence, and coincidence is
/// what the reference-only join was already full of.
pub const MIN_FILE_OVERLAP: f64 = 0.15;

/// At or above this overlap the session plainly did the PR's work.
pub const STRONG_FILE_OVERLAP: f64 = 0.5;

/// File overlap AND an explicit reference to the same PR: two independent
/// signals agreeing, the strongest claim this module can make.
pub const CONFIDENCE_OVERLAP_AND_REFERENCE: f64 = 1.0;

/// Strong file overlap and no reference at all. Common, and correct: the PR did
/// not exist yet while the session was writing it.
pub const CONFIDENCE_STRONG_OVERLAP: f64 = 0.9;

/// Ceiling of the weak-overlap band — an overlap above [`MIN_FILE_OVERLAP`] but
/// below [`STRONG_FILE_OVERLAP`], scaled by the overlap from
/// [`CONFIDENCE_MENTION_ONLY`] up to here.
pub const CONFIDENCE_WEAK_OVERLAP_MAX: f64 = 0.6;

/// A PR the session NAMED and whose files it did not touch.
///
/// This is the case that produced the inverted output this module was rebuilt
/// to fix, so it is deliberately the weakest layer that still attributes
/// anything. A session that mentions a PR number and changes none of its files
/// is, on the evidence, DISCUSSING that PR — reviewing it, quoting it, being
/// asked to look at it — not authoring it. The spend is still reported rather
/// than dropped, because it was really spent; it is reported as the thin claim
/// it is. Flat, whatever the reference's provenance: how reliably a mention was
/// DETECTED says nothing about who wrote the code.
pub const CONFIDENCE_MENTION_ONLY: f64 = 0.3;

/// Split weight a mention-only match carries before [`source_confidence`]
/// scales it. Deliberately the overlap floor: the weakest thing that counts as
/// a match at all, so any measured overlap outweighs any number of bare
/// mentions.
const MENTION_ONLY_SCORE: f64 = MIN_FILE_OVERLAP;

/// One merged PR a session could have authored. `files` is local only.
#[derive(Clone)]
struct PrCandidate {
    number: u64,
    merged_at_ms: i64,
    files: Rc<BTreeSet<String>>,
}

/// One repo a session can be measured against: the slug PRs are keyed on, what
/// changed in the repo during the session's window, and the PRs that merged in
/// the join window. Local only — neither file set leaves this module.
#[derive(Clone)]
struct RepoWindow {
    slug: String,
    files: Rc<BTreeSet<String>>,
    candidates: Vec<PrCandidate>,
}

/// One (session, PR) pair the join accepted.
struct PrMatch {
    slug: String,
    number: u64,
    /// Overlap × time proximity for a measured match, a floor value for a bare
    /// mention. Drives the proportional split and the share-scaled confidence;
    /// never returned.
    score: f64,
    confidence: f64,
}

/// Jaccard index of two changed-file sets — `|A ∩ B| / |A ∪ B|`. Pure.
///
/// Symmetric on purpose. Containment (`|A ∩ B| / |B|`) would score every small
/// PR merged inside a long session's window as a perfect match, which is how a
/// week-long session ends up claiming every PR that landed while it ran.
#[must_use]
pub fn file_overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    if intersection == 0 {
        return 0.0;
    }
    // |A ∪ B| = |A| + |B| - |A ∩ B|; the intersection is bounded by both sizes,
    // so this cannot underflow.
    intersection as f64 / (a.len() + b.len() - intersection) as f64
}

/// How much a merge `gap_ms` after a session ended still looks like that
/// session's work: 1.0 at or before the session's end, decaying exponentially
/// to [`TIME_DECAY_AT_WINDOW`] at [`JOIN_WINDOW_MS`]. Pure.
#[must_use]
pub fn time_proximity(gap_ms: i64) -> f64 {
    if gap_ms <= 0 {
        return 1.0;
    }
    // tau solves exp(-window / tau) = TIME_DECAY_AT_WINDOW.
    let tau = JOIN_WINDOW_MS as f64 / -TIME_DECAY_AT_WINDOW.ln();
    (-(gap_ms as f64) / tau).exp()
}

/// Layer the signals, strongest first, into the PRs one session is charged for.
/// Pure — every git read has already happened.
///
/// ```text
///   file overlap AND a reference  → CONFIDENCE_OVERLAP_AND_REFERENCE  (1.0)
///   strong file overlap alone     → CONFIDENCE_STRONG_OVERLAP         (0.9)
///   weak-but-above-floor overlap  → scaled by overlap, ceiling
///                                   CONFIDENCE_WEAK_OVERLAP_MAX       (0.6)
///   a reference alone             → CONFIDENCE_MENTION_ONLY           (0.3)
///   nothing                       → no match; the caller reports the
///                                   session as unattributed
/// ```
fn join(end_ms: i64, windows: &[RepoWindow], refs: &[(String, u64, f64)]) -> Vec<PrMatch> {
    // Keyed on the lowercased slug for the same reason `finish` is: one repo,
    // however it was spelled, is one repo.
    let mut out: BTreeMap<(String, u64), PrMatch> = BTreeMap::new();

    for w in windows {
        if w.files.is_empty() {
            continue;
        }
        for c in &w.candidates {
            let overlap = file_overlap(&w.files, &c.files);
            if overlap < MIN_FILE_OVERLAP {
                continue;
            }
            let referenced = refs
                .iter()
                .any(|(slug, number, _)| *number == c.number && slug.eq_ignore_ascii_case(&w.slug));
            let confidence = if referenced {
                CONFIDENCE_OVERLAP_AND_REFERENCE
            } else if overlap >= STRONG_FILE_OVERLAP {
                CONFIDENCE_STRONG_OVERLAP
            } else {
                // The weak band starts where a bare mention ends: a measured
                // overlap, however thin, is a stronger claim than a name in a
                // sentence. That ordering IS the thesis of this join.
                CONFIDENCE_MENTION_ONLY
                    + (CONFIDENCE_WEAK_OVERLAP_MAX - CONFIDENCE_MENTION_ONLY)
                        * (overlap / STRONG_FILE_OVERLAP)
            };
            out.insert(
                (w.slug.to_lowercase(), c.number),
                PrMatch {
                    slug: w.slug.clone(),
                    number: c.number,
                    score: overlap * time_proximity(c.merged_at_ms.saturating_sub(end_ms)),
                    confidence,
                },
            );
        }
    }

    // Mentions the files did not corroborate. `or_insert_with` is load bearing:
    // a PR already matched by overlap keeps its measured score and confidence,
    // and the mention only raised it to 1.0 above.
    for (slug, number, source) in refs {
        out.entry((slug.to_lowercase(), *number))
            .or_insert_with(|| PrMatch {
                slug: slug.clone(),
                number: *number,
                score: MENTION_ONLY_SCORE * source,
                confidence: CONFIDENCE_MENTION_ONLY,
            });
    }

    out.into_values().collect()
}

/// The match scores as split weights, plus their sum. Pure.
///
/// Negative, non-finite and zero scores read as no weight; an all-zero set
/// falls back to an even split, because the caller asked for `n` shares and a
/// session's spend has to land somewhere.
///
/// One function, called once per session, because every quantity a session is
/// divided into MUST be divided by the same numbers. Tokens and `active_ms`
/// re-deriving "the same" weights independently is how they would eventually
/// stop being the same.
fn split_weights(weights: &[f64]) -> (Vec<f64>, f64) {
    let mut w: Vec<f64> = weights
        .iter()
        .map(|x| if x.is_finite() && *x > 0.0 { *x } else { 0.0 })
        .collect();
    let mut sum: f64 = w.iter().sum();
    if !(sum > 0.0) {
        w = vec![1.0; weights.len()];
        sum = weights.len() as f64;
    }
    (w, sum)
}

/// Split `total` into `weights.len()` whole units proportional to `weights`,
/// summing back to `total` EXACTLY. Pure.
///
/// Largest remainder: floor every exact share, then hand the leftover units to
/// the largest fractional parts, ties to the lower index so the answer is
/// deterministic. `sum` is `weights.iter().sum()`, passed in because one
/// session apportions five token classes AND its `active_ms` with it.
fn apportion(total: u64, weights: &[f64], sum: f64) -> Vec<u64> {
    let n = weights.len();
    if n <= 1 {
        return vec![total; n];
    }
    if total == 0 {
        return vec![0; n];
    }
    let mut out = Vec::with_capacity(n);
    let mut fractions: Vec<(f64, usize)> = Vec::with_capacity(n);
    let mut assigned: u64 = 0;
    for (i, w) in weights.iter().enumerate() {
        let exact = total as f64 * (w / sum);
        // f64 stops counting whole tokens above 2^53; clamping to what is left
        // keeps the invariant that no share exceeds the total, and the
        // remainder loop below hands back whatever the floor dropped.
        let (floor, fraction) = if exact.is_finite() && exact >= 0.0 {
            let f = exact.floor();
            ((f as u64).min(total - assigned), exact - f)
        } else {
            (0, 0.0)
        };
        assigned += floor;
        fractions.push((fraction, i));
        out.push(floor);
    }
    let mut rest = total - assigned;
    fractions.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    // `% n` rather than a bare index: `rest` is at most `n - 1` in exact
    // arithmetic, and this stays terminating and correct if float slop ever
    // leaves more.
    let mut k = 0usize;
    while rest > 0 {
        out[fractions[k % n].1] += 1;
        rest -= 1;
        k += 1;
    }
    out
}

// ── The git side: bounded, memoised repo reads ───────────────────────────────

/// First-parent commits walked per repo to enumerate merged PRs. Matches the
/// anchor miner's own window, so the two agree on which merges exist.
const MAX_HISTORY: &str = "2000";

/// The log format `parse_git_log` reads: sha, committer date, subject. The body
/// field it also accepts is deliberately NOT asked for — the join has no use for
/// a commit message, so none is ever read into this process.
const LOG_FORMAT: &str = "--format=%H\u{1f}%cI\u{1f}%s\u{1e}";

/// Stdout ceiling for one repo's first-parent walk (~150 bytes a commit).
const LOG_MAX_BYTES: usize = 512 * 1024;

/// Stdout ceiling for one PR's numstat. One row per file, so ~4k files.
const NUMSTAT_MAX_BYTES: usize = 256 * 1024;

/// Paths kept from one session window. A window that changed more files than
/// this is churn, not a PR, and the tail cannot move a Jaccard.
const SESSION_FILES_MAX: usize = 4_000;

/// Repos one session is measured against.
const REPOS_PER_SESSION_MAX: usize = 4;

/// Directories the sibling-repo probe looks in.
const SEARCH_DIRS_MAX: usize = 8;

/// Distinct slugs the sibling-repo probe will look up in one call. Most of what
/// reaches it is path-shaped noise from the reference miner, and the answer for
/// a slug is the same however many sessions ask for it.
const MAX_SLUG_PROBES: usize = 128;

/// Ceiling on one repo's merged-PR walk. It is the join's FIRST question about
/// a repo and the answer decides whether anything else is worth asking: a repo
/// that cannot list its own merges in a second cannot serve this join at all.
const HISTORY_TIMEOUT: Duration = Duration::from_millis(1_200);

/// Ceiling on one merged PR's numstat. A healthy repo answers in tens of
/// milliseconds; a partial clone that has to fetch the blobs first does not
/// answer at all, and waiting four seconds to find that out — once per
/// candidate PR — is how a bounded command stops being one.
const NUMSTAT_TIMEOUT: Duration = Duration::from_millis(1_200);

/// Ceiling on the affordability probe that precedes a repo's FIRST window read.
/// See [`RepoIndex::changed_files`].
const CANARY_TIMEOUT: Duration = Duration::from_millis(1_000);

/// A single git call for one repo taking longer than this drops that repo from
/// the rest of the join.
///
/// Measured, not guessed. On this device a 1,200-commit repo answers every
/// question in tens of milliseconds while seven repos beside it take SECONDS TO
/// MINUTES for the same reads — a date-ranged `--numstat` gives git no revision
/// bound, and a partial clone has to fetch blobs before it can diff at all.
/// Without the quarantine the first such repo eats the whole budget and every
/// repo after it, including the one the user asked about, silently degrades to
/// mention-only. A healthy repo never trips it.
const SLOW_REPO_CALL: Duration = Duration::from_millis(1_200);

/// Whole-join backstop. The per-repo quarantine is what actually keeps the
/// command responsive; this only bounds a machine pathological in some way the
/// quarantine does not model.
const JOIN_BUDGET: Duration = Duration::from_secs(30);

/// PR diffs read per call. The cache means this counts DISTINCT merges rather
/// than (session, PR) pairs.
const MAX_PR_DIFFS: usize = 400;

/// One repo's merged-PR index: the slug PRs are keyed on, and every merge the
/// first-parent walk reached as `(number, merge sha, merged-at ms)`.
struct RepoHistory {
    slug: String,
    prs: Vec<(u64, String, i64)>,
}

/// Every git read the join makes, bounded and memoised for one call.
///
/// The memoisation is not an optimisation, it is what makes the join viable: a
/// PR merged inside a busy fortnight is a candidate for dozens of sessions, and
/// one `git show` per (session, PR) pair on a repo with hundreds of PRs turns a
/// two-second command into a minute of subprocesses.
///
/// `RepoIndex::offline` is the same type with git switched off, which is what
/// keeps `spend_by_pr_events` hermetic.
struct RepoIndex {
    enabled: bool,
    resolver: GitResolver,
    /// repo root → its merged-PR index. `None` caches "not a repo this can
    /// read", so a dead root is probed once rather than once per session.
    repos: HashMap<String, Option<Rc<RepoHistory>>>,
    /// (repo root, merge sha) → the PR's changed paths.
    pr_files: HashMap<(String, String), Rc<BTreeSet<String>>>,
    /// (repo root, since ms, until ms) → the window's changed paths.
    window_files: HashMap<(String, i64, i64), Rc<BTreeSet<String>>>,
    /// lowercased slug → repo root, learned from the sessions' own cwds.
    by_slug: HashMap<String, String>,
    /// Directories a repo nobody `cd`-ed into might sit under.
    search_dirs: Vec<String>,
    /// Slugs already probed and not found, so the probe runs once each.
    missing: HashSet<String>,
    /// Roots quarantined by [`SLOW_REPO_CALL`].
    slow: HashSet<String>,
    /// Roots whose window read has already been probed for affordability.
    canaried: HashSet<String>,
    deadline: Instant,
    diffs_read: usize,
    /// Windows handed straight back instead of read from git, so `finish` — the
    /// composition of the join, the split and the confidence mean — is testable
    /// without a repo on disk.
    #[cfg(test)]
    seeded: Vec<RepoWindow>,
}

/// Append `s` if it is new and there is room. Small N, so a linear scan beats a
/// set plus its allocation.
fn push_unique(v: &mut Vec<String>, s: String, cap: usize) {
    if v.len() < cap && !v.contains(&s) {
        v.push(s);
    }
}

impl RepoIndex {
    fn reading_git() -> Self {
        Self::new(true)
    }

    /// No git at all: every session resolves by reference alone.
    fn offline() -> Self {
        Self::new(false)
    }

    /// Offline, but with the windows a session's repo WOULD have produced.
    #[cfg(test)]
    fn seeded(windows: Vec<RepoWindow>) -> Self {
        Self {
            seeded: windows,
            ..Self::new(false)
        }
    }

    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            resolver: GitResolver::new(),
            repos: HashMap::new(),
            pr_files: HashMap::new(),
            window_files: HashMap::new(),
            by_slug: HashMap::new(),
            search_dirs: Vec::new(),
            missing: HashSet::new(),
            slow: HashSet::new(),
            canaried: HashSet::new(),
            deadline: Instant::now() + JOIN_BUDGET,
            diffs_read: 0,
            #[cfg(test)]
            seeded: Vec::new(),
        }
    }

    /// Is there no point shelling out again at all?
    fn spent(&self) -> bool {
        !self.enabled || Instant::now() >= self.deadline
    }

    /// Is this repo still worth asking? See [`SLOW_REPO_CALL`].
    fn usable(&self, root: &str) -> bool {
        !self.spent() && !self.slow.contains(root)
    }

    /// Record what one git call for `root` cost, quarantining the repo when the
    /// call took as long as `limit` — which, for a call that timed out, is
    /// exactly what happened. Fed the elapsed time of EVERY git call including
    /// the failures: a timeout is the most informative measurement there is.
    fn charge(&mut self, root: &str, elapsed: Duration, limit: Duration) {
        if elapsed >= limit {
            self.slow.insert(root.to_string());
        }
    }

    /// Record where a session ran: the repo it sits in, keyed by slug, and the
    /// directories a sibling repo could sit in. Idempotent.
    ///
    /// Deliberately does NOT build the repo's history: this runs for every
    /// session before any of them is joined, and a first-parent walk of a repo
    /// no session turns out to need is a second of subprocess for nothing.
    /// [`Self::slug_at`] is one cached `git config` read instead.
    fn learn(&mut self, cwds: &[String]) {
        if !self.enabled {
            return;
        }
        for cwd in cwds {
            let path = Path::new(cwd);
            for dir in [Some(path), path.parent()].into_iter().flatten() {
                push_unique(
                    &mut self.search_dirs,
                    dir.to_string_lossy().into_owned(),
                    SEARCH_DIRS_MAX,
                );
            }
            let Some(root) = resolve_repo_root(Some(cwd)) else {
                continue;
            };
            if let Some(slug) = self.slug_at(&root) {
                self.by_slug.entry(slug.to_lowercase()).or_insert(root);
            }
        }
    }

    /// The remote slug of the repo at `root`, or `None`. One
    /// `git config --get remote.origin.url`, cached per path by the resolver
    /// itself — cheap enough to ask about a directory that turns out not to
    /// matter.
    fn slug_at(&mut self, root: &str) -> Option<String> {
        if !self.usable(root) {
            return None;
        }
        let started = Instant::now();
        let ctx = self.resolver.resolve(Some(root));
        self.charge(root, started.elapsed(), SLOW_REPO_CALL);
        ctx?.remote_slug
    }

    /// One repo's merged-PR index, read at most once per root.
    fn history(&mut self, root: &str) -> Option<Rc<RepoHistory>> {
        if let Some(hit) = self.repos.get(root) {
            return hit.clone();
        }
        let built = self.read_history(root).map(Rc::new);
        self.repos.insert(root.to_string(), built.clone());
        built
    }

    fn read_history(&mut self, root: &str) -> Option<RepoHistory> {
        // No remote slug ⇒ no key to attribute against ⇒ nothing to join. This
        // also runs first because it is the cheaper of the two reads, and it
        // quarantines a repo whose git is unresponsive before the walk.
        let slug = self.slug_at(root)?;
        if !self.usable(root) {
            return None;
        }
        let started = Instant::now();
        let log = run_git(
            &["log", "--first-parent", "-n", MAX_HISTORY, LOG_FORMAT],
            root,
            LOG_MAX_BYTES,
            HISTORY_TIMEOUT,
        );
        self.charge(root, started.elapsed(), HISTORY_TIMEOUT);
        let log = log?;
        // The parsers' own readers decide which subjects name a PR and keep the
        // newest occurrence of each, so this join and the anchor miner cannot
        // disagree about which merges exist. `select_anchor_commits` applies no
        // cap of its own; `AnchorConfig::default()` sets no date cutoff.
        let prs = select_anchor_commits(&parse_git_log(&log), &AnchorConfig::default())
            .into_iter()
            .filter_map(|(number, c)| Some((number, c.sha.clone(), parse_iso_ms(&c.committed_at)?)))
            .collect();
        Some(RepoHistory { slug, prs })
    }

    /// The repo root a slug lives at, or `None`.
    ///
    /// Learned roots first. Failing that the repo may simply sit NEXT TO a
    /// directory an agent has run in — a harness driving subagents from a parent
    /// directory never has the repo as its own cwd, which is the ordinary shape
    /// on a real machine — so `<known dir>/<repo name>` is probed and accepted
    /// ONLY when git confirms the remote slug. A name match alone would be a
    /// guess, and a guess here silently attributes one repo's spend to another.
    ///
    /// Every step is deliberately cheap, because most of what reaches here is
    /// not a repo at all: the reference miner also yields path-shaped "slugs"
    /// (`erpc/networks.go`, `plans/notes.md`) and a long session can name
    /// dozens. A missing `.git` costs one `stat`, a present one costs a cached
    /// `git config`, each slug is probed once, and the whole call is capped.
    fn root_for_slug(&mut self, slug: &str) -> Option<String> {
        let key = slug.to_lowercase();
        if let Some(hit) = self.by_slug.get(&key) {
            return Some(hit.clone());
        }
        if self.spent()
            || self.missing.len() >= MAX_SLUG_PROBES
            || !self.missing.insert(key.clone())
        {
            return None;
        }
        let name = key.rsplit('/').next().unwrap_or_default().to_string();
        if name.is_empty() {
            return None;
        }
        for dir in self.search_dirs.clone() {
            let candidate = Path::new(&dir).join(&name);
            if !candidate.join(".git").exists() {
                continue;
            }
            let candidate = candidate.to_string_lossy().into_owned();
            if self
                .slug_at(&candidate)
                .is_some_and(|s| s.eq_ignore_ascii_case(slug))
            {
                self.by_slug.insert(key, candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    /// The repos this session can be measured against, each carrying the files
    /// its window changed and the PRs that merged in the join window.
    fn windows(
        &mut self,
        cwds: &[String],
        refs: &[(String, u64, f64)],
        start_ms: Option<i64>,
        end_ms: Option<i64>,
    ) -> Vec<RepoWindow> {
        #[cfg(test)]
        if !self.seeded.is_empty() {
            return self.seeded.clone();
        }
        // A session nothing timestamped cannot be placed against a merge date,
        // and a window is the only thing the file capture can read.
        let (Some(start), Some(end)) = (start_ms, end_ms) else {
            return Vec::new();
        };
        if self.spent() {
            return Vec::new();
        }

        let mut roots: Vec<String> = Vec::new();
        for cwd in cwds {
            if let Some(r) = resolve_repo_root(Some(cwd)) {
                push_unique(&mut roots, r, REPOS_PER_SESSION_MAX);
            }
        }
        for (slug, _, _) in refs {
            if roots.len() >= REPOS_PER_SESSION_MAX {
                break;
            }
            if let Some(r) = self.root_for_slug(slug) {
                push_unique(&mut roots, r, REPOS_PER_SESSION_MAX);
            }
        }

        let until = end.saturating_add(COMMIT_GRACE_MS);
        let last_merge = end.saturating_add(JOIN_WINDOW_MS);
        let mut out = Vec::new();
        for root in roots {
            // Cheapest discriminator first, every time. The merged-PR walk is
            // one bounded `git log` per repo for the whole call; the candidate
            // shortlist off it is free; only then is the window read — which is
            // the expensive one, because `--since/--until` makes git traverse
            // — worth paying for.
            let Some(history) = self.history(&root) else {
                continue;
            };
            let shortlist: Vec<&(u64, String, i64)> = history
                .prs
                .iter()
                .filter(|(_, _, merged_ms)| *merged_ms >= start && *merged_ms <= last_merge)
                .collect();
            if shortlist.is_empty() {
                continue;
            }
            let files = self.changed_files(&root, start, until);
            if files.is_empty() {
                continue;
            }
            let mut candidates = Vec::new();
            for (number, sha, merged_ms) in shortlist {
                let pr_files = self.merge_files(&root, sha);
                if pr_files.is_empty() {
                    continue;
                }
                candidates.push(PrCandidate {
                    number: *number,
                    merged_at_ms: *merged_ms,
                    files: pr_files,
                });
            }
            if candidates.is_empty() {
                continue;
            }
            out.push(RepoWindow {
                slug: history.slug.clone(),
                files,
                candidates,
            });
        }
        out
    }

    /// The paths changed in `root` during `[since_ms, until_ms]`. Memoised.
    ///
    /// The parsers' own read, so this join and the daemon's `FileRef`s are built
    /// from exactly the same git call rather than two that could drift.
    ///
    /// Preceded, once per repo, by the SAME read through this module's own
    /// runner under a tighter cap. `collect_files_changed` stops at four
    /// seconds, and four seconds is the right ceiling for a daemon pass and the
    /// wrong one for a command a person is waiting on — especially multiplied by
    /// every session that names the repo. The probe answers in tens of
    /// milliseconds on a healthy repo; a repo that needs longer is retired by
    /// [`Self::charge`] and never asked again.
    fn changed_files(&mut self, root: &str, since_ms: i64, until_ms: i64) -> Rc<BTreeSet<String>> {
        let key = (root.to_string(), since_ms, until_ms);
        if let Some(hit) = self.window_files.get(&key) {
            return hit.clone();
        }
        let mut set = BTreeSet::new();
        if let (true, Some(since), Some(until)) =
            (self.usable(root), ms_to_iso(since_ms), ms_to_iso(until_ms))
        {
            if self.canaried.insert(root.to_string()) {
                let started = Instant::now();
                let _ = run_git(
                    &[
                        "-c",
                        "core.quotePath=false",
                        "log",
                        &format!("--since={since}"),
                        &format!("--until={until}"),
                        "--numstat",
                        "--no-renames",
                        "--format=%H",
                        "-n",
                        MAX_HISTORY,
                    ],
                    root,
                    NUMSTAT_MAX_BYTES,
                    CANARY_TIMEOUT,
                );
                self.charge(root, started.elapsed(), CANARY_TIMEOUT);
            }
            if self.usable(root) {
                let started = Instant::now();
                let changes = collect_files_changed(root, &since, &until);
                self.charge(root, started.elapsed(), SLOW_REPO_CALL);
                for c in changes
                    .unwrap_or_default()
                    .into_iter()
                    .take(SESSION_FILES_MAX)
                {
                    set.insert(c.path);
                }
            }
        }
        let files = Rc::new(set);
        self.window_files.insert(key, files.clone());
        files
    }

    /// One merged PR's changed paths, memoised per `(root, merge sha)` — THE
    /// cache the whole join rests on.
    ///
    /// An exhausted budget caches an empty set rather than retrying: a bounded
    /// command that degrades must degrade once, not once per session.
    fn merge_files(&mut self, root: &str, sha: &str) -> Rc<BTreeSet<String>> {
        let key = (root.to_string(), sha.to_string());
        if let Some(hit) = self.pr_files.get(&key) {
            return hit.clone();
        }
        let mut set = BTreeSet::new();
        if self.usable(root) && self.diffs_read < MAX_PR_DIFFS {
            self.diffs_read += 1;
            let started = Instant::now();
            // `--format=` is why no commit message is read. `-m --first-parent`
            // gives a true merge's diff against mainline, and is a no-op on a
            // squash merge's single parent — which is the case that matters,
            // since squashing is what made a sha-based join impossible.
            let out = run_git(
                &[
                    "-c",
                    "core.quotePath=false",
                    "show",
                    "--numstat",
                    "--format=",
                    "-m",
                    "--first-parent",
                    "--no-renames",
                    sha,
                ],
                root,
                NUMSTAT_MAX_BYTES,
                NUMSTAT_TIMEOUT,
            );
            self.charge(root, started.elapsed(), SLOW_REPO_CALL);
            for row in crate::diff::parse_numstat(out.as_deref().unwrap_or_default()) {
                set.insert(row.path.to_string());
            }
        }
        let files = Rc::new(set);
        self.pr_files.insert(key, files.clone());
        files
    }
}

/// Run `git` in `cwd`, reading at most `max_bytes` of stdout and killing the
/// child past `timeout`. `None` on spawn failure, timeout, or empty output — a
/// bad sha writes to stderr, which is `/dev/null`.
///
/// ponytail: a near-copy of `diff::run_git_bounded`, which is private to that
/// module. Upgrade path: promote one of the two the moment a third caller wants
/// it.
fn run_git(args: &[&str], cwd: &str, max_bytes: usize, timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(8 * 1024);
        let _ = (&mut stdout).take(max_bytes as u64).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
        }
    };
    let buf = reader.join().ok()?;
    (!timed_out && !buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned())
}

/// An ISO-8601 instant as epoch-ms, or `None` when it cannot be placed in time.
///
/// Parsed rather than compared as a string: a transcript may stamp a local
/// offset (`+02:00`), and ISO-8601 only sorts lexically within one offset.
fn parse_iso_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// Epoch-ms as an ISO-8601 instant. Only ever fed to `git --since/--until`.
fn ms_to_iso(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
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
            collect_jsonl(
                &PathBuf::from(&data_dir).join(sub),
                *depth,
                since_ms,
                &mut found,
            );
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
    parse_iso_ms(ts).is_some_and(|ms| ms >= since_ms)
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
            seq: None,
            started_at: None,
            first_token_at: None,
            source_event_id: format!("{session}_{input}_{output}"),
            ts: "2026-08-01T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: session.into(),
            actor_id: None,
            recipient_actor_id: None,
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
            reasoning_excerpt: None,
            reasoning_bytes: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    #[test]
    fn single_pr_session_attributes_every_token() {
        let s = spend_by_pr_events(&[
            ev(
                "s1",
                "working on https://github.com/acme/api/pull/42",
                100,
                10,
            ),
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
        // No repo to measure against, so this is a mention and reads as one.
        assert_eq!(pr.attribution_confidence, CONFIDENCE_MENTION_ONLY);
        assert_eq!(s.unattributed, TokenMix::default());
        assert_eq!(s.unattributed_sessions, 0);
        assert_eq!(s.sessions_scanned, 1);
    }

    #[test]
    fn two_mentioned_prs_split_evenly_when_no_files_corroborate() {
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
                // Neither PR's files were checked, so both are mentions. The
                // split halves the TOKENS, not the strength of the evidence.
                CONFIDENCE_MENTION_ONLY
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
    fn source_rank_is_a_split_weight_and_no_longer_the_confidence() {
        for (source, weight) in [
            ("git", CONFIDENCE_GIT),
            ("tool", CONFIDENCE_TOOL),
            ("content", CONFIDENCE_CONTENT),
            ("model", CONFIDENCE_MODEL),
            ("something_new", CONFIDENCE_MODEL),
        ] {
            assert_eq!(source_confidence(source), weight, "{source}");
            // However reliably the NUMBER was read, a mention the files do not
            // corroborate is still only a mention.
            let s = spend_by_pr_events(&[ev_sourced("s1", source, 10, 1)]);
            assert_eq!(s.by_pr.len(), 1, "{source}");
            assert_eq!(
                s.by_pr[0].attribution_confidence, CONFIDENCE_MENTION_ONLY,
                "{source}"
            );
        }
    }

    #[test]
    fn the_strongest_source_for_one_pr_survives_the_fold() {
        // The same PR seen from prose and from a git-sourced blob resolves to
        // the stronger provenance, as the parsers' own dedupe does.
        let git: DetectedRefs = serde_json::from_value(json!({
            "repos": [],
            "pull_requests": [{"slug": "acme/api", "number": 42, "source": "git", "confidence": 0.9}],
            "issues": [],
        }))
        .unwrap();
        let prose = detect_references("see https://github.com/acme/api/pull/42", "content");
        assert_eq!(
            session_prs(vec![prose, git]),
            vec![("acme/api".to_string(), 42, CONFIDENCE_GIT)]
        );
    }

    #[test]
    fn a_stronger_source_takes_a_bigger_share_of_a_split_mention() {
        // Neither PR is corroborated by files, so the only thing separating
        // them is how the mention was detected.
        let mut e = bare("s1", 1_000, 0);
        e.references = Some(json!({
            "repos": [],
            "pull_requests": [
                {"slug": "acme/api", "number": 1, "source": "git", "confidence": 0.9},
                {"slug": "acme/api", "number": 2, "source": "model", "confidence": 0.9},
            ],
            "issues": [],
        }));
        let s = spend_by_pr_events(&[e]);
        assert_eq!(s.by_pr.len(), 2);
        let git = s.by_pr.iter().find(|p| p.pr_number == 1).unwrap();
        let model = s.by_pr.iter().find(|p| p.pr_number == 2).unwrap();
        assert!(
            git.mix.input > model.mix.input,
            "git {} vs model {}",
            git.mix.input,
            model.mix.input
        );
        assert_eq!(git.mix.input + model.mix.input, 1_000);
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
        for class in [
            "input",
            "output",
            "cache_creation",
            "cache_read",
            "reasoning",
        ] {
            assert!(
                v["mix"][class].as_u64().is_some(),
                "{class} must survive into the JSON"
            );
        }
        assert_eq!(v["mix"]["cache_read"], 8);
        assert!(v["equiv_tokens"].as_f64().is_some());
        assert!(
            v["active_ms"].as_u64().is_some(),
            "time travels beside the tokens, never folded into them"
        );
    }

    #[test]
    fn excerpt_is_the_fallback_when_no_blob_was_stamped() {
        let mut e = bare("s1", 10, 1);
        e.content_excerpt = Some("fixes https://github.com/acme/api/pull/7".into());
        let s = spend_by_pr_events(&[e]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].pr_number, 7);
        assert_eq!(s.by_pr[0].attribution_confidence, CONFIDENCE_MENTION_ONLY);
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
        assert_eq!(s.by_pr[0].attribution_confidence, CONFIDENCE_MENTION_ONLY);
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
            assert!(
                !pr.slug.is_empty(),
                "an attributed PR always names its repo"
            );
            assert!(pr.pr_number >= 1);
            // The ranked figure is always recomputable from the raw classes
            // published beside it — never a number a reader cannot check.
            assert_equiv(pr.equiv_tokens, pr.mix.equiv_tokens());
            assert!(pr.equiv_tokens >= 0.0);
            assert!(pr.equiv_tokens <= pr.mix.raw_total() as f64 * W_OUTPUT);
            assert!(pr.session_count >= 1);
            assert!(
                pr.attribution_confidence > 0.0
                    && pr.attribution_confidence <= CONFIDENCE_OVERLAP_AND_REFERENCE,
                "confidence {} out of range",
                pr.attribution_confidence
            );
        }
        assert!(
            u32::try_from(s.by_pr.len()).unwrap_or(u32::MAX)
                <= s.sessions_scanned.saturating_mul(2)
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

    // ── the join: file overlap, time proximity, layering ────────────────────

    const DAY_MS: i64 = 86_400_000;

    /// A fixed instant to hang the synthetic windows off. The value is
    /// irrelevant — only the gaps to the merge dates matter.
    const T0: i64 = 1_800_000_000_000;

    fn files(paths: &[&str]) -> Rc<BTreeSet<String>> {
        Rc::new(paths.iter().map(|p| (*p).to_string()).collect())
    }

    /// One repo window with one candidate PR.
    fn one_window(
        slug: &str,
        session: &[&str],
        number: u64,
        pr: &[&str],
        merged_at_ms: i64,
    ) -> RepoWindow {
        RepoWindow {
            slug: slug.into(),
            files: files(session),
            candidates: vec![PrCandidate {
                number,
                merged_at_ms,
                files: files(pr),
            }],
        }
    }

    /// `spend_by_pr_events`, but with the repo windows git would have produced.
    fn spend_with(events: &[RawEvent], windows: Vec<RepoWindow>) -> SpendSummary {
        let mut sessions: BTreeMap<String, SessionAcc> = BTreeMap::new();
        for e in events {
            fold_event(&mut sessions, e);
        }
        finish(sessions, &mut RepoIndex::seeded(windows))
    }

    #[test]
    fn file_overlap_is_the_jaccard_index() {
        let a = files(&["a", "b", "c"]);
        assert_eq!(file_overlap(&a, &a), 1.0, "identical sets");
        assert_eq!(file_overlap(&a, &files(&["x", "y"])), 0.0, "disjoint sets");
        // Two shared out of four distinct.
        assert_eq!(file_overlap(&a, &files(&["b", "c", "d"])), 0.5);
        // A superset is NOT a perfect match — three shared out of five.
        assert!((file_overlap(&a, &files(&["a", "b", "c", "d", "e"])) - 0.6).abs() < 1e-12);
        // Nothing to compare against is not a match, it is no measurement.
        assert_eq!(file_overlap(&a, &files(&[])), 0.0);
        assert_eq!(file_overlap(&files(&[]), &files(&[])), 0.0);
    }

    #[test]
    fn time_proximity_decays_from_one_to_the_window_floor() {
        assert_eq!(time_proximity(0), 1.0, "merged as the session ended");
        assert_eq!(time_proximity(-DAY_MS), 1.0, "merged during the session");
        assert!((time_proximity(JOIN_WINDOW_MS) - TIME_DECAY_AT_WINDOW).abs() < 1e-9);
        let gaps = [0, DAY_MS, 3 * DAY_MS, 7 * DAY_MS, JOIN_WINDOW_MS];
        for w in gaps.windows(2) {
            assert!(
                time_proximity(w[0]) > time_proximity(w[1]),
                "not monotone at {}..{}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn a_distant_merge_scores_below_a_same_day_one() {
        let paths = ["a.rs", "b.rs"];
        let near = join(T0, &[one_window("acme/api", &paths, 1, &paths, T0)], &[]);
        let far = join(
            T0,
            &[one_window("acme/api", &paths, 1, &paths, T0 + 12 * DAY_MS)],
            &[],
        );
        assert_eq!((near.len(), far.len()), (1, 1));
        assert!(
            far[0].score < near[0].score * 0.5,
            "{} vs {}",
            far[0].score,
            near[0].score
        );
        // Recency moves the SHARE, never the strength of the evidence: the same
        // files changed either way.
        assert_eq!(far[0].confidence, near[0].confidence);
    }

    #[test]
    fn the_floor_rejects_one_shared_file_between_large_changesets() {
        // Twenty files each, one in common — the lockfile every PR touches.
        let session: BTreeSet<String> = (0..20).map(|i| format!("src/s{i}.rs")).collect();
        let mut pr: BTreeSet<String> = (0..19).map(|i| format!("src/p{i}.rs")).collect();
        pr.insert("src/s0.rs".into());
        let overlap = file_overlap(&session, &pr);
        assert!(overlap < MIN_FILE_OVERLAP, "coincidence scored {overlap}");
        let w = RepoWindow {
            slug: "acme/api".into(),
            files: Rc::new(session),
            candidates: vec![PrCandidate {
                number: 1,
                merged_at_ms: T0,
                files: Rc::new(pr),
            }],
        };
        assert!(join(T0, &[w], &[]).is_empty(), "coincidence became a match");
    }

    #[test]
    fn layers_run_strongest_first() {
        let paths = ["a.rs", "b.rs", "c.rs", "d.rs"];
        let refs = vec![("acme/api".to_string(), 7, CONFIDENCE_CONTENT)];

        // 1. the files AND the transcript agree.
        let m = join(T0, &[one_window("acme/api", &paths, 7, &paths, T0)], &refs);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].confidence, CONFIDENCE_OVERLAP_AND_REFERENCE);

        // 2. the files alone — the ordinary shape, because the PR did not exist
        //    yet while the session was writing it.
        let m = join(T0, &[one_window("acme/api", &paths, 9, &paths, T0)], &[]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].confidence, CONFIDENCE_STRONG_OVERLAP);

        // 3. the transcript alone: PR 9's files are untouched, and the PR the
        //    session NAMES is not a candidate at all.
        let m = join(
            T0,
            &[one_window("acme/api", &paths, 9, &["z.rs"], T0)],
            &refs,
        );
        assert_eq!(m.len(), 1, "an unshared file must not match");
        assert_eq!((m[0].number, m[0].confidence), (7, CONFIDENCE_MENTION_ONLY));
    }

    #[test]
    fn a_weak_overlap_lands_between_a_mention_and_a_strong_match() {
        let owned: Vec<String> = (0..10).map(|i| format!("s{i}.rs")).collect();
        let session: Vec<&str> = owned.iter().map(String::as_str).collect();
        // Two of the session's ten files plus one of its own: 2 shared, 11
        // distinct — above the floor, well below strong.
        let thin = ["s0.rs", "s1.rs", "z.rs"];
        // Five of the ten: still weak, but twice the evidence.
        let thick = ["s0.rs", "s1.rs", "s2.rs", "s3.rs", "s4.rs", "z.rs"];

        let m = join(T0, &[one_window("acme/api", &session, 1, &thin, T0)], &[]);
        assert_eq!(m.len(), 1);
        let thin_conf = m[0].confidence;
        let m = join(T0, &[one_window("acme/api", &session, 1, &thick, T0)], &[]);
        assert_eq!(m.len(), 1);
        let thick_conf = m[0].confidence;

        for c in [thin_conf, thick_conf] {
            assert!(
                c > CONFIDENCE_MENTION_ONLY && c < CONFIDENCE_WEAK_OVERLAP_MAX,
                "weak-band confidence {c} escaped its band"
            );
        }
        // Scaled by the overlap, so more shared files reads stronger.
        assert!(thin_conf < thick_conf);
    }

    #[test]
    fn disjoint_file_sets_leave_the_session_unattributed() {
        let w = one_window("acme/api", &["a.rs"], 1, &["b.rs"], T0);
        let s = spend_with(&[bare("s1", 100, 10)], vec![w]);
        assert!(s.by_pr.is_empty());
        assert_eq!(s.unattributed.raw_total(), 110);
        assert_eq!(s.unattributed_sessions, 1);
        assert_eq!(s.sessions_scanned, 1);
    }

    // ── the split ───────────────────────────────────────────────────────────

    #[test]
    fn proportional_shares_sum_exactly_to_the_original_in_every_class() {
        let mix = TokenMix {
            input: 101,
            output: 7,
            cache_creation: 1_000,
            cache_read: 65_537,
            reasoning: 2,
        };
        for weights in [
            vec![1.0],
            vec![1.0, 1.0, 1.0],
            vec![0.7, 0.2, 0.1],
            vec![0.93, 0.07],
            vec![3.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            // Degenerate weights must still return n shares that add up.
            vec![0.0, 0.0],
            vec![f64::NAN, 1.0],
            vec![-1.0, 2.0],
        ] {
            let (w, sum) = split_weights(&weights);
            let shares = mix.split_with(&w, sum);
            assert_eq!(shares.len(), weights.len(), "weights {weights:?}");
            let mut summed = TokenMix::default();
            for s in &shares {
                summed.add(*s);
            }
            assert_eq!(summed, mix, "weights {weights:?} lost a class");
        }
    }

    #[test]
    fn a_bigger_score_takes_a_bigger_share() {
        let mix = TokenMix {
            input: 1_000,
            ..TokenMix::default()
        };
        let (w, sum) = split_weights(&[0.9, 0.1]);
        let shares = mix.split_with(&w, sum);
        assert_eq!((shares[0].input, shares[1].input), (900, 100));
        // An even split is still exact, and its remainder is deterministic.
        let (w, sum) = split_weights(&[1.0, 1.0, 1.0]);
        let shares = TokenMix {
            input: 10,
            ..TokenMix::default()
        }
        .split_with(&w, sum);
        assert_eq!(
            shares.iter().map(|s| s.input).collect::<Vec<_>>(),
            vec![4, 3, 3]
        );
    }

    // ── time: the union of activity windows ─────────────────────────────────

    /// The window one event opens, as the tests read it.
    const W: i64 = ACTIVITY_WINDOW_MS;

    /// The same event, stamped at a given instant.
    fn at(mut e: RawEvent, ts: &str) -> RawEvent {
        e.ts = ts.into();
        e
    }

    #[test]
    fn a_session_with_no_events_is_zero_not_one_free_window() {
        let mut empty: Vec<i64> = Vec::new();
        assert_eq!(active_ms(&mut empty), 0);
    }

    #[test]
    fn a_single_event_is_one_window() {
        assert_eq!(active_ms(&mut [1_000_000]), W as u64);
    }

    #[test]
    fn events_inside_one_window_count_once() {
        // A minute apart: one window plus that minute, NOT two windows.
        assert_eq!(active_ms(&mut [0, 60_000]), (W + 60_000) as u64);
        // Forty events inside one minute are that same one-minute span.
        let mut burst: Vec<i64> = (0..40).map(|i| i * 1_500).collect();
        assert_eq!(active_ms(&mut burst), (W + 58_500) as u64);
        // Repeated identical stamps add nothing at all.
        assert_eq!(active_ms(&mut [7, 7, 7]), W as u64);
    }

    #[test]
    fn a_three_hour_gap_is_two_windows_not_three_hours() {
        // The whole reason this is not `ended_at - started_at`: nobody worked
        // through the gap, so the gap is not work.
        assert_eq!(active_ms(&mut [0, 3 * 60 * 60 * 1000]), (2 * W) as u64);
    }

    #[test]
    fn out_of_order_stamps_measure_the_same_span() {
        // Transcripts interleave; the union is a set operation and cannot care.
        let ordered = active_ms(&mut [0, 60_000, 3 * W]);
        assert_eq!(ordered, (W + 60_000 + W) as u64);
        assert_eq!(active_ms(&mut [3 * W, 60_000, 0]), ordered);
    }

    #[test]
    fn one_weight_vector_divides_tokens_and_time_alike() {
        // The contract in one assertion: both quantities go through the same
        // sanitized weights, so a PR's time and its tokens describe the same
        // fraction of the same session.
        let (w, sum) = split_weights(&[0.9, 0.1]);
        let shares = TokenMix {
            input: 1_000,
            ..TokenMix::default()
        }
        .split_with(&w, sum);
        assert_eq!((shares[0].input, shares[1].input), (900, 100));
        assert_eq!(apportion(1_000_000, &w, sum), vec![900_000, 100_000]);
    }

    #[test]
    fn a_sessions_time_lands_on_the_pr_it_worked_on() {
        let s = spend_by_pr_events(&[
            at(
                ev("s1", "https://github.com/acme/api/pull/42", 100, 10),
                "2026-08-01T10:00:00.000Z",
            ),
            at(ev("s1", "still on it", 50, 5), "2026-08-01T10:01:00.000Z"),
        ]);
        assert_eq!(s.by_pr.len(), 1);
        assert_eq!(s.by_pr[0].active_ms, (W + 60_000) as u64);
        assert_eq!(s.unattributed_active_ms, 0);
    }

    #[test]
    fn a_split_session_splits_its_time_the_way_it_splits_its_tokens() {
        let s = spend_by_pr_events(&[
            at(
                ev(
                    "s1",
                    "landing https://github.com/acme/api/pull/1 then https://github.com/acme/api/pull/2",
                    100,
                    0,
                ),
                "2026-08-01T10:00:00.000Z",
            ),
            at(ev("s1", "wrap up", 100, 0), "2026-08-01T13:00:00.000Z"),
        ]);
        assert_eq!(s.by_pr.len(), 2);
        // Two windows three hours apart, divided by the same even weights the
        // 200 input tokens are.
        for p in &s.by_pr {
            assert_eq!(p.mix.input, 100, "{p:?}");
            assert_eq!(p.active_ms, W as u64, "{p:?}");
        }
    }

    #[test]
    fn time_nobody_can_attribute_is_reported_rather_than_dropped() {
        let s = spend_by_pr_events(&[
            at(bare("s1", 10, 1), "2026-08-01T10:00:00.000Z"),
            at(bare("s1", 20, 2), "2026-08-01T14:00:00.000Z"),
        ]);
        assert!(s.by_pr.is_empty());
        assert_eq!(s.unattributed_active_ms, (2 * W) as u64);
        assert_eq!(s.unattributed_sessions, 1);
    }

    #[test]
    fn every_millisecond_is_attributed_or_reported_unattributed() {
        let s = spend_by_pr_events(&[
            at(
                ev("s1", "https://github.com/acme/api/pull/1", 10, 1),
                "2026-08-01T10:00:00.000Z",
            ),
            at(
                ev("s2", "https://github.com/acme/api/pull/2", 10, 1),
                "2026-08-01T11:00:00.000Z",
            ),
            at(bare("s3", 10, 1), "2026-08-01T12:00:00.000Z"),
        ]);
        let attributed: u64 = s.by_pr.iter().map(|p| p.active_ms).sum();
        assert_eq!(attributed + s.unattributed_active_ms, (3 * W) as u64);
    }

    #[test]
    fn an_unplaceable_turn_opens_no_window() {
        // A turn we cannot place in time is unknown, and unknown must not read
        // as five free minutes of work.
        let s = spend_by_pr_events(&[
            at(
                ev("s1", "https://github.com/acme/api/pull/42", 10, 1),
                "not a timestamp",
            ),
            at(ev("s1", "more", 10, 1), "2026-08-01T10:00:00.000Z"),
        ]);
        assert_eq!(s.by_pr[0].active_ms, W as u64);
    }

    // ── the regression this whole module was rebuilt for ────────────────────

    #[test]
    fn an_authored_pr_outranks_one_the_session_merely_mentioned() {
        // The defect in miniature. One session, two erpc PRs: 1016 is the one it
        // actually wrote — its window changed exactly that PR's files — and 1031
        // is one somebody asked it to look at. The reference-only join gave 1016
        // nothing and 1031 the entire session.
        let authored = [
            "consensus/executor.go",
            "consensus/policy.go",
            "common/config.go",
        ];
        let events = [ev(
            "s1",
            "while you are in there, look at https://github.com/erpc/erpc/pull/1031",
            1_000,
            0,
        )];
        let end = parse_iso_ms(&events[0].ts).unwrap();

        // Before: the mention took everything and the authored PR did not exist.
        let before = spend_by_pr_events(&events);
        assert_eq!(before.by_pr.len(), 1);
        assert_eq!(before.by_pr[0].pr_number, 1031);
        assert_eq!(before.by_pr[0].mix.input, 1_000);

        let after = spend_with(
            &events,
            vec![RepoWindow {
                slug: "erpc/erpc".into(),
                files: files(&authored),
                candidates: vec![
                    PrCandidate {
                        number: 1016,
                        merged_at_ms: end + 2 * DAY_MS,
                        files: files(&authored),
                    },
                    PrCandidate {
                        number: 1031,
                        merged_at_ms: end + DAY_MS,
                        files: files(&["docs/readme.md", "scripts/release.sh"]),
                    },
                ],
            }],
        );

        assert_eq!(after.by_pr.len(), 2);
        // Ranked by spend, and the authored PR is now first.
        assert_eq!(after.by_pr[0].pr_number, 1016);
        let authored_pr = &after.by_pr[0];
        let mentioned = after.by_pr.iter().find(|p| p.pr_number == 1031).unwrap();

        assert!(
            authored_pr.mix.input > 8 * mentioned.mix.input,
            "authored {} vs mentioned {}",
            authored_pr.mix.input,
            mentioned.mix.input
        );
        // Still exact: every token is somewhere.
        assert_eq!(authored_pr.mix.input + mentioned.mix.input, 1_000);
        // And the evidence reads for what it is on both rows: a measured file
        // overlap on one, a name in a sentence on the other. Splitting the
        // session blurs neither.
        assert_eq!(
            authored_pr.attribution_confidence,
            CONFIDENCE_STRONG_OVERLAP
        );
        assert_eq!(mentioned.attribution_confidence, CONFIDENCE_MENTION_ONLY);
    }
}
