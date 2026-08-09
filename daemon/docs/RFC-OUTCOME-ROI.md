# RFC — Outcome/ROI engine (attribution, judge, anchors, calibration)

Status: proposed · Scope: the contract between the open-source daemon (this repo) and the
closed-source server/self-hosted engine that turns *spend* into *ROI on shipped outcomes*.

Everything device-side in this document is grounded in code that already ships or in the two
additive wire types this RFC introduces (§3). Server-side sections (§4–§7) are design; they bind
the server to the device contract, not the reverse. Privacy invariant throughout: the daemon's
wire schema (`daemon/crates/modelstat-wire/src/schema.rs`) is the *only* thing the uploader can
send (`daemon/crates/modelstat-ingest/` is the single outbound channel — README.md, "Privacy &
data handling"), and every type added here stays in the public-shape safety class already
established by `RepoRef`/`FileRef`: slugs, PR numbers, commit SHAs, timestamps, line counts.
No file contents, no prompts, no home paths.

---

## 1. Problem & unit economics

modelstat today answers *"what did we spend, on what kind of work?"* (README.md — spend by
activity, repo, model, person). It does not answer the question finance actually asks next:
**what did that spend buy?**

The unit economics are deliberately narrow:

```
        value of shipped outcomes ($)
ROI = ─────────────────────────────────
        token spend ($) + seat spend ($)
```

| Decision | Rationale |
|---|---|
| **Denominate in dollars, never tokens.** | Tokens are not fungible: an Opus token, a GPT-5 token, and a local-3B token are different goods at different prices doing different work. The daemon already prices every turn in dollars on-device (README.md — "parse + price turns"; `prices/` tables), so dollars are the only unit that survives cross-model aggregation. Token counts remain on the wire as evidence, never as the numerator or denominator. |
| **Numerator = shipped outcomes, not activity.** | Sessions, turns, and abstracts measure *effort in*. The numerator counts only work that landed: merged PRs and direct-to-main commit ranges (§2), discounted by whether they *stayed* landed (§5). |
| **Headline metric: hours returned per dollar.** | "Value" is operationalized as *engineer-hours of equivalent output* (the judge's effort interval, §4) × an org-set loaded rate (§6). Reporting "hours returned per $" keeps the metric legible to both engineers ("this saved me a day") and finance ("$1 in → 0.4 loaded hours out") without pretending to measure business impact — which is explicitly out of scope (§8, row 7). |
| **Seat cost included.** | Subscription seats (Claude Max, Cursor, Copilot) are real spend the token meter never sees. The org configures seat $/person/month server-side; the daemon ships nothing new for this. |

## 2. Attribution ledger

The ledger is the join table: `spend rows (sessions) ⟷ outcome rows (merged PRs / commit ranges)`.

**Unit of account: the merged PR.** For direct-to-main workflows (no PR ever exists) the unit is
a *commit range*: the session's commits to one repo, grouped per session (§3a). One ledger row =
one (session, outcome) edge with a dollar amount and a confidence.

**The device half already exists.** The session→PR edge is `SessionMetadata`
(`daemon/crates/modelstat-parsers/src/references.rs`, `SessionMetadata` — repos, pull_requests,
issues, files; shipped per session under `IngestBatch.session_metadata`), assembled by the
four-channel pass in `daemon/crates/modelstat-pipeline/src/session_metadata.rs`
(`build_session_metadata`), which fuses in descending order of trust:

| Rank | Channel | Source tag |
|---|---|---|
| 3 | git context already on each event + injected on-disk repo read (`GitEnrichment::resolve_git`) | `git` |
| 2 | tool results | `tool` |
| 1 | redacted content — PR/issue URLs surviving in abstracts + excerpts | `content` |
| 0 | one best-effort on-device model call per session, reply re-parsed deterministically | `model` |

The ranking is implemented, not aspirational: `source_rank` in
`daemon/crates/modelstat-parsers/src/references.rs` (git=3 > tool=2 > content=1 > model=0), and
dedupe keeps the strongest copy per natural key (`dedupe`, same file). The server MUST inherit
this ranking as the prior on edge quality — a `git`-sourced PR ref with an on-device verified
outcome (§5) is near-certain; a `model`-sourced ref alone is a hint.

**Cost splitting — one session, many PRs.** A session's dollar cost splits across the outcomes
it references:

| Case | Rule |
|---|---|
| 1 session → 1 PR | 100% to that PR. |
| 1 session → N PRs, file evidence available | **File-overlap weighting.** `SessionMetadata.files` (`FileRef` in `daemon/crates/modelstat-parsers/src/references.rs`: repo-relative path + `lines_added`/`lines_deleted`, mined via git `--numstat` in step 6 of `build_session_metadata`) is intersected with each PR's changed-file set (server-side, from the forge or from `AnchorPr.files_changed`-style mining). Weight ∝ overlapping line churn. |
| 1 session → N PRs, no file evidence | Even split 1/N. |
| N sessions → 1 PR | Each session's (weighted) share simply accumulates on the PR row; no extra rule needed. |

Every ledger row carries `attribution_confidence ∈ [0,1]`, derived from (a) the best source rank
on the edge, (b) whether the device verified the outcome locally (§5), and (c) whether the split
was overlap-weighted or an even-split fallback. Rows below a threshold render as "estimated" in
the UI; they are never silently dropped or silently trusted.

**Abandoned spend is a first-class category.** Sessions whose metadata references no outcome
that ever merges (or that reference nothing at all — note `is_empty_session_metadata` in
`references.rs`: sessions with no references ship no metadata) accrue to an explicit
**abandoned/exploratory** bucket, reported alongside ROI. Hiding it would overstate ROI by
construction; a team whose abandoned share drops from 40% to 15% has improved even if per-PR
ROI is flat. Exploration is not waste, but it is also not a shipped outcome — the category
keeps the numerator honest without moralizing.

## 3. New device contracts (this repo)

Two additive extensions, both in the established Zod-parity serde style
(`daemon/crates/modelstat-wire/src/schema.rs` header comment: `.optional()` ⇒
`skip_serializing_if = "Option::is_none"`, `.default()` ⇒ `#[serde(default)]`; caps applied in
UTF-8 bytes per `daemon/crates/modelstat-wire/src/caps.rs`). Existing golden fixtures keep
parsing; absent fields serialize as omitted.

### 3a. `SessionMetadata.commits` — direct-to-main attribution

```
CommitRef {
  slug:         Option<String>   // cap 200, the repo it landed in
  sha:          String           // hex, 7..=64 chars
  committed_at: String           // ISO-8601
  source:       String           // default "git"
}
SessionMetadata.commits: Vec<CommitRef>   // cap 100, default empty
```

Rationale: `PullRequestRef` covers PR-flow teams; trunk-based teams merge to main with no PR
number to reference, so today their shipped work is invisible to the join. The pass already
computes the session's commit-capture window — session span + `COMMIT_GRACE_MS` (4h,
capped at the next session's start so two sessions never double-claim a commit;
`daemon/crates/modelstat-pipeline/src/session_metadata.rs`) — and already runs git in that
window for `FileRef`s (step 6). `commits` records the SHAs the same window read observes. The
server groups a session's commits per repo into one commit-range work item: the direct-to-main
analogue of a PR. A SHA + timestamp is the same safety class as a slug (public repo fact).

### 3b. `IngestBatch.repo_anchors` — pre-AI anchors mined on-device

```
AnchorPr {
  pr_number:     u64
  merge_sha:     String          // hex 7..=64
  merged_at:     String          // ISO-8601
  files_changed: u32
  lines_added:   u64
  lines_deleted: u64
  span_ms:       Option<u64>     // first-commit→merge wall-clock; omitted when unknown
  commit_count:  Option<u32>     // omitted when unknown
}
RepoAnchors {
  slug:     String               // cap 200
  host:     Option<String>       // cap 80, nullable
  cutoff:   String               // ISO-8601 — anchors are strictly before this
  mined_at: String               // ISO-8601
  head_sha: String               // hex 7..=64 — history state at mining time
  anchors:  Vec<AnchorPr>        // cap 50
}
IngestBatch.repo_anchors: Option<Vec<RepoAnchors>>   // cap 10 repos; optional, omitted when absent
```

This is the calibration substrate for §4: **merged-PR statistics from the repo's own pre-AI
history.** The repos are already on disk (that is the whole premise of the daemon — README.md,
"logs already on disk"; the metadata pass already runs `git log` against them via the injected
`GitEnrichment` seam). Mining walks local history for merge/squash commits before the cutoff —
the same subject-ref convention `find_merge_commit_for_pr` already reads in
`daemon/crates/modelstat-parsers/src/git_outcome.rs` — and emits *only* the public-shape stats
above. `span_ms` is derived from commit timestamps (first commit in the range → merge), the
honest wall-clock signal §4's calibration needs; it is optional because squash-merges can
destroy the range.

| Decision | Rationale |
|---|---|
| Default cutoff **2022-06-01**, env-overridable (`MODELSTAT_ANCHOR_CUTOFF`). | Predates Copilot-everywhere and ChatGPT; PRs merged before it are a defensibly human baseline. Orgs that adopted AI earlier/later set their own date — the `cutoff` field makes whatever was used auditable per batch. |
| Mined on-device, shipped as stats. | The alternative — server clones the repo — is exactly the access model this product exists to avoid. The daemon reads history it already reads (`run_git` in `modelstat-parsers`), ships numbers in the `FileRef` safety class (`references.rs` calls it out: "no contents, no home paths"). |
| `head_sha` + `mined_at` on every set. | Anchor sets are versioned inputs to calibration (§4, §7d). A rebase/rewrite changes `head_sha`; the server treats that as a new `anchor_set_id`, never a silent mutation. |
| Caps 50 anchors × 10 repos per batch. | Calibration needs tens of anchors, not thousands (isotonic regression saturates quickly); caps bound batch size like every other collection in the schema (`caps.rs`). Refresh ships a new set rather than growing one. |

## 4. Impact judge (server / self-hosted engine)

The judge scores each merged PR (or commit range) with an **effort interval in
expert-engineer-hours**. Architecture is deliberately *not* "ask an LLM how many hours":

| Decision | Rationale |
|---|---|
| **LLM as feature extractor, small calibrated regression head as estimator.** | The LLM reads the diff and emits structured features: work category, novelty vs boilerplate fraction, risk domains touched, test/infra share, cross-cutting-ness. A small regression head (per-repo calibrated, §below) maps features → hours. End-to-end LLM hour-guessing fails on all three axes we can measure: run-to-run stability, cost per score, and rerun consistency after model upgrades. Features are cheap to re-head; a new judge model re-runs extraction once and recalibrates. |
| **Anchor-based calibration: per-repo isotonic mapping.** | The judge's raw output is a *placement* (where this PR sits relative to the repo's anchor PRs, §3b). An isotonic (monotone) regression maps placement → hours using the repo's own pre-AI anchors, whose `span_ms`/size stats are ground truth from that codebase's actual history. This makes "hours" mean *hours in this repo, for this team* — not hours in a vendor's hand-labeled corpus (§8, row 2). Isotonic because the only assumption we trust is monotonicity (harder placement ⇒ ≥ hours); no parametric shape. |
| **Output is ALWAYS an interval `{p10, p50, p90}`.** | Never a point estimate. The interval comes from the anchor distribution around the placement plus ensemble spread. Point estimates manufacture false precision (§8, row 1) and invite ranking abuse the numbers cannot support. |
| **Ensemble of 3; disagreement widens the interval.** | Three extraction runs (temperature/model-seed varied). Head maps each; the reported interval spans min-p10..max-p90 of the runs. Agreement ⇒ tight band; disagreement is *signal about uncertainty*, not noise to average away. |
| **Everything versioned: `(rubric_version, judge_model, anchor_set_id)` stored per score.** | A score is meaningless without its provenance triple. Any component changing triggers re-score of affected rows; dashboards never mix triples in one trend line without a marked break. Mirrors the daemon's own discipline (`PROCESSING_VERSION` in `daemon/PARITY.md`). |
| **PR description is untrusted input.** | The description field is a prompt-injection surface and a gaming surface ("this took weeks of careful analysis…"). The diff is primary evidence; title/description are extracted-feature *hints* whose influence is bounded by the feature schema — there is no free-text path from description to hours (§8, row 5). |
| **Reviews are scored work items.** | Review is engineering output; scoring only authored PRs taxes reviewers and rewards volume. Review work items are scored separately (diff-of-review-round + comment structure as evidence), attributed to the reviewer, and flow into the same ledger with their own category. |
| **Self-hosted mode reuses the daemon's injected-engine seam.** | The judge is the same shape as the daemon's model channels: an injected async function behind a frozen prompt contract — exactly the `LinkExtractor` seam (`daemon/crates/modelstat-pipeline/src/session_metadata.rs`: a trait-object closure the pipeline never links an engine into, built by the collector from frozen prompts in `daemon/crates/modelstat-pipeline/src/prompts.rs` + the summarizer client). Self-hosted orgs point the judge at their own OpenAI-compatible endpoint, the same way the summarizer already works in self-hosted mode (README.md, "three modes"). Cloud, self-hosted, and local differ in *where the judge runs*, never in the contract. |

## 5. Realized-value verification

**Merged ≠ valuable.** Every outcome row carries a realized-value multiplier applied to its
judged hours:

| Signal | Multiplier | Evidence |
|---|---|---|
| Alive (default) | 1.0 | — |
| **Reverted** | 0.0 | Already detected on-device: `check_pull_request_outcome` in `daemon/crates/modelstat-parsers/src/git_outcome.rs` reads local history for the merge commit (subject-ref convention, shipped with its evidence — `merge_sha`, `merge_subject`, `merge_method` on `PullRequestRef`, `references.rs`) and `is_reverted` matches `git revert`'s "This reverts commit" body. The pass stamps `reverted` in step 5 of `build_session_metadata`. The server trusts the device signal because the evidence rides along — it can check the convention rather than take the boolean on faith (design note on `PrOutcome`, `git_outcome.rs`). |
| **Rapid churn** | discount | The PR's files are substantially rewritten within 30 days. Detected from *subsequent sessions'* `FileRef` spend on the same `(slug, path)` set — re-spend on just-shipped lines is the device-visible shadow of churn. Discount ∝ fraction of the PR's added lines re-churned, floored at 0.25 (churn can be iteration, not always waste — the floor caps how wrong we can be in either direction). |
| **Defect linkage** | discount | Issues or fix-PRs within ≤30 days touching the same files, joined via `SessionMetadata.issues`/`pull_requests` + file overlap. Same floored-discount shape as churn. |

Multipliers compose multiplicatively and are recomputed as the 30-day window matures — an
outcome's realized value is *provisional for 30 days*, and the dashboard says so.

## 6. ROI formula & reporting

```
value($)  = effort_p50 (hours) × loaded_rate ($/hr, org-set, default $120) × realized_multiplier
ROI band  = [ Σ value_p10 , Σ value_p90 ] / ( token $ + seat $ )    — reported per aggregate
```

| Decision | Rationale |
|---|---|
| ROI is reported as a **[p10, p90] band**, headline "hours returned per dollar" at p50. | The interval discipline of §4 survives aggregation or it was theater. |
| Loaded rate is org-set, default $120/hr. | A knob finance already owns. Defaulting avoids a setup wall; the dashboard shows which rate produced every dollar figure. |
| **Aggregate-level only**: weekly × repo × work-type. | The metric is built to steer *spend allocation* (which models, which work-types, which repos), not people. |
| **Person-level data is visible ONLY to the person themselves.** | Self-coaching ("my abandoned share is 3× the team's") is valuable; manager-facing person rankings are a Goodhart engine that also poisons the input data — engineers who know they are ranked on judged-hours will optimize judged-hours (§8, rows 5–6). This is a product invariant, not a settings default. |
| **"Wouldn't-have-been-built" work is a separate new-capacity category** — never infinite ROI. | Work with no plausible human counterfactual (the one-off migration script nobody would have prioritized) cannot honestly claim "hours returned". It is reported as *new capacity* (judged hours, no ROI ratio), flagged by the judge's counterfactual-plausibility feature. Folding it into ROI would let the metric inflate on exactly the cheapest work. |

## 7. Calibration loop

Three **independent** ground-truth channels — independent so that a failure of one is visible in
the others:

| Channel | Mechanism | Published where |
|---|---|---|
| **(a) Per-repo backtest** | Hold out a slice of the repo's pre-AI anchors (§3b) from calibration; judge them blind; compare judged intervals to actual `span_ms`-derived hours. **Spearman ρ and median absolute error are published ON the dashboard, per repo.** If the number is bad, the org sees it is bad — accuracy claims are load-bearing only when falsifiable by the customer (§8, rows 1–2). | Dashboard, per repo |
| **(b) Spot labels** | One-click prompt on sampled scored PRs, to the *author only* (consistent with §6): "How long would this have taken you without AI?" — bucketed answers, sampled sparsely to avoid nag-fatigue. Aggregated as a bias check on the judge, weighted by response rate. | Calibration report |
| **(c) Team-level natural experiment** | Complexity-weighted merge throughput (judged hours shipped per week) pre- vs post-AI-adoption per team, from the same anchors + ledger. The coarsest but least gameable channel: it needs no judge trust at the individual-PR level, only rank stability. | Dashboard, per team |
| **Drift monitoring** | Anchor refresh (new `RepoAnchors` set, new `anchor_set_id`) automatically re-runs (a). A ρ drop past threshold flags the repo's scores as *uncalibrated* until re-headed. Judge-model or rubric changes re-run (a) across all repos before rollout (the `(rubric_version, judge_model, anchor_set_id)` triple of §4 is what makes this cheap and auditable). | Internal + dashboard flag |

## 8. Known shortcomings of prior art — and our countermeasures

Weave (workweave.ai, YC — ["Weave Hour": a standardized unit of engineering output,
approximately one hour of work by an expert software engineer](https://www.ycombinator.com/companies/weave-3))
is the closest prior art and drew a public critique thread:
[Show HN, Nov 2024](https://news.ycombinator.com/item?id=42196381). Rows below cite that public
record; each countermeasure is a mechanism specified in §2–§7, not an intention.

| # | Shortcoming (public record) | Our countermeasure |
|---|---|---|
| 1 | **Point estimates with false precision** — example scores published to three decimals ("15.266" hours for a PostHog PR, same HN thread). | Intervals `{p10,p50,p90}` everywhere, ensemble disagreement widens them (§4); ROI reported as a band (§6); precision claims bounded by *published* per-repo error (§7a). |
| 2 | **Ground truth = vendor's proprietary corpus** — founder, on the 0.94 correlation: "Evaluated on a proprietary data set of manually labelled PRs" (HN thread). Unverifiable by the customer. | Calibration anchors are the *customer's own* pre-AI merged PRs, mined on-device from their local history (§3b), and the backtest against them — Spearman ρ + median abs error — is published on the customer's dashboard (§7a). The accuracy claim is auditable by the party it is sold to. |
| 3 | **Regressions / long-term debt admittedly not captured** — founder: "Not captured (part of why it's only an important part of the story…)" (HN thread). | Realized-value multiplier (§5): reverted ⇒ 0.0 (device-detected, `git_outcome.rs`), rapid-churn and ≤30-day defect-linkage discounts; values provisional until the window matures. |
| 4 | **Wall-clock vs "isolated complexity hours" conflation** — HN critique: a "15h" PR demonstrably took ~2 weeks of elapsed collaborative work; commenter suggests the unit is really "isolated complexity hours". | Anchors carry real wall-clock: `AnchorPr.span_ms` is first-commit→merge from the repo's own history (§3b), so the isotonic mapping (§4) is calibrated to *observed* repo wall-clock, not an idealized uninterrupted-expert abstraction. Residual conflation is bounded by the published backtest error (§7a). |
| 5 | **Gameable via PR-description/code inflation** — HN: "developers setting up LLM prompts to make their code seem more complex"; "As soon as people know how the metric is calculated, they will game it". | Description is untrusted input — diff-first judging with a bounded feature schema, no free-text path to hours (§4). Boilerplate-fraction is an explicit extracted feature, so padding *lowers* novelty. Person-level invisibility to managers (§6) removes the strongest gaming incentive. Churn discount (§5) claws back inflation that ships as rework. |
| 6 | **Surveillance backlash** — top HN comment: the "AI scored your productivity at 47%" performance-review dystopia. | Person-level data visible only to the person themselves; aggregate-only reporting (weekly/repo/work-type); no manager-facing person rankings — a product invariant (§6). The metric steers spend, not reviews. |
| 7 | **Business impact not measured but not disclaimed prominently** — HN: "If you build something that doesn't solve problems with impact to the business, your real productivity is zero"; founder conceded it is not accounted for. | Explicitly out of scope and *stated* in the metric's definition (§1): the numerator is engineer-hours of equivalent output, never business value. "Wouldn't-have-been-built" work is fenced into a new-capacity category rather than laundered into ROI (§6). |
| 8 | **Only authored PRs counted** — review, mentoring, and unblocking invisible (HN: the "Jane" comment's uncounted guidance work). | Reviews are first-class scored work items attributed to the reviewer (§4). (Mentoring remains honestly out of scope — stated, per row 7's principle.) |
| 9 | **Opaque single score** — "a black box that takes in data and spits out… something" (HN). | Every score stores its `(rubric_version, judge_model, anchor_set_id)` provenance (§4); every ledger row carries `attribution_confidence` and its split method (§2); anchor sets are pinned by `head_sha`/`mined_at` (§3b). Every number decomposes into inspectable parts. |

## 9. Router-label side-effect

Every scored outcome emits a training row for the model-recommendation feature the MCP server
already gestures at (README.md: "Recommend a model for a code-review task — based on what worked
for us before"). Defining the schema *now* means labels accumulate from the first scored PR:

```
RouterLabel {
  work_type:            String     // the org-learned activity vocabulary
  repo_slug:            String
  model:                String     // primary model of the attributed sessions
  provider:             String
  tokens:               TokenUsage // per-class counts, as on the wire today (schema.rs)
  cost_usd:             f64        // attributed dollar share (§2 split)
  effort_p10/p50/p90:   f64        // judged hours (§4)
  realized_multiplier:  f64        // §5, as of labeling
  attribution_confidence: f64      // §2
  rubric_version / judge_model / anchor_set_id   // §4 provenance triple
  merged_at:            String
}
```

This is a server-side derived table (nothing new on the device wire); rows are refreshed when
multipliers mature (§5) or scores re-version (§4). The eventual recommender is out of scope
here — the point is that the ledger's exhaust *is* the training set, and it starts accumulating
in P2, not after a future schema migration.

## 10. Rollout phases

| Phase | Ships | Gate to next |
|---|---|---|
| **P1 — Ledger (no LLM)** | §2 join on existing `SessionMetadata` + §3a `commits`; cost splitting; abandoned-spend category; dashboard shows spend-per-merged-PR and abandoned share. Deterministic end-to-end. | Join coverage: ≥70% of spend attributable on pilot orgs. |
| **P2 — Judge + anchors + backtest** | §3b `repo_anchors` mining; §4 judge; §7a backtest published on dashboard; router labels (§9) start accumulating. | Backtest ρ acceptable on pilot repos; interval calibration honest (p10–p90 covers ~80% of held-out anchors). |
| **P3 — Realized multipliers** | §5 revert/churn/defect discounts; provisional-value windows in UI. | Discounts stable across a full window on pilot data. |
| **P4 — Continuous calibration** | §7b spot labels, §7c natural experiment, §7d drift-triggered re-scoring. | Channels (a)/(b)/(c) agree within stated error on pilots. |
| **P5 — Recommendations** | Model/work-type routing trained on §9 labels. | — |

Device-side, only P1/P2 touch this repo (§3's two additive wire types); P3 consumes signals the
daemon already ships (`merged`/`reverted`/`FileRef` — `references.rs`, `git_outcome.rs`). The
privacy boundary is unchanged in every phase: one uploader crate, one schema file, public shapes
only (README.md, "Audit it yourself").
