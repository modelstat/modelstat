//! The local join from agent sessions to shipped work, in measured primitives.
//!
//! ```text
//!   session transcripts ──▶ tokens (5 classes) + event timestamps
//!                                        │
//!            changed files × time proximity to each merge
//!                                        │
//!                    ┌───────────────────┴───────────────────┐
//!                    ▼                                       ▼
//!              PrSpend per PR                          unattributed
//!        (mix, equiv_tokens, active_ms)          (mix, active_ms, count)
//!
//!   git show --numstat ──▶ DiffFeatures (files, +/−, hunks, path classes)
//! ```
//!
//! ## Primitives, not a verdict
//!
//! Two modules, both reading only what this device can already see:
//!
//! * [`attribution`] — what the machine spent on work that shipped. Tokens by
//!   class, and [`active_ms`](attribution::active_ms), the union of activity
//!   windows over a session's own events. Both are attributed to pull requests
//!   through the SAME split weights, so time and tokens can never disagree
//!   about which PR a session belongs to.
//! * [`diff`] — what a merged PR changed, read from the local repo: files,
//!   lines added, lines deleted, hunks, and churn by path class.
//!
//! Everything here is a quantity somebody could recount by hand from the same
//! files. Nothing is blended: there is no composite of these numbers, no
//! weighting of one against another, and no ranking of a person or a change.
//! What any given team considers expensive is theirs to decide, and a score
//! computed here would be this crate's opinion wearing the clothes of a
//! measurement.
//!
//! For the same reason time is reported beside outcomes and never multiplied
//! into them. `active_ms` measures activity, not productivity or savings.
//!
//! ## What is not measured, and is therefore absent
//!
//! `agent_working_ms` — the developer's wait on the agent — is turn-level and
//! needs message timing this crate does not model. The server computes it from
//! messages; the daemon does not guess it.
//!
//! ## Privacy
//!
//! The `Serialize` types are exactly the count shapes:
//! [`PrSpend`](attribution::PrSpend), [`SpendSummary`](attribution::SpendSummary)
//! and [`TokenMix`](attribution::TokenMix) — a repo slug, a PR number and
//! numbers. [`DiffFeatures`] deliberately does NOT implement it, so no source
//! text, path, or commit message can reach a wire through this crate. Paths are
//! read locally (the only way to tell a lockfile from a parser) and dropped;
//! transcript paths, turn text and working directories live inside one call.

pub mod attribution;
pub mod diff;

pub use attribution::{spend_by_pr, spend_by_pr_events, PrSpend, SpendSummary, TokenMix};
pub use diff::{classify_path, diff_features, parse_numstat, DiffFeatures, PathClass};
