# RFC — Outcome/ROI engine (attribution, anchors, effort units, calibrated hours)

Status: proposed, **revised after measurement** · Scope: the contract between the open-source
daemon (this repo) and the closed-source server/self-hosted engine that turns *spend* into
*ROI on shipped outcomes*.

The first draft of this document proposed calibrating an effort judge against anchors mined from
each repo's *pre-AI era*, and treated human labels as a secondary cross-check. That design was
implemented and run against real repositories. It failed three times, in three different ways,
and the third failure is fatal to the whole class of approach (§1). This revision replaces the
central claim rather than qualifying it: **the engine reports dimensionless effort units always,
and hours only when the customer has supplied local labels to calibrate against** (§7).

Everything device-side here is grounded in code in this repo. Server-side sections are design;
they bind the server to the device contract, not the reverse. Privacy invariant throughout: the
daemon's wire schema (`daemon/crates/modelstat-wire/src/schema.rs`) is the *only* thing the
uploader can send (`daemon/crates/modelstat-ingest/` is the single outbound channel — README.md,
"Privacy & data handling"), and every type added here stays in the public-shape safety class
already established by `RepoRef`/`FileRef`: slugs, PR numbers, commit SHAs, timestamps, integer
counts, derived scores. No file contents, no prompts, no home paths, no author identity.

---

## 1. What we measured, and what it ruled out

Three hypotheses, each implemented and run against real repositories on disk
(`daemon/crates/modelstat-parsers/examples/anchor_probe.rs` is the instrument: it runs the real
miner over a repo path and prints what it found).

| # | Hypothesis | What we ran | Result | Verdict |
|---|---|---|---|---|
| 1 | **A fixed "pre-AI era" date cutoff selects a human baseline.** Default `2022-06-01`, env-overridable — predates Copilot-everywhere and ChatGPT. | Mine anchors on erpc, prism, modelstat. | **Zero anchors on every repo.** First commits: erpc `2024-05-06`, prism `2026-07-06`, modelstat `2026-06-14`. Every repo was *created* after the cutoff, so the whole of every history is "AI era" by that rule and the denominator is empty everywhere. Worse, the failure is silent: "0 anchors" is indistinguishable from "not mined". | **Killed.** Adoption is a per-commit fact, not a calendar fact. Replaced by AI-trailer detection (§6). |
| 2 | **Per-PR commit clustering yields per-PR effort.** Cluster a PR's own commit timestamps into sittings; the summed spans are minutes of work. | Mine, then count anchors that carry a usable per-PR commit range. | **0–17% coverage: erpc 0%, modelstat 1%, goldsky-infra 12%, prism 17%.** Cause is structural, not sampling: squash-merge is the dominant convention, a squash commit has ONE parent, and the branch it flattened is not in the repo's history at all — there is no range to cluster (`merge_range` returns `None` for a single-parent merge, `daemon/crates/modelstat-parsers/src/git_anchors.rs`). | **Killed as a general source.** On the repos where the product's users work, the signal is absent for 83–100% of PRs. |
| 3 | **Author-stream commit intervals are the high-coverage fallback.** Cluster each author's whole commit stream instead of one PR's branch; attribute intervals to the PRs they fall in. | Interval clustering over each author's commit stream on erpc, prism, modelstat. | **95–100% coverage, and useless.** It saturates — on erpc `p50 = p90 = 120 min`, the per-interval ceiling, so the statistic is reporting the cap rather than the work. And it barely tracks the thing effort must track: **Spearman ρ vs change size = 0.11 (erpc, n=27), 0.24 (prism, n=58), 0.12 (modelstat, n=87)**, against **≈0.30 for plain lines-of-code**. | **Killed, decisively.** The highest-coverage timing signal carries *less* information about the change than counting lines does — it loses to the trivial baseline it was invented to improve on. |

**Conclusion, stated plainly: local git contains no reliable per-PR human-effort ground truth.**
Git records when commits were written, not how long the work took, and the gap between those two
is not noise that averages out — it is the review latency, the meetings, the thinking that left no
commit, and the batching habits of whoever pressed the button. Any product that reports
"human-equivalent hours" from git alone is fabricating them.

This is a fact about the data, not a gap in the implementation, so no amount of further work on
the miner recovers it. What it changes is the product's claim surface (§2) and the shape of the
estimator (§7). What it does *not* touch: the attribution ledger (§4), the outcome signals (§8),
and the AI-vs-human split — all of which are deterministic reads of facts git really does record.

## 2. What this product can and cannot claim

| Measurable on-device, with real ground truth | Not measurable without human labels |
|---|---|
| **AI dollars and tokens per PR.** Every turn is parsed and priced on-device against the `prices/` tables (README.md — "parse + price turns"); the session→PR edge is `SessionMetadata` (`daemon/crates/modelstat-parsers/src/references.rs`). Spend attribution is arithmetic over facts, not inference. | **Human-equivalent hours.** The counterfactual "how long would a human have taken" is not recorded anywhere on the device. §1 finding 3 is the measurement that closes this door. Available only as Tier 2 (§7b), from labels a human actually wrote, published with its cross-validated error. |
| **AI-vs-human authorship, per PR, deterministically.** The tools sign their own work; `is_ai_authored` (`git_anchors.rs`) reads the trailer. This is a read of a string that is either present or absent — not a model's opinion. Real counts in §6. | **Dollar ROI in hours-saved terms.** It is `hours × loaded_rate`, so it inherits the hours' status exactly: absent below the label threshold, and never more precise than the calibration error above it. |
| **Shipped / reverted / abandoned.** Merge detection and `is_reverted` are local git reads with their evidence shipped alongside (`daemon/crates/modelstat-parsers/src/git_outcome.rs`; `merge_sha`/`merge_subject`/`merge_method` on `PullRequestRef`). Sessions that reference no outcome that ever merges are an explicit abandoned bucket (§4). | **Cross-repo effort comparison in any absolute unit.** Tier 1 units are normalized *within* a repo against that repo's own human-anchor population (§7a). "1.4 units in erpc" and "1.4 units in prism" are not the same quantity and are never summed as if they were. |
| **A relative effort ranking within one repo.** Change features (`daemon/crates/modelstat-effort/src/diff.rs`) placed against the repo's own human-authored PRs. Ordinal, dimensionless, and comparable to itself over time. | **Business impact.** Out of scope and stated as such (§10, row 7), unchanged from the first draft — the numerator was never business value. |

Two statements this document commits to, because the market does not:

* **A vendor that reports hours-saved from git alone is fabricating the number.** Not
  approximating it, not estimating it with wide error bars — fabricating it, because the input
  data does not contain it (§1).
* **The one public 0.94-correlation claim in this space was not made against git.** Weave's
  founder, on that number: *"Evaluated on a proprietary data set of manually labelled PRs"*
  ([HN, Nov 2024](https://news.ycombinator.com/item?id=42196381)). Manually labelled is the
  operative phrase, and it is consistent with our finding: labels are where hours come from. Our
  disagreement with that design is not that it used labels — it is that the labels were the
  vendor's and unverifiable by the customer. Ours are the customer's, stay on the customer's
  device, and every hour figure ships with the leave-one-out error measured on them (§7b).

## 3. Problem & unit economics

modelstat today answers *"what did we spend, on what kind of work?"* (README.md — spend by
activity, repo, model, person). It does not answer the question finance asks next: **what did
that spend buy?**

Tier 1 — always available, no labels, no time unit:

```
                    Σ effort units of shipped outcomes
units per dollar = ───────────────────────────────────      (within one repo)
                      token spend ($) + seat spend ($)
```

Tier 2 — only with ≥8 local labels for the repo (§7b), always rendered with its error:

```
value($) = hours_p50 × loaded_rate ($/hr, org-set, default $120) × realized_multiplier (§8)
```

| Decision | Rationale |
|---|---|
| **Denominate spend in dollars, never tokens.** | Tokens are not fungible: an Opus token, a GPT-5 token and a local-3B token are different goods at different prices doing different work. The daemon already prices every turn in dollars on-device (`prices/`), so dollars are the only unit that survives cross-model aggregation. Token counts stay on the wire as evidence, never as numerator or denominator. |
| **Numerator = shipped outcomes, not activity.** | Sessions, turns and abstracts measure *effort in*. The numerator counts only work that landed: merged PRs, discounted by whether it *stayed* landed (§8). |
| **Default headline is `$ per unit`, not `hours returned per $`.** | This reverses the first draft. "Hours returned per dollar" is legible to finance precisely because it asserts something we cannot support from git (§1). `$ per unit` asserts only what the data holds: spend divided by a within-repo effort ranking. It is comparable to *itself* — this repo, last month vs this month — which is the comparison that actually steers spend. |
| **Hours are a *view*, gated on labels, never the default.** | The dashboard surfaces hours only for repos that cleared the threshold, and never without the calibration error beside the number (§7b). A tenant with no labels sees units and is told, in one line, what it would take to see hours. |
| **Seat cost included.** | Subscription seats (Claude Max, Cursor, Copilot) are real spend the token meter never sees. The org configures seat $/person/month server-side; the daemon ships nothing new for this. |
| **The AI/human split is a headline in its own right.** | It needs no judge, no labels and no calibration — it is `is_ai_authored` over merged PRs (§6). "52 of your last 103 merged PRs were AI-assisted, and they cost $X" is a complete, defensible sentence on day one, which `$ per unit` is not until the ledger has coverage. |

## 4. Attribution ledger

The ledger is the join table: `spend rows (sessions) ⟷ outcome rows (merged PRs)`.
**Measurement did not touch this section**; it is arithmetic over device facts and stands as
first drafted.

**Unit of account: the merged PR.** One ledger row = one (session, outcome) edge with a dollar
amount and a confidence. Trunk-based teams who merge to main with no PR are outside the join —
per-session commit capture was drafted for them and dropped (§5a).

**The device half already exists.** The session→PR edge is `SessionMetadata`
(`daemon/crates/modelstat-parsers/src/references.rs` — repos, pull_requests, issues, files;
shipped per session under `IngestBatch.session_metadata`), assembled by the four-channel pass in
`daemon/crates/modelstat-pipeline/src/session_metadata.rs` (`build_session_metadata`), which
fuses in descending order of trust:

| Rank | Channel | Source tag |
|---|---|---|
| 3 | git context already on each event + injected on-disk repo read (`GitEnrichment::resolve_git`) | `git` |
| 2 | tool results | `tool` |
| 1 | redacted content — PR/issue URLs surviving in abstracts + excerpts | `content` |
| 0 | one best-effort on-device model call per session, reply re-parsed deterministically | `model` |

The ranking is implemented, not aspirational: `source_rank` in `references.rs` (git=3 > tool=2 >
content=1 > model=0), and dedupe keeps the strongest copy per natural key (`dedupe`, same file).
The server MUST inherit this ranking as the prior on edge quality — a `git`-sourced PR ref with an
on-device verified outcome (§8) is near-certain; a `model`-sourced ref alone is a hint.

**Cost splitting — one session, many PRs.**

| Case | Rule |
|---|---|
| 1 session → 1 PR | 100% to that PR. |
| 1 session → N PRs, file evidence available | **File-overlap weighting.** `SessionMetadata.files` (`FileRef`: repo-relative path + `lines_added`/`lines_deleted`, mined via git `--numstat` in step 6 of `build_session_metadata`) is intersected with each PR's changed-file set. Weight ∝ overlapping line churn. |
| 1 session → N PRs, no file evidence | Even split 1/N. |
| N sessions → 1 PR | Each session's (weighted) share accumulates on the PR row. |

Every ledger row carries `attribution_confidence ∈ [0,1]`, derived from (a) the best source rank
on the edge, (b) whether the device verified the outcome locally (§8), and (c) whether the split
was overlap-weighted or an even-split fallback. Rows below a threshold render as "estimated";
they are never silently dropped or silently trusted.

**Abandoned spend is a first-class category.** Sessions whose metadata references no outcome that
ever merges — or that reference nothing at all (`is_empty_session_metadata` in `references.rs`:
sessions with no references ship no metadata) — accrue to an explicit **abandoned/exploratory**
bucket, reported alongside ROI. Hiding it would overstate ROI by construction; a team whose
abandoned share drops from 40% to 15% has improved even if per-PR ROI is flat. Exploration is not
waste, but it is also not a shipped outcome.

## 5. Device contracts (this repo)

One additive extension, in the established Zod-parity serde style
(`daemon/crates/modelstat-wire/src/schema.rs` header comment: `.optional()` ⇒
`skip_serializing_if = "Option::is_none"`, `.default()` ⇒ `#[serde(default)]`; caps in UTF-8
bytes per `daemon/crates/modelstat-wire/src/caps.rs`). Existing golden fixtures keep parsing.

### 5a. Rejected — per-session commit capture

A `SessionMetadata.commits` array (one sha + timestamp per commit in the session's window) was
drafted here to give trunk-based teams a unit of account, and briefly shipped as a device type.
It is gone. The server attributes shipped work per-PR and drops the field on ingest (`store-ch`
migration `0006_drop_commits_json.sql`), so the capture was an extra bounded `git log` per repo
per session producing data nothing reads. Direct-to-main work stays outside the ledger until the
server grows a commit-range outcome row; the device half is small and can return with it.

### 5b. `IngestBatch.repo_anchors` — human-authored PRs, mined on-device

```
AnchorPr {
  pr_number:      u64
  merge_sha:      String          // hex 7..=64
  merged_at:      String          // ISO-8601
  files_changed:  u32
  lines_added:    u64
  lines_deleted:  u64
  span_ms:        Option<u64>     // first-commit→merge wall clock; omitted when unknown
  commit_count:   Option<u32>     // omitted when unknown
  active_minutes: Option<u32>     // clustered commit sittings; OBSERVATION ONLY — see below
  ai_assisted:    bool            // always false inside RepoAnchors.anchors
}
RepoAnchors {
  slug:               String       // cap 200
  host:               Option<String>   // cap 80, nullable
  cutoff:             Option<String>   // ISO-8601, NULL by default — see §6
  mined_at:           String
  head_sha:           String       // hex 7..=64 — history state at mining time
  human_anchor_count: u32
  ai_pr_count:        u32          // AI PRs seen in the SAME window
  anchors:            Vec<AnchorPr>    // cap ANCHORS_PER_REPO_COUNT_MAX = 50
}
IngestBatch.repo_anchors: Option<Vec<RepoAnchors>>   // cap REPO_ANCHORS_COUNT_MAX = 10 repos
```

| Decision | Rationale |
|---|---|
| **`cutoff` is `Option`, null by default.** | The first draft made it mandatory with a `2022-06-01` default. §1 finding 1 is what happened: every real repo postdates it and mined zero anchors. It survives only as an operator's explicit window (`AnchorConfig::cutoff`, `git_anchors.rs`), and the field stays on the wire so whatever was used is auditable per batch. Null means "no date filter", which is now the norm. |
| **`human_anchor_count` + `ai_pr_count` are shipped, not derivable.** | The walk stops as soon as the anchor cap is reached, so both counts describe the *same* recent window. A consumer reading `human_anchor_count: 6, ai_pr_count: 81` knows immediately that the baseline is thin and the repo is AI-dominated — a fact the anchor list alone cannot carry. |
| **`active_minutes` stays on the wire and drives nothing.** | It is a real observation about the PR (`active_minutes` in `git_anchors.rs`: commit timestamps clustered into sittings, 90-minute gap, 30-minute ramp). §1 findings 2 and 3 are why it is not an input to any estimate: it is absent for 83–100% of PRs and, where present, ρ ≈ 0.11–0.24 against change size. Keeping it visible is honest — deleting it would hide the evidence that killed the design; consuming it would repeat the mistake. |
| **Mined on-device, shipped as public shape.** | The alternative — server clones the repo — is exactly the access model this product exists to avoid. The daemon reads history it already reads (`run_git` in `modelstat-parsers`) and ships numbers only. |
| **`head_sha` + `mined_at` on every set.** | Anchor sets are versioned inputs (§7). A rebase/rewrite changes `head_sha`; the server treats that as a new `anchor_set_id`, never a silent mutation. `daemon/crates/modelstat-daemon/src/anchors.rs` uses the same pair locally as the re-mine gate: HEAD unchanged ⇒ the answer cannot have changed ⇒ no walk. |

**Mining is gated four ways** (`daemon/crates/modelstat-daemon/src/anchors.rs`): total opt-out via
`MODELSTAT_ANCHORS=0`, at most once per repo per daemon run, only when HEAD moved since the last
mine (recorded in `anchors.json` beside `state.json` — absolute repo paths live there and nowhere
else), and at most 10 repos per batch. Every git call is bounded and timed out (`--first-parent`,
`--max-count 2000`, 4s per call, 20s per repo, 60s per batch); any failure yields `None`. A batch
never waits on git and never fails for it.

## 6. Anchors: human-authored PRs, detected by the tools' own trailers

An anchor is **a merged PR with no AI trailer on any of its commits**. Not a PR merged before a
date (§1 finding 1) — a PR nothing signed.

**The detection rule** — `is_ai_authored(subject, body)` in
`daemon/crates/modelstat-parsers/src/git_anchors.rs`, one function, so widening it is a one-line
change in one place. Vendors: `claude|codex|cursor|copilot|devin|aider`. Three forms, all real:

| Form | Example | Match |
|---|---|---|
| Attribution trailer | `Co-Authored-By: Claude <noreply@anthropic.com>` | marker, then vendor, same line |
| Sign-off line | `🤖 Generated with [Claude Code](…)` | marker anywhere on its line, vendor after it |
| Vendor trailer key | `Claude-Code: 2.1.0` | hyphenated key at line start |

Two subtleties that real data forced, both of which cost us false positives before they were
fixed:

* **Line-scoped, not message-scoped.** The vendor name must sit after the marker on the *same
  line*. A message-wide match turns any commit body that mentions a tool in prose — "reverts the
  Cursor migration", "as discussed re: Codex" — into an AI PR, and a body containing both a human
  `Co-Authored-By:` and an unrelated mention three lines down classifies wrong.
* **Word-boundary, not substring.** `\b<vendor>\b`, so `.cursorrules`, `codexample` and a path
  like `docs/codex.md` do not trip it.

The asymmetry matters and dictates the conservative rule: **every false positive silently shrinks
the human baseline** — the population Tier 1 normalizes against (§7a) — while a false negative
merely adds one slightly-optimistic anchor. We would rather miss an AI PR than invent a human one.

**PR-number extraction is structural only** (`pr_number_from_subject`): `Merge pull request #123
from …`, or a trailing `(#123)`. A prose `#123` is deliberately not a merge. `git_outcome.rs` does
read a bare `#123` and is right to — it is checking a PR the session already named. Mining has no
such witness, and "fix bug reported in #123" would invent a calibration point out of a sentence.

**Classification is over all of a PR's commits**, not just the merge commit: the branch range for
a true merge, the squashed body for a squash merge. One AI commit makes the PR AI-assisted.

**What the mine actually finds** (probe runs this session, `anchor_probe.rs`; note the walk stops
at the 50-anchor cap, so both numbers describe the same window):

| Repo | Human anchors | AI-assisted PRs | Window |
|---|---|---|---|
| erpc | 50 (cap reached) | 52 | 103 merged-PR candidates |
| prism | 50 (cap reached) | 6 | 57 candidates |
| modelstat | 6 | 81 | 89 candidates |

Anchors have exactly two consumers, and neither of them is hours:

1. **The AI-vs-human split** (§3) — `human_anchor_count` / `ai_pr_count`, deterministic.
2. **The normalization population for Tier 1** (§7a) — the target PR's change features are placed
   against *this repo's own human-authored PRs*, which is what makes a unit mean "relative to how
   this team works" rather than "relative to a vendor's corpus". Only two anchor fields feed it,
   `lines_added + lines_deleted` and `files_changed`, because those are the only two an `AnchorPr`
   carries that describe the change. That is a deliberately small basis, and it is honest about
   what remains after §1: a size-and-shape ranking, normalized to a human population.

**Anchors are explicitly not a source of hours.** The first draft's isotonic mapping from anchor
`span_ms` to hours is deleted, not deprecated: §1 finding 2 says the span is usually absent, and
finding 3 says that where timing exists it does not track the work. A repo that squash-merges
everything still produces a perfectly good normalization population — sizes and shapes survive
squashing; timing does not.

## 7. The effort engine: two tiers

`daemon/crates/modelstat-effort/` — a crate that never opens a socket. Five modules: `diff.rs`
reads the commit locally and classifies-then-drops paths; `judge.rs` asks an **injected**
`Scorer` for a relative placement; `units.rs` is Tier 1; `calibrate.rs` and `labels.rs` are
Tier 2. `DiffFeatures` and `JudgedFeatures` deliberately do not implement `Serialize`, so no
source text, path or commit message can reach a wire through this crate.

The tier boundary is the document's central claim and a hard invariant:

> **Tier 1 is always available and is never a time unit. Tier 2 is a time unit and is available
> only with ≥8 local human labels. Below the threshold the API returns no hours at all — it does
> not degrade to a guess.**

### 7a. Tier 1 — relative effort units (no labels, always on)

```
EffortUnits { units: f64, percentile_vs_human_anchors: f64, judged: bool, anchor_n: usize }

anchor_score(a) = 0.55·ln(1+churn) + 0.20·ln(1+files_changed)      // the only two features an AnchorPr carries
target_score    = anchor_score(weighted target) + scatter + language-spread [+ judge blend]
units           = exp(target_score − median(anchor_scores))        // 1.0 ≡ repo median human PR
percentile_vs_human_anchors = empirical CDF of anchor_score over the human anchors, at the target
```

| Decision | Rationale |
|---|---|
| **Dimensionless. A PR of median human *shape* is exactly `1.0`; the empirical median of real PRs sits a shade above it.** | The number answers "how does this PR compare to what a human PR looks like in this repo", which is a question the data can answer. It is never printed with a time or currency suffix, and the type carries no field that could hold one. The `1.0` is exact by construction and pinned by a test — but measured over 150 real commits with every anchor's `active_minutes` absent (the ordinary squash-merge case) the distribution came out p05 `0.17` / p25 `0.30` / p50 `1.14` / p75 `5.43` / p95 `11.4` / max `122`. The median is `1.14`, not `1.00`, because a real target carries scatter and language-spread terms an `AnchorPr` structurally cannot; stating the exact figure rather than the round one is the point of the row. |
| **Normalized within-repo against the human-anchor population (§6), never across repos.** | A 400-line change means something different in a Rust daemon and a Terraform monorepo. Within-repo normalization is what makes units stable under the thing that varies most between customers, and it is why `units = exp(target − median(anchors))` rather than an absolute curve: the repo's own median human PR *defines* 1.0. Cross-repo sums are a reporting bug, not a feature to add later. |
| **The target's churn is class-weighted; the anchors' cannot be, and that asymmetry is deliberate.** | `diff.rs` weights the target's lines by path class (source 1.0, test 0.8, config 0.5, doc 0.2, generated/lockfile/vendored 0.02) before the log. `AnchorPr` carries no path information — by design, §5b — so an anchor's churn is raw. The asymmetry is one-directional: it can only push a generated-heavy target *down* the ranking, never up, which is exactly what makes a 5k-line lockfile PR score below a 200-line logic PR. Erring toward under-crediting the measured side is the right direction for a metric people are paid against. |
| **No interval.** | An interval on a dimensionless, ordinal score is theatre: it looks like precision accounting while quantifying nothing the reader can act on. Intervals appear in Tier 2, where the width is derived from measured residuals against real labels. |
| **`judged` and `anchor_n` travel with the number.** | `judged: false` means the placement came from change features alone because no `Scorer` was available — the estimate still exists and the consumer knows which kind it is. `anchor_n` is the size of the population it was normalized against; a unit computed against four anchors is a different object from one computed against fifty, and hiding that is how thin baselines become confident numbers. `anchor_n: 0` falls back to a nominal median (a 200-churn/5-file PR) and is the explicit flag that `percentile_vs_human_anchors` carries no information. |
| **The judge is never asked for a duration — and is never shown one.** | It is handed the target's shape and 5–8 of the repo's real human PRs, spaced across the churn range, and asked only for a *placement*; when it answers, the placement blends 50/50 with the anchor score at that quantile, plus a bounded novelty-minus-boilerplate term. A model that is systematically optimistic about absolute durations cannot express that bias through this interface. What crosses the seam is counts, extensions and structure-only line shapes (`structure_excerpt` in `diff.rs`) — no identifiers, no paths, no commit messages. **The `active=Nmin` column is gone from the reference lines, and this was not cosmetic**: `judge::reference_anchors` used to require `active_minutes.is_some()`, which on erpc's 0% clustering coverage (§1 finding 2) left zero usable references and made the judge decline on *every* PR. It now selects on `!ai_assisted` alone. |
| **The `Scorer` seam is injected, exactly like `LinkExtractor`.** | `daemon/crates/modelstat-pipeline/src/session_metadata.rs` already establishes the pattern: a trait-object the pipeline never links an engine into, built by the collector from frozen prompts (`daemon/crates/modelstat-pipeline/src/prompts.rs`). Self-hosted orgs point the scorer at their own OpenAI-compatible endpoint. Cloud, self-hosted and local differ in *where* the scorer runs, never in the contract. A scorer that is down degrades to `judged: false`; it never fails the estimate. |

### 7b. Tier 2 — calibrated hours (≥8 local labels, error always published)

```
labels::MIN_LABELS: usize = 8
labels::{ Label, LabelStore }    // JSON, caller-supplied path: {repo_slug: {pr_number: {minutes, labeled_at}}}
calibrate::calibrate_hours(..) -> Option<Calibration>          // None below MIN_LABELS
calibrate::estimate_hours(..)  -> HoursEstimate
Calibration   { scale, exponent, n, median_abs_pct_error, spearman_rho }   // all leave-one-out
HoursEstimate { p10, p50, p90 }  // hours
EffortReport  { units: EffortUnits, hours: Option<HoursEstimate>, calibration: Option<Calibration> }
```

All of it is re-exported at the crate root, so `modelstat_effort::calibrate_hours` and
`modelstat_effort::MIN_LABELS` are the citable paths. `calibrate.rs` is the old calibration
module, gutted: everything that turned `active_minutes` into minutes — `estimate`,
`estimate_from_size`, `size_prior`, `MIN_ANCHORS`, `EffortEstimate`, `Confidence`, and
`DiffFeatures::authored_lines` — is deleted, not deprecated, and what remains is Tier-2-only
fitting and the LOOCV/Spearman math.

| Decision | Rationale |
|---|---|
| **Hours come from labels a human wrote, or they do not come at all.** | This is §1's conclusion turned into an API. The label is the only artifact in the system that contains the counterfactual — "how long would this have taken you without AI" — and no arrangement of commit timestamps substitutes for it. |
| **Threshold ≥8, enforced by the compiler rather than at a call site.** | `Calibration` and `HoursEstimate` have private fields, no public constructor, and derive `Serialize` but **not** `Deserialize`. The only way to obtain a `Calibration` is `calibrate_hours` with `n ≥ 8`. This is a checked claim, not an intended one: a probe that struct-literals a `Calibration`, struct-literals a `HoursEstimate`, and `serde_json::from_str`s a `Calibration` fails to compile — `E0451` on both literals, `E0277` on the JSON route. An invariant that matters this much does not live in a runtime `if`. |
| **Below the threshold: `hours: None`, `calibration: None`.** | Not a wide interval, not a flagged guess, not a "provisional" number. A wide interval is still an assertion that the quantity was measured, and the whole finding of §1 is that it was not. The UI says what is missing and how many labels remain. |
| **Error is leave-one-out cross-validated, never in-sample — and the reason is the sign, not the ratio.** | The overfitting story would predict LOO error far above in-sample; measured, the ratio is only **1.03–1.15×**, because two parameters cannot memorize much. So the case for LOOCV is not "in-sample flatters the fit by 2×". It is that in-sample *cannot fail loudly*: on a label set whose units carry no information, LOO reports **~95% error and a negative `spearman_rho`** — the calibration announcing, in the sign of a number, that it cannot rank this repo's PRs — while in-sample scored that same set at 87% and never examined the sign. We publish the channel that can say "this does not work". |
| **No hours figure renders without its error beside it.** | A hard rule for every surface: dashboard, CLI, API response, export. "≈6.5 h (±41% LOO error, n=11 labels)" is a defensible sentence; "6.5 h" is not, and "15.266 h" (§10, row 1) is the failure mode this rule exists to prevent. |
| **Interval width is derived from the observed LOO residual spread**, not a fixed ±30%. | The band is a measurement of this repo's calibration quality, so it narrows as labels accumulate and widens when the label set is noisy. A constant band would be decoration. |
| **Labels stay on the device.** | A label is a person's self-report about their own work — outside the public-shape safety class, so it is not in the wire schema and cannot be uploaded by `modelstat-ingest`. The `LabelStore` is a local JSON file at a caller-supplied path. Only the fitted `Calibration` — five scalars, no PR identity — is derived output that may travel, subject to org policy. |
| **Label capture is sampled, author-only, and never a nag.** | One question on a sampled subset of the author's own recently merged PRs, bucketed answers, declinable. The response rate is itself reported: a calibration built on 8 labels from one author is annotated as such. |
| **Provenance travels with every score: `(rubric_version, judge_model, anchor_set_id, calibration_n)`.** | A score is meaningless without it. Any component changing triggers re-score of affected rows; dashboards never mix provenance tuples in one trend line without a marked break. Mirrors the daemon's own discipline (`PROCESSING_VERSION` in `daemon/PARITY.md`). |

### 7c. What replaced the old calibration loop

The first draft proposed three independent ground-truth channels: (a) a per-repo backtest against
anchor `span_ms`-derived hours, (b) spot labels as a cross-check, (c) a team-level natural
experiment. Channel (a) is deleted — it backtested against a quantity §1 proved is not effort, so
it would have published a confident ρ against noise. Channel (b) is promoted from cross-check to
**the** source of hours (§7b). Channel (c) survives, demoted to a sanity check:

| Channel | Mechanism | Status |
|---|---|---|
| **Local labels + LOOCV** | §7b. The customer's own labels, the customer's own error number, on the customer's own dashboard. | **Primary.** The only path to hours. |
| **Unit rank stability** | Re-score a repo's merged PRs after a judge-model or rubric change and compare *rankings*, not values. Tier 1 is ordinal, so rank instability is exactly the defect it can have, and it needs no labels to detect. | Always on, Tier 1's own regression test. |
| **Team-level throughput** | Units shipped per week per team, before vs after adoption, from the ledger. Coarse and unattributable to an individual PR, which is also why it is the least gameable. | Sanity check; never a calibration input. |
| **Drift** | Anchor refresh (new `head_sha` ⇒ new `anchor_set_id`) re-runs normalization; new labels re-run `calibrate_hours`. A `median_abs_pct_error` that crosses the publish threshold pulls the hours view *off*, back to units. | Automatic, and the removal is visible to the customer. |

## 8. Realized-value verification

**Merged ≠ valuable.** Every outcome row carries a realized-value multiplier applied to its units
(and to its hours where Tier 2 is live):

| Signal | Multiplier | Evidence |
|---|---|---|
| Alive (default) | 1.0 | — |
| **Reverted** | 0.0 | Already detected on-device: `check_pull_request_outcome` (`daemon/crates/modelstat-parsers/src/git_outcome.rs`) reads local history for the merge commit and `is_reverted` matches `git revert`'s "This reverts commit" body. The pass stamps `reverted` in step 5 of `build_session_metadata`. The server trusts the device signal because the evidence rides along — it can check the convention rather than take the boolean on faith. |
| **Rapid churn** | discount | The PR's files are substantially rewritten within 30 days, detected from *subsequent sessions'* `FileRef` spend on the same `(slug, path)` set. Discount ∝ fraction of the PR's added lines re-churned, floored at 0.25 — churn can be iteration, not waste, and the floor caps how wrong we can be in either direction. |
| **Defect linkage** | discount | Issues or fix-PRs within ≤30 days touching the same files, joined via `SessionMetadata.issues`/`pull_requests` + file overlap. Same floored-discount shape. |

Multipliers compose multiplicatively and are recomputed as the 30-day window matures — an
outcome's realized value is *provisional for 30 days*, and the dashboard says so.

## 9. Reporting

| Decision | Rationale |
|---|---|
| **Aggregate-level only**: weekly × repo × work-type. | The metric is built to steer *spend allocation* (which models, which work-types, which repos), not people. |
| **Person-level data is visible ONLY to the person themselves.** | Self-coaching ("my abandoned share is 3× the team's") is valuable; manager-facing person rankings are a Goodhart engine that also poisons the input data — including the labels of §7b, which are self-reports and would become negotiations. A product invariant, not a settings default. |
| **Units are never summed across repos; hours are never shown without their error.** | The two claim-surface rules of §2 and §7b, enforced at the rendering layer rather than trusted to callers. |
| **"Wouldn't-have-been-built" work is a separate new-capacity category** — never infinite ROI. | Work with no plausible human counterfactual (the one-off migration script nobody would have prioritized) cannot honestly claim "hours returned". It is reported as *new capacity* — units shipped, no ROI ratio — flagged by the judge's counterfactual-plausibility feature. Folding it into ROI would let the metric inflate on exactly the cheapest work. |

## 10. Known shortcomings of prior art — and what we actually counter

Weave (workweave.ai, YC — ["Weave Hour": a standardized unit of engineering output, approximately
one hour of work by an expert software engineer](https://www.ycombinator.com/companies/weave-3))
is the closest prior art and drew a public critique thread:
[Show HN, Nov 2024](https://news.ycombinator.com/item?id=42196381). Every countermeasure below is
a mechanism that exists in §4–§9. Where our first draft claimed a countermeasure the measurements
or the implementation do not support, the row now says so instead.

| # | Shortcoming (public record) | What we actually do |
|---|---|---|
| 1 | **Point estimates with false precision** — example scores published to three decimals ("15.266" hours for a PostHog PR, same HN thread). | Partial, and stated as partial. Tier 1 *is* a point number — but a dimensionless, repo-relative one that no reader can mistake for an accounting quantity, carrying `anchor_n` and `judged` (§7a). Tier 2 hours are always `{p10,p50,p90}` **and** always rendered with `median_abs_pct_error` from LOOCV (§7b). Where we cannot bound the error, we publish no number. |
| 2 | **Ground truth = vendor's proprietary corpus** — founder, on the 0.94 correlation: *"Evaluated on a proprietary data set of manually labelled PRs"* (HN thread). Unverifiable by the customer. | Hours are calibrated on **the customer's own labels**, stored on the customer's device (`labels::LabelStore`), and the accuracy number published next to every hour figure is leave-one-out error **on those same labels** (§7b). The claim is auditable by the party it is sold to, and reproducible by them. Our first draft answered this row with "the customer's own pre-AI anchors"; §1 killed that answer — anchors calibrate *nothing* to hours now, they only define the normalization population (§6). |
| 3 | **Regressions / long-term debt admittedly not captured** — founder: "Not captured (part of why it's only an important part of the story…)". | Realized-value multipliers (§8): reverted ⇒ 0.0, device-detected in `git_outcome.rs`; rapid-churn and ≤30-day defect-linkage discounts; values provisional until the window matures. |
| 4 | **Wall-clock vs "isolated complexity hours" conflation** — HN critique: a "15h" PR demonstrably took ~2 weeks of elapsed collaborative work. | **We removed the conflation by removing wall clock from the estimator entirely.** The first draft answered this row by calibrating on `AnchorPr.span_ms` — first-commit→merge — which is *the* wall-clock quantity the critique is about, and §1 measured what it is worth: ρ ≈ 0.11–0.24 against change size, worse than line count. Tier 1 makes no temporal claim at all; Tier 2's unit is whatever the labeller meant when they answered the question, which makes the definition the customer's rather than ours. `span_ms` and `active_minutes` remain on the wire as observations and feed nothing (§5b). |
| 5 | **Gameable via PR-description/code inflation** — HN: "developers setting up LLM prompts to make their code seem more complex". | The judge never sees prose: the prompt is counts, extensions and structure-only line shapes (`structure_excerpt`, `diff.rs`) — there is no free-text path from a PR description to a score. Boilerplate fraction is an explicit extracted feature, so padding *lowers* the placement. Person-level invisibility to managers (§9) removes the strongest incentive. Churn discount (§8) claws back inflation that ships as rework. Residual and unpatched: an author who inflates their own §7b labels moves their own repo's hours; the LOOCV error is what makes an inconsistent labeller visible. |
| 6 | **Surveillance backlash** — top HN comment: the "AI scored your productivity at 47%" performance-review dystopia. | Person-level data visible only to the person themselves; aggregate-only reporting; no manager-facing person rankings (§9). Labels are self-reports and never leave the device (§7b). The metric steers spend, not reviews. |
| 7 | **Business impact not measured but not disclaimed prominently** — HN: "If you build something that doesn't solve problems with impact to the business, your real productivity is zero". | Explicitly out of scope and stated up front (§2, right column): the numerator is shipped engineering output, never business value. "Wouldn't-have-been-built" work is fenced into a new-capacity category rather than laundered into ROI (§9). |
| 8 | **Only authored PRs counted** — review, mentoring and unblocking invisible. | **Not countered — we share this shortcoming.** The first draft claimed reviews would be "first-class scored work items"; nothing in the device or the crate supports that, and asserting it here would be exactly the kind of unbacked claim this revision exists to remove. What the daemon sees is sessions and merged outcomes; review and mentoring effort are outside that, and a team should read every number in this system as covering authored, shipped work only. |
| 9 | **Opaque single score** — "a black box that takes in data and spits out… something". | Every score carries `(rubric_version, judge_model, anchor_set_id, calibration_n)` (§7b); every ledger row carries `attribution_confidence` and its split method (§4); anchor sets are pinned by `head_sha`/`mined_at` (§5b); `EffortUnits` exposes `percentile_vs_human_anchors`, `judged` and `anchor_n` (§7a). Every number decomposes into inspectable parts, and the parts are the ones a skeptic would ask for. |

## 11. Router-label side-effect

Every scored outcome emits a training row for the model-recommendation feature the MCP server
already gestures at (README.md: "Recommend a model for a code-review task — based on what worked
for us before"):

```
RouterLabel {
  work_type:              String      // the org-learned activity vocabulary
  repo_slug:              String
  model / provider:       String
  tokens:                 TokenUsage  // per-class counts, as on the wire today (schema.rs)
  cost_usd:               f64         // attributed dollar share (§4 split)
  units:                  f64         // Tier 1 — ALWAYS present (§7a)
  hours_p10/p50/p90:      Option<f64> // Tier 2 — present only for calibrated repos (§7b)
  calibration_error_pct:  Option<f64> // never present without the hours, never absent with them
  realized_multiplier:    f64         // §8, as of labeling
  attribution_confidence: f64         // §4
  rubric_version / judge_model / anchor_set_id / calibration_n   // §7b provenance
  merged_at:              String
}
```

Server-side derived table; nothing new on the device wire. Rows refresh when multipliers mature
(§8) or scores re-version (§7b). The router trains on `units`, which are available from P1 and
comparable within a repo — waiting for hours would mean waiting for labels the recommender does
not need. The eventual recommender is out of scope; the point is that the ledger's exhaust *is*
the training set.

## 12. Rollout phases

| Phase | Ships | Gate to next |
|---|---|---|
| **P1 — Ledger + AI/human split + units** (no labels, no hours) | §4 join on existing `SessionMetadata`; cost splitting; abandoned bucket; §5b/§6 anchor mining with the trailer rule; AI-vs-human split and `$ per unit` on the dashboard; §7a `EffortUnits`. Nothing here needs a human label, and the split needs no model at all. | Join coverage ≥70% of spend on pilot orgs; unit rankings stable across re-scores (§7c). |
| **P2 — Label capture + calibrated hours** | §7b `LabelStore`, sampled author-only label prompt, `calibrate_hours`, LOOCV error, hours view gated at `MIN_LABELS = 8`. | ≥8 labels on pilot repos **and** `median_abs_pct_error` below the publish threshold. Repos that miss it stay on units — that is a normal outcome, not a failure to work around. |
| **P3 — Realized multipliers** | §8 revert/churn/defect discounts; provisional-value windows in the UI. | Discounts stable across a full 30-day window on pilot data. |
| **P4 — Continuous calibration** | Re-fit on new labels; drift detection pulls the hours view off when error crosses the threshold; anchor refresh re-runs the split and the normalization (§7c). | Label-drift and anchor-refresh paths exercised end-to-end on pilots, including the un-publish direction. |
| **P5 — Routing labels** | Model/work-type routing trained on §11 rows. | — |

Device-side, only P1/P2 touch this repo: §5's additive `repo_anchors` wire type and the
`modelstat-effort` crate (whose only `Serialize` types are the five-scalar `Calibration`, the
three-float `HoursEstimate` and `EffortUnits`). P3 consumes signals the daemon already ships
(`merged`/`reverted`/`FileRef` — `references.rs`, `git_outcome.rs`). The privacy boundary is
unchanged in every phase: one uploader crate, one schema file, public shapes only — plus one new
rule this revision adds, that human labels are local-only and never join them (README.md,
"Audit it yourself").
