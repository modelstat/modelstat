# RFC — The work ledger: measured primitives, and no score

Status: **accepted, and narrowed by measurement** · Scope: the contract between the open-source
daemon (this repo) and the closed-source server/self-hosted engine that reports *what the AI spend
bought*.

This document has been rewritten twice by its own measurements.

The first draft proposed calibrating an effort judge against anchors mined from each repo's
*pre-AI era*. That was implemented, run against real repositories, and failed three times in three
different ways (§1). The third failure is fatal to the whole class of approach: **local git
contains no reliable per-PR human-effort ground truth.**

The second draft replaced the central claim with a two-tier estimator — dimensionless "effort
units" always, hours only once the customer had hand-labelled enough PRs to calibrate against.
That was also implemented, and it is also gone. It failed for a different reason, and not a
statistical one: **it was a verdict wearing data's clothes** (§2). Every weight in it was our
opinion about what counts as work, imposed on a customer who never agreed to it.

What is left is this document's whole thesis, and it is deliberately small:

> **We report measured primitives. We never blend them into a score.**
> Tokens, time, authorship, lifecycle and change size are things that happened, each countable,
> each traceable to the rows that produced it. What they are worth is the reader's judgement, not
> ours.

Everything device-side here is grounded in code in this repo. Server-side sections are design;
they bind the server to the device contract, not the reverse. Privacy invariant throughout: the
daemon's wire schema (`daemon/crates/modelstat-wire/src/schema.rs`) is the *only* thing the
uploader can send (`daemon/crates/modelstat-ingest/` is the single outbound channel — README.md,
"Privacy & data handling"), and every type here stays in the public-shape safety class already
established by `RepoRef`/`FileRef`: slugs, PR numbers, commit SHAs, timestamps, integer counts.
No file contents, no prompts, no home paths, no author identity.

---

## 1. What we measured, and what it ruled out

Three hypotheses about deriving human effort from git, each implemented and run against real
repositories on disk (`daemon/crates/modelstat-parsers/examples/anchor_probe.rs` is the
instrument: it runs the real miner over a repo path and prints what it found).

| # | Hypothesis | What we ran | Result | Verdict |
|---|---|---|---|---|
| 1 | **A fixed "pre-AI era" date cutoff selects a human baseline.** Default `2022-06-01`, env-overridable — predates Copilot-everywhere and ChatGPT. | Mine anchors on erpc, prism, modelstat. | **Zero anchors on every repo.** First commits: erpc `2024-05-06`, prism `2026-07-06`, modelstat `2026-06-14`. Every repo was *created* after the cutoff, so the whole of every history is "AI era" by that rule and the baseline is empty everywhere. Worse, the failure is silent: "0 anchors" is indistinguishable from "not mined". | **Killed.** Adoption is a per-commit fact, not a calendar fact. Replaced by AI-trailer detection (§4). |
| 2 | **Per-PR commit clustering yields per-PR effort.** Cluster a PR's own commit timestamps into sittings; the summed spans are minutes of work. | Mine, then count anchors that carry a usable per-PR commit range. | **0–17% coverage: erpc 0%, modelstat 1%, goldsky-infra 12%, prism 17%.** Cause is structural, not sampling: squash-merge is the dominant convention, a squash commit has ONE parent, and the branch it flattened is not in the repo's history at all — there is no range to cluster (`merge_range` returns `None` for a single-parent merge, `daemon/crates/modelstat-parsers/src/git_anchors.rs`). | **Killed as a general source.** On the repos where this product's users work, the signal is absent for 83–100% of PRs. |
| 3 | **Author-stream commit intervals are the high-coverage fallback.** Cluster each author's whole commit stream instead of one PR's branch; attribute intervals to the PRs they fall in. | Interval clustering over each author's commit stream on erpc, prism, modelstat. | **95–100% coverage, and useless.** It saturates — on erpc `p50 = p90 = 120 min`, the per-interval ceiling, so the statistic reports the cap rather than the work. And it barely tracks the thing effort must track: **Spearman ρ against change size = 0.11 (erpc, n=27), 0.24 (prism, n=58), 0.12 (modelstat, n=87)**, against **≈0.30 for plain lines-of-code**. | **Killed, decisively.** The highest-coverage timing signal carries *less* information about the change than counting lines does. |

**Conclusion, stated plainly: git records when commits were written, not how long the work took.**
The gap between those two is not noise that averages out — it is review latency, meetings, and the
thinking that left no commit, plus the batching habits of whoever pressed the button. Finding 3 is
the decisive one and deserves restating on its own terms: an "hours" number derived from commit
timing correlates with the size of the change at **ρ ≈ 0.11–0.24**, while simply counting the
changed lines correlates at **≈0.30**. The elaborate derivation is *worse than the trivial one*.
A vendor reporting hours-saved from git alone is not approximating the number — it is fabricating
it, because the input data does not contain it.

This is a fact about the data, not a gap in the implementation, so no further work on the miner
recovers it.

## 2. Why there is no score

Finding §1 killed *hours*. It did not, by itself, kill a dimensionless effort index, and for one
iteration we shipped one: a weighted blend of churn, file count, hunk scatter and language spread,
optionally blended 50/50 with an LLM judge's placement, normalised to the repo's median
human-authored PR. It is deleted — the crate that computed it, the labels that calibrated it, the
judge seam it called, and every column that printed it.

Two reasons, and neither is statistical.

**Each company values different things.** The index had weights: source churn counted 1.0, tests
0.8, config 0.5, docs 0.2, generated 0.02; churn was worth 0.55 of the log-score and file count
0.20. Every one of those numbers is an opinion about what engineering work is worth, and we do not
hold it on the customer's behalf. A platform team that measures itself on deleted code, an infra
team whose quarter is 4,000 lines of Terraform, a docs-heavy developer-tools company — the same
weights insult all three differently. There is no defensible universal setting, and a
per-customer settings page for it is just asking them to author the verdict we would then hand
back to them as a finding.

**A blended number is a verdict people cannot trust or audit.** The most common question asked of
this class of product is not "is this useful?" — it is *"how do I know this number is correct?"*
Against a primitive, that question has an answer: here are the sessions, here are the commits,
add them up. Against a composite it has no answer at all, only an explanation of our weights, and
that conversation is about our judgement rather than about their work. Worse, the number lands on
people. "Your PR scored 0.4" is a performance claim we manufactured and cannot defend; "your PR
changed 3 files and 40 lines, and 90 minutes of agent time went into it" is a fact the author can
confirm or correct.

So the product's differentiator is negative space: **we refuse to produce the number the category
is built on.** Tokens and time are concrete things that really happened. We show them, next to
what shipped, and stop.

Three rules fall out, and they are enforced in code rather than in review:

1. **No composite, at any layer.** Not a score, not an index, not a weighted rollup, not a
   "productivity" column, not a default sort by one. A percentile of a *measured* quantity is
   fine — a percentile of a blend is a verdict in a lab coat.
2. **Ranking is the reader's.** `modelstat roi --sort` orders by one column the user named
   (`files`, `added`, `deleted`, `lines`, `sessions`, `tokens`, `active`, `pr`, or `recent`).
   The default is `recent` — git's merge order — because it asserts nothing.
3. **Spend sits beside outcomes, never multiplied into them.** No "hours saved", no "value
   delivered", no dollars at all unless the user supplies a rate (`--usd-per-mtok`; the device
   holds no price table, and a stale one quietly invents money). Unattributed spend is always
   reported. Unknown renders as `—`, never as a zero that looks like success.

## 3. What is measured

Five families. Every one is a count of something that happened, and none of them are combined.

| Family | Quantities | Where it comes from | Status |
|---|---|---|---|
| **Tokens** | The five disjoint classes (`input`, `output`, `cache_creation`, `cache_read`, `reasoning`) per session, plus `equiv_tokens` — the classes weighted into fresh-input equivalents. | Parsed on-device from the tool transcripts; `TokenMix` in `daemon/crates/modelstat-work/src/attribution.rs`. | Shipped. |
| **Time** | `active_ms` — the union of 5-minute windows around a session's event timestamps, so idle gaps do not count (`attribution::active_ms`, `ACTIVITY_WINDOW_MS`). The server additionally derives `agent_working_ms` (developer waiting on the agent) and `waiting_on_user_ms` (agent blocked on a human) from turn-level message timing the daemon does not model. | On-device from event timestamps; server-side from the time plane. | Shipped (device: `active_ms` only). |
| **Authorship** | AI-assisted vs human-authored, per merged PR. | `is_ai_authored` reads the tools' own commit trailers (§4). A read of a string that is either present or absent. | Shipped. |
| **Lifecycle** | Merged, reverted, and sessions that reference no outcome that ever merged (§6). | `check_pull_request_outcome` / `is_reverted`, `daemon/crates/modelstat-parsers/src/git_outcome.rs`. | Shipped. |
| **Change primitives** | `files_changed`, `lines_added`, `lines_deleted`, `commits_count`, and — on the device only — churn split by path class (test / config / doc / generated). | Two readings of the same bounded `git show -m --first-parent --numstat`. What ships: `git_outcome::measure_pr_change` folds it through `git::parse_numstat_totals` — the crate's one totals parse, which matches the path column and never captures it — into the four counts that ride `PullRequestRef` to the server. What stays: `modelstat-work/src/diff.rs` keeps the row-level parse `modelstat roi` needs, because classifying a path means reading it (`DiffFeatures` is deliberately not `Serialize`, so no path can reach a wire through that crate). A hunk count was also read here and is gone: it needed a second, far larger `git show` — 52% of the git time on a 120-merge walk — and nothing consumed it. | Shipped. |

**One pair of quantities exists server-side and not on the device.** The daemon does not compute
`agent_working_ms` / `waiting_on_user_ms`, because turn-level wait needs message timing it does
not model.

**`commits_count` is measured only where a branch survives to count.** A merge commit has two
parents, so `sha^1..sha^2` is the PR's own commits — the same set the forge lists, counted from
the history already on disk. A squash or rebase merge has one parent and the branch it flattened
is gone: there the daemon reports NOTHING rather than the constant `1` a first-parent walk would
see, which would be a fabrication wearing the costume of a measurement (§1 finding 2 measured
squash-merging repos at 0–17% branch-range coverage, so that constant would be most of the
column). The numstat beside it is still real and still ships — a squash merge's diff is exactly
what landed. The server, when a forge integration is connected, can count commits for the
squashed case too, because the forge remembers the branch.

So the device and the dashboard agree on files and lines everywhere, and agree on commits
wherever the merge kept its branch. **Where they differ, the device is the one saying less.**

**No absence names a reason, and none may be read as one.** An omitted `commits_count` means the
local history could not count it; a NULL server-side means *not measured* and nothing further:
one bucket over several causes — no forge integration connected, a PR seen only through a list
endpoint that omits the field, a detail fetch that failed or timed out, a per-run fetch budget
exhausted before reaching that PR — with no column recording which. Inferring the cause is a bug
with a friendly face: "NULL ⇒ tell them to connect an integration" is wrong for the user who has
one connected and hit a timeout, and wrong in the direction that looks actionable. Telling those
states apart needs a provenance column somebody would have to add, and is never an inference from
the NULL.

What the two absences do share is the only thing they should: both refuse to fabricate a number.
That is also why these columns are nullable rather than `NOT NULL DEFAULT 0` — a default would
bake in the exact failure mode this document exists to refuse, an unobserved quantity rendering
as a figure somebody can act on.

**`equiv_tokens` is a normalisation, not a new measurement, and never appears alone.** Raw token
counts are not comparable between PRs: cache reads are 92.3% of raw volume on a measured device
(1,606 turns) and are re-counted every turn, so a raw sum ranks PRs by conversation length rather
than by work. The equivalent weights the classes against each other (`W_INPUT` 1.0, cache-write
1.25, cache-read 0.1, output 5.0) — Anthropic-family list *ratios*, published in the `--json`
output so a consumer on another provider can see exactly what was applied and re-derive the figure
from the raw classes printed beside it. This is the one derived number in the system, it is
labelled `eq` everywhere it appears, and the classes it came from are never more than a line away.

**Time is attributed by the same weights as tokens.** A session that gave a PR a third of its
tokens gave it a third of its active time. That identity is not cosmetic: if the two were split
differently, time and tokens could disagree about which PR a session belonged to, and a reader
reconciling the two columns would find a discrepancy neither number could explain.

**Time is never surfaced against shipped work as a ratio.** `active_ms` per PR is reported.
`active_ms per line`, `lines per hour`, and anything of that shape are not, and adding one would
re-introduce §2's problem through the back door — a rate is a composite with the denominator
hidden in the units.

## 4. Authorship: human PRs detected by the tools' own trailers

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

The asymmetry dictates the conservative rule: a false positive shrinks the human population, a
false negative merely adds one slightly-optimistic anchor. We would rather miss an AI PR than
invent a human one.

**PR-number extraction is structural only** (`pr_number_from_subject`): `Merge pull request #123
from …`, or a trailing `(#123)`. A prose `#123` is deliberately not a merge. `git_outcome.rs` does
read a bare `#123` and is right to — it is checking a PR the session already named. Mining has no
such witness, and "fix bug reported in #123" would invent a row out of a sentence.

**Classification is over all of a PR's commits**, not just the merge commit: the branch range for
a true merge, the squashed body for a squash merge. One AI commit makes the PR AI-assisted.

**What the mine actually finds** (probe runs, `anchor_probe.rs`; the walk stops at the 50-anchor
cap, so both numbers describe the same window):

| Repo | Human anchors | AI-assisted PRs | Window |
|---|---|---|---|
| erpc | 50 (cap reached) | 52 | 103 merged-PR candidates |
| prism | 50 (cap reached) | 6 | 57 candidates |
| modelstat | 6 | 81 | 89 candidates |

Authorship now has exactly one consumer: **the split itself.** "52 of your last 103 merged PRs
were AI-assisted, and here is what they changed and what they cost" is a complete, defensible
sentence that needs no judge, no labels and no calibration. The second consumer anchors used to
have — the normalisation population an effort index was scored against — went with the index
(§2).

### 4a. `IngestBatch.repo_anchors` — the wire type

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
  slug: String, host: Option<String>, cutoff: Option<String>, mined_at: String,
  head_sha: String, human_anchor_count: u32, ai_pr_count: u32,
  anchors: Vec<AnchorPr>          // cap ANCHORS_PER_REPO_COUNT_MAX = 50
}
IngestBatch.repo_anchors: Option<Vec<RepoAnchors>>   // cap REPO_ANCHORS_COUNT_MAX = 10 repos
```

| Decision | Rationale |
|---|---|
| **`cutoff` is `Option`, null by default.** | The first draft made it mandatory with a `2022-06-01` default. §1 finding 1 is what happened. It survives only as an operator's explicit window (`AnchorConfig::cutoff`), and stays on the wire so whatever was used is auditable per batch. |
| **`human_anchor_count` + `ai_pr_count` are shipped, not derivable.** | The walk stops at the anchor cap, so both counts describe the *same* recent window. `human_anchor_count: 6, ai_pr_count: 81` tells a consumer immediately that the repo is AI-dominated — a fact the anchor list alone cannot carry. |
| **`active_minutes` and `span_ms` stay on the wire and drive nothing.** | They are real observations about the PR. §1 findings 2 and 3 are why nothing consumes them: absent for 83–100% of PRs and, where present, ρ ≈ 0.11–0.24 against change size. Keeping them visible is honest — deleting them would hide the evidence that killed the design; consuming them would repeat the mistake. |
| **Mined on-device, shipped as public shape.** | The alternative — server clones the repo — is exactly the access model this product exists to avoid. The daemon reads history it already reads and ships numbers only. |
| **`head_sha` + `mined_at` on every set.** | A rebase changes `head_sha`; the server treats that as a new anchor set, never a silent mutation. `daemon/crates/modelstat-daemon/src/anchors.rs` uses the same pair as the re-mine gate: HEAD unchanged ⇒ no walk. |

**Mining is gated four ways** (`anchors.rs`): total opt-out via `MODELSTAT_ANCHORS=0`, at most once
per repo per daemon run, only when HEAD moved since the last mine (recorded in `anchors.json`
beside `state.json` — absolute repo paths live there and nowhere else), and at most 10 repos per
batch. Every git call is bounded and timed out (`--first-parent`, `--max-count 2000`, 4s per call,
20s per repo, 60s per batch); any failure yields `None`. A batch never waits on git and never
fails for it.

## 5. The attribution join

The ledger is the join table: `spend rows (sessions) ⟷ outcome rows (merged PRs)`. **No
measurement touched this section** and none of §2's deletions reach it — it is arithmetic over
device facts, and it is where the traceability requirement (§7) is discharged.

**Unit of account: the merged PR.** One ledger row = one (session, outcome) edge carrying a token
mix, an active-time share, and a confidence. Trunk-based teams who merge to main with no PR are
outside the join; the device half for them was drafted and dropped (§5b).

**A session is joined to the PR it AUTHORED, not the one it mentioned.** The first cut joined on
PR references alone — the numbers and URLs the parsers mine out of turn text. That is backwards,
and visibly so: a PR number does not exist while the work is being done. The branch and the
commits come first and the PR is opened afterwards, so a reference in a transcript overwhelmingly
marks a session that DISCUSSED an already-open PR (a review, a follow-up, "look at #1037") rather
than the one that wrote it. On a real device this inverted the headline: AI-authored PRs showed no
spend while human-authored PRs showed all of it.

What survives real repositories is **changed-file overlap × time proximity**. Matching the
session's own commit SHAs would be simpler and does not work: squash-merge rewrites them, so a
session's local shas never appear on mainline. What squashing cannot rewrite is *which files
changed*.

* session side: `git log --since --until --numstat` over the session's window plus a commit grace
  period, the same read the daemon's session-metadata pass already makes;
* PR side: a bounded `git show --numstat -m --first-parent` on the merge commit, cached per
  `(repo, merge sha)` for the whole call.

`file_overlap` is the Jaccard index of the two sets, `time_proximity` decays it with the gap to the
merge, and the product is the match score that both selects the PR and weights the split.
References still matter, but only as a second signal layered on top.

**Splitting one session across N PRs.** The score is the weight; a session that scores 3:1 across
two PRs contributes 3/4 and 1/4 — of its tokens *and*, by the same weights, of its `active_ms`.
Even split 1/N only where there is no file evidence at all.

**Every row carries `attribution_confidence ∈ [0,1]`**, and the CLI surfaces it twice: a `~` on
any row at or below `WEAK_CONFIDENCE = 0.3` (mention-only, little or no file overlap), and in the
rollup as both a token-weighted mean *and* the share of attributed volume resting on weak matches.
The mean alone hides distribution — 0.72 can be nine certain PRs, or one certain PR beside one
large guess — and past 50% weak volume the rollup says so in words.

**Unattributed spend is a first-class figure, not a residual.** Sessions that resolve to no PR —
exploration, reading, ops work, repos not on this disk — accrue to an explicit device-wide bucket
reported with tokens *and* active time beside every per-PR figure. On a real machine it is large,
and hiding it would make every attributed number look better than it is. Exploration is not waste,
but it is also not a shipped outcome.

### 5b. Rejected — per-session commit capture

A `SessionMetadata.commits` array (one sha + timestamp per commit in the session's window) was
drafted to give trunk-based teams a unit of account, and briefly shipped as a device type. It is
gone. The server attributes shipped work per-PR and drops the field on ingest, so the capture was
an extra bounded `git log` per repo per session producing data nothing reads. Direct-to-main work
stays outside the ledger until the server grows a commit-range outcome row; the device half is
small and can return with it.

## 6. Lifecycle is reported as a state, not as a discount

Merged, reverted, and referenced-but-never-merged are all local git reads with their evidence
shipped alongside (`git_outcome.rs`; `merge_sha`/`merge_subject`/`merge_method` on
`PullRequestRef`; `is_reverted` matches `git revert`'s "This reverts commit" body). The server
trusts the device signal because the evidence rides along — it can check the convention rather
than take the boolean on faith.

An earlier draft turned these into **multipliers**: reverted × 0.0, rapid-churn and defect-linkage
discounts floored at 0.25, composed multiplicatively onto the effort index. That is deleted with
the index. The floor of 0.25 was invented; so was the 30-day window; so was the choice to discount
rather than to report. "This PR was reverted" is a fact. "This PR is worth 25% of what it looked
like" is a verdict, and it is exactly the kind a customer cannot audit.

So: reverted PRs are **reported as reverted**, alongside their tokens and time, and the reader
decides whether that spend was a loss or the cost of finding out. Rapid re-churn on the same files
may return later as a *reported* number — "38% of this PR's added lines were rewritten within 30
days" — but never as a coefficient applied to something else.

## 7. Traceability is a requirement, not a feature

**Every number must be traceable to the rows that produced it.** This is the property a composite
cannot have, and it is why the primitives are worth the loss of a headline figure. Concretely:

* **Per-PR figures decompose into sessions.** The server exposes
  `GET /v1/analytics/roi/explain?work_id=…`, which returns every contributing session with its
  raw tokens, the split weight `k` it was given, and the tokens and active-ms that weight
  contributed — so a reader can re-add the column themselves and find the session responsible for
  a figure that looks wrong.
* **Unattributed spend has the same door.** `?unattributed=true` lists the sessions that resolved
  to no work item, with tokens and active time. A total nobody can enumerate is a total nobody
  should believe.
* **The device does the same offline.** `modelstat roi --json` carries, per PR, the five raw token
  classes, the raw total, the equivalent, the session count, the active-ms and the
  `attribution_confidence` — plus the `equiv_weights` themselves and `weak_confidence_threshold`,
  so the human table's `~` marks are reproducible rather than mysterious.
* **Absence is explicit.** Change primitives are `null` — never `0` — when git could not read a
  merge's diff, and every group total publishes `diffs_read` beside `prs` so a consumer can see
  that a sum covers three of five PRs before dividing by five.

## 8. Reporting rules

| Rule | Rationale |
|---|---|
| **Aggregate-level by default**: weekly × repo × work-type. | The data is built to steer *spend allocation* (which models, which work-types, which repos), not people. |
| **Person-level data is visible ONLY to the person themselves.** | Self-coaching ("my unattributed share is 3× the team's") is valuable; manager-facing person rankings are a Goodhart engine. A product invariant, not a settings default. This mattered more when there was a score to rank on; it still holds now that there is only spend. |
| **Cross-repo sums are limited to what is actually additive.** | Tokens, time and counts are. A 400-line change means something different in a Rust daemon and a Terraform monorepo, so change primitives are compared within a repo and never pooled into a cross-repo "output" figure. |
| **No dollars without an explicit rate.** | The daemon holds no price table by design (pricing is a server concern; a stale table invents money). `--usd-per-mtok` prices the *equivalent*, because a rate quoted per million input tokens applied to a raw sum that is 92% cache reads invents roughly 5× the spend. |
| **Business impact is out of scope and says so.** | The numerator was never business value, and no arrangement of these primitives becomes one. |

## 9. Prior art, and what we actually differ on

Weave (workweave.ai, YC — ["Weave Hour": a standardized unit of engineering output, approximately
one hour of work by an expert software engineer](https://www.ycombinator.com/companies/weave-3)) is
the closest prior art and drew a public critique thread:
[Show HN, Nov 2024](https://news.ycombinator.com/item?id=42196381).

Most of the critique is about the score, and our answer to most of it is now the same sentence:
there is no score. That is a narrower claim than the one this document used to make, and a
defensible one.

| Shortcoming (public record) | Our position |
|---|---|
| **Point estimates with false precision** — example scores published to three decimals ("15.266" hours for a PostHog PR). | We publish no estimate to be precise about. Counts are exact; the one derived figure (`equiv_tokens`) ships with the raw classes and the weights that produced it. |
| **Ground truth = the vendor's proprietary corpus** — founder, on the 0.94 correlation: *"Evaluated on a proprietary data set of manually labelled PRs"*. Unverifiable by the customer. | Nothing is calibrated, so there is no corpus to disclose or hide. Our own labelling route — customer labels, on-device, with published LOOCV error — was better than a vendor corpus and is *still* deleted, because the thing it calibrated should not exist (§2). |
| **Wall-clock vs "isolated complexity hours" conflation** — a "15h" PR that demonstrably took ~2 weeks of elapsed collaborative work. | We report two clocks and never merge them: `active_ms` (union of 5-minute windows, idle excluded) and, server-side, `agent_working_ms` / `waiting_on_user_ms`. None of them claims to be "how long this would have taken a human", which is the quantity §1 shows is not in the data. |
| **Gameable via PR-description or code inflation.** | Inflating a diff moves `lines_added`, visibly, in a column labelled `lines_added`. There is no scoring function to fool, and no free-text path into any number — the judge that read structure-only diff excerpts is deleted along with the score it fed. |
| **Surveillance backlash** — the "AI scored your productivity at 47%" performance-review dystopia. | This is the failure mode §2 is written against. No score exists to put in a review; person-level data stays with the person; the numbers steer spend, not people. |
| **Opaque single score** — "a black box that takes in data and spits out… something". | There is no single number to open. Every figure decomposes: per-PR spend into contributing sessions with their weights (§7), the equivalent into its five raw classes and published ratios, the join into a confidence with a stated threshold. |
| **Only authored, shipped work is counted** — review, mentoring and unblocking invisible. | **Not countered — we share this shortcoming**, and unattributed spend (§5) is where most of it lands: reported as a real, large number rather than quietly redistributed onto the PRs we *can* see. Read every attributed figure as covering authored, shipped work only. |

## 10. Device surface, as built

| Component | What it is |
|---|---|
| `daemon/crates/modelstat-work/` | Two modules, no socket. `diff.rs` reads a merge's change primitives locally, classifying paths and then dropping them (`DiffFeatures` is not `Serialize`, so no path or source text can reach a wire through this crate). `attribution.rs` is the §5 join, plus `active_ms` / `ACTIVITY_WINDOW_MS` as the citable definition of active time. |
| `modelstat roi` | The device-side view: per merged PR — authorship, files changed, lines +/−, sessions, input-equivalent tokens (with the raw mix in the rollup) and active time. `--sort` over any of those columns, `--json` for the full document, `--usd-per-mtok` for the only dollars that exist. `--help` states in as many words that this reports measured quantities and does not score anyone. |
| `IngestBatch.repo_anchors` | §4a. |
| `SessionMetadata.pull_requests[].{files_changed,lines_added,lines_deleted,commits_count}` | The four counts `measure_pr_change` reads off the merge commit the outcome check already found, so the server's change columns are populated by local git alone — a forge integration becomes a second source, not the only one. Omitted, never zeroed, when the local repo cannot say. |

What was deleted with the score, so it does not come back by accident: `units.rs` (the composite),
`calibrate.rs` (label-fitted hours + LOOCV), `judge.rs` (the `Scorer` seam and its prompts),
`labels.rs` (the on-device label store), and the `modelstat label` command that fed them. The crate
that held them is now `modelstat-work`, renamed for the same reason: *effort* was the estimand, and
there is no longer an estimand — only a ledger.
