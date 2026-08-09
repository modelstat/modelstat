/**
 * Per-session deterministic metadata — the repos, pull requests, commits,
 * and issues a single AI-coding session touched.
 *
 * This is the join layer between AI *spend* (events/segments/tokens) and
 * development *outcomes* (PRs merged, commits landed, tickets closed). The
 * daemon attaches one {@link SessionMetadata} per session to the ingest
 * batch; the server can later join it to GitHub/GitLab/Linear/Jira to answer
 * "what did this spend actually ship?".
 *
 * Everything in this module is **pure and dependency-free** (zod only) so it
 * can run in any daemon runtime (Node CLI, browser extension) and be
 * exhaustively unit-tested without a model, a network, or a filesystem.
 *
 * Three detection channels feed it, each stamped on every reference as a
 * `source` for trust + precedence:
 *   - `git`     — resolved from the repo on disk / the event's git context
 *                 (highest trust: deterministic, not user content).
 *   - `tool`    — extracted from a tool call (e.g. a `gh pr create` result).
 *   - `content` — pattern-matched from a redacted excerpt / segment abstract.
 *   - `model`   — surfaced by the on-device LLM from session content, then
 *                 re-parsed deterministically here (works for ANY provider,
 *                 even ones whose logs carry no structured git data).
 *
 * Privacy: only public reference shapes ride this — `org/repo`, PR/issue
 * numbers, commit SHAs, ticket keys, and the URLs that contain them. Raw
 * prompts, code, and home paths never reach here (the text handed in has
 * already been through redaction).
 */
import { z } from "zod";

/** How a reference was detected. Drives dedupe precedence (see
 * {@link dedupeSessionMetadata}): a deterministic `git` hit beats a
 * `content`/`model` guess for the same entity. */
export const REF_SOURCES = ["git", "tool", "content", "model"] as const;
export const RefSource = z.enum(REF_SOURCES);
export type RefSource = z.infer<typeof RefSource>;

/** Trust ranking — higher wins when the same entity is seen twice. */
const SOURCE_RANK: Record<RefSource, number> = { git: 3, tool: 2, content: 1, model: 0 };

/** A git host + `org/repo` the session worked in, with every branch seen.
 * Plural by design: one session can span several repos and branches. */
export const RepoRef = z.object({
  /** `github.com`, `gitlab.com`, … — null when only the slug is known. */
  host: z.string().max(80).nullable().default(null),
  /** `org/repo`. The stable identity of the repo. */
  slug: z.string().max(200),
  /** Every branch observed for this repo this session. */
  branches: z.array(z.string().max(200)).max(50).default([]),
  source: RefSource.default("content"),
});
export type RepoRef = z.infer<typeof RepoRef>;

/** A pull/merge request the session referenced. */
export const PullRequestRef = z.object({
  host: z.string().max(80).nullable().default(null),
  slug: z.string().max(200).nullable().default(null),
  number: z.number().int().positive(),
  url: z.string().max(400).nullable().default(null),
  source: RefSource.default("content"),
  confidence: z.number().min(0).max(1).default(0.9),
  // On-device verified-outcome signals — filled by the parsers' local git-check
  // (`checkPullRequestOutcome`) when the PR's repo is on disk. `null` = unknown;
  // the server's CPVO engine classifies verified/failed from these. Mirrors
  // core's `PullRequestRef` field-for-field.
  merged: z.boolean().nullable().optional(),
  merged_at: z.string().max(40).nullable().optional(),
  reverted: z.boolean().nullable().optional(),
  hotfixed: z.boolean().nullable().optional(),
  reopened: z.boolean().nullable().optional(),
  // The evidence behind `merged`. It is decided from a CONVENTION — a commit
  // subject mentioning `#<n>` — which is wrong in both directions ("Fix bug
  // reported in #123" merged nothing; a custom squash template merges without
  // the number). These three ship the matched commit (public repo facts) and
  // the name of the reading, so the server can weigh the claim instead of
  // taking it. Absent when nothing matched, and from daemons predating them.
  merge_sha: z.string().max(64).optional(),
  merge_subject: z.string().max(400).optional(),
  merge_method: z.string().max(40).optional(),
});
export type PullRequestRef = z.infer<typeof PullRequestRef>;

export const ISSUE_PROVIDERS = [
  "github",
  "gitlab",
  "bitbucket",
  "linear",
  "jira",
  "other",
] as const;

/** An issue / ticket the session referenced. `key` is the provider-native
 * id: a number string for GitHub/GitLab issues, `TEAM-123` for Linear/Jira. */
export const IssueRef = z.object({
  provider: z.enum(ISSUE_PROVIDERS).default("other"),
  key: z.string().max(80),
  slug: z.string().max(200).nullable().default(null),
  url: z.string().max(400).nullable().default(null),
  source: RefSource.default("content"),
  confidence: z.number().min(0).max(1).default(0.8),
  /** The observed text does not identify WHAT this reference is. Two shapes set
   * it: `org/repo#N`, which GitHub resolves to either a PR or an issue (the
   * daemon files it as an issue and says so), and a bare `TEAM-123`, which may
   * be no ticket at all (`UTF-8`, `SHA-256`). A URL never sets it. Absent —
   * not `false` — when the reference is unambiguous. */
  ambiguous: z.boolean().optional(),
});
export type IssueRef = z.infer<typeof IssueRef>;

/** A file a session changed, with the lines added/deleted across the session's
 * window (git `--numstat`). The raw signal behind the per-file *token re-spend*
 * heatmap: a path that keeps reappearing across sessions is re-spend — a likely
 * redesign candidate. Always `git`-sourced + repo-relative, the same public-shape
 * safety class as a slug (no file contents, no home paths). */
export const FileRef = z.object({
  /** `org/repo` this file belongs to — ties the path to a repo for the per-repo
   * rollup (one session can span several repos). Null when the slug is unknown. */
  slug: z.string().max(200).nullable().default(null),
  /** Repo-relative path. Never absolute / home — git emits repo-relative. */
  path: z.string().max(400),
  lines_added: z.number().int().nonnegative().default(0),
  lines_deleted: z.number().int().nonnegative().default(0),
  source: RefSource.default("git"),
});
export type FileRef = z.infer<typeof FileRef>;

/** A commit the session produced — the session-side half of the spend→outcome
 * join (the pre-AI baseline rides the batch as `RepoAnchors`). Same public
 * safety class as a slug (sha, timestamp, provenance — no messages, no file
 * contents, no author identities). */
export const CommitRef = z.object({
  /** `org/repo` this commit landed in. Null when the slug is unknown. */
  slug: z.string().max(200).nullable().default(null),
  /** Full or abbreviated hex sha. */
  sha: z
    .string()
    .min(7)
    .max(64)
    .regex(/^[0-9a-fA-F]+$/),
  /** ISO-8601 commit timestamp. */
  committed_at: z.string().max(40),
  source: RefSource.default("git"),
});
export type CommitRef = z.infer<typeof CommitRef>;

/**
 * The deterministic metadata for one session. Attached to the ingest batch
 * under `session_metadata[session_id]`. Every collection is plural and
 * capped; an empty {@link SessionMetadata} should simply not be shipped.
 */
export const SessionMetadata = z.object({
  repos: z.array(RepoRef).max(50).default([]),
  pull_requests: z.array(PullRequestRef).max(100).default([]),
  issues: z.array(IssueRef).max(100).default([]),
  files: z.array(FileRef).max(500).default([]),
  commits: z.array(CommitRef).max(100).default([]),
});
export type SessionMetadata = z.infer<typeof SessionMetadata>;

/** The per-event form of {@link SessionMetadata}: the references found in a
 * single turn's text. Smaller caps — one turn references few things — and it
 * rides each {@link RawEvent} (`references`) so the server can attribute a ref
 * to the exact turn/segment that touched it, not just the session. */
export const EventReferences = z.object({
  repos: z.array(RepoRef).max(24).default([]),
  pull_requests: z.array(PullRequestRef).max(24).default([]),
  issues: z.array(IssueRef).max(24).default([]),
});
export type EventReferences = z.infer<typeof EventReferences>;

/** The mutable accumulation shape the detectors emit. Same fields as
 * {@link SessionMetadata}; {@link dedupeSessionMetadata} folds many of these
 * into one validated, deduped, capped {@link SessionMetadata}. */
export interface DetectedRefs {
  repos: RepoRef[];
  pull_requests: PullRequestRef[];
  issues: IssueRef[];
}

export function emptyDetectedRefs(): DetectedRefs {
  return { repos: [], pull_requests: [], issues: [] };
}

// ── Detection patterns ───────────────────────────────────────────────
//
// All global + case-insensitive on the host so `matchAll` iterates every
// occurrence. Slugs allow dots/dashes; GitLab paths allow nested subgroups
// (`group/subgroup/repo`) which is why the GitLab patterns capture up to the
// `/-/` separator. Ticket keys are intentionally uppercase-anchored.

const GITHUB_PR = /https?:\/\/github\.com\/([\w.-]+)\/([\w.-]+)\/pull\/(\d+)/gi;
const GITLAB_MR = /https?:\/\/gitlab\.com\/([\w./-]+?)\/-\/merge_requests\/(\d+)/gi;
const BITBUCKET_PR = /https?:\/\/bitbucket\.org\/([\w.-]+)\/([\w.-]+)\/pull-requests\/(\d+)/gi;
const GITHUB_ISSUE = /https?:\/\/github\.com\/([\w.-]+)\/([\w.-]+)\/issues\/(\d+)/gi;
const GITLAB_ISSUE = /https?:\/\/gitlab\.com\/([\w./-]+?)\/-\/issues\/(\d+)/gi;
const LINEAR_ISSUE = /https?:\/\/linear\.app\/[\w.-]+\/issue\/([A-Z][A-Z0-9]*-\d+)/gi;
const JIRA_ISSUE = /https?:\/\/[\w.-]+\/browse\/([A-Z][A-Z0-9]+-\d+)/gi;
/** `org/repo#123` — the GitHub shorthand. Safe enough to scan in any text:
 * it requires a slash, a hash, and digits. Anchored on a boundary so it
 * doesn't fire mid-word. */
const SLUG_HASH = /(?:^|[\s([{<])([\w.-]+\/[\w.-]+)#(\d+)\b/g;
/** A bare `TEAM-123` ticket key. High false-positive risk in free text
 * (`UTF-8`, `COVID-19`), so this is gated to the `model` + branch channels
 * where the surrounding instruction makes it intentional. */
const BARE_TICKET = /\b([A-Z][A-Z0-9]{1,9}-\d{1,6})\b/g;

function repoFrom(host: string | null, slug: string, source: RefSource): RepoRef {
  return { host, slug, branches: [], source };
}

/**
 * Extract every repo / PR / commit / issue reference from a blob of text.
 *
 * The `source` tags every reference and also gates the noisier patterns:
 * bare `TEAM-123` ticket keys are only mined when `source === "model"` (the
 * on-device model is explicitly asked for them), never from raw `content`
 * excerpts where they'd collide with `UTF-8`-style strings. Full URLs and the
 * `org/repo#123` shorthand are always safe and always scanned.
 *
 * Every URL-bearing reference also contributes its `(host, slug)` to `repos`,
 * so a single PR link yields both the PR and the repo it belongs to.
 */
export function detectReferences(text: string, source: RefSource = "content"): DetectedRefs {
  const out = emptyDetectedRefs();
  if (!text) return out;

  for (const m of text.matchAll(GITHUB_PR)) {
    const slug = `${m[1]}/${m[2]}`;
    out.pull_requests.push({
      host: "github.com",
      slug,
      number: Number(m[3]),
      url: m[0],
      source,
      confidence: 0.95,
    });
    out.repos.push(repoFrom("github.com", slug, source));
  }
  for (const m of text.matchAll(GITLAB_MR)) {
    out.pull_requests.push({
      host: "gitlab.com",
      slug: m[1] ?? null,
      number: Number(m[2]),
      url: m[0],
      source,
      confidence: 0.95,
    });
    if (m[1]) out.repos.push(repoFrom("gitlab.com", m[1], source));
  }
  for (const m of text.matchAll(BITBUCKET_PR)) {
    const slug = `${m[1]}/${m[2]}`;
    out.pull_requests.push({
      host: "bitbucket.org",
      slug,
      number: Number(m[3]),
      url: m[0],
      source,
      confidence: 0.95,
    });
    out.repos.push(repoFrom("bitbucket.org", slug, source));
  }

  for (const m of text.matchAll(GITHUB_ISSUE)) {
    const slug = `${m[1]}/${m[2]}`;
    out.issues.push({
      provider: "github",
      key: m[3] ?? "",
      slug,
      url: m[0],
      source,
      confidence: 0.95,
    });
    out.repos.push(repoFrom("github.com", slug, source));
  }
  for (const m of text.matchAll(GITLAB_ISSUE)) {
    out.issues.push({
      provider: "gitlab",
      key: m[2] ?? "",
      slug: m[1] ?? null,
      url: m[0],
      source,
      confidence: 0.95,
    });
    if (m[1]) out.repos.push(repoFrom("gitlab.com", m[1], source));
  }
  for (const m of text.matchAll(LINEAR_ISSUE)) {
    out.issues.push({
      provider: "linear",
      key: m[1] ?? "",
      slug: null,
      url: m[0],
      source,
      confidence: 0.9,
    });
  }
  for (const m of text.matchAll(JIRA_ISSUE)) {
    out.issues.push({
      provider: "jira",
      key: m[1] ?? "",
      slug: null,
      url: m[0],
      source,
      confidence: 0.9,
    });
  }

  for (const m of text.matchAll(SLUG_HASH)) {
    // `org/repo#123` is ambiguous between issue and PR on GitHub. Default to an
    // issue (the superset; a real PR URL elsewhere wins on dedupe) — but an
    // explicit PR cue right before it ("PR org/repo#123", "merged org/repo#123")
    // disambiguates toward a PR, so a shorthand-only PR still reaches the
    // first-class PR entity instead of being misfiled as an issue. The cue must
    // be adjacent, so issue cues ("fixes", "closes") never trip it.
    const slug = m[1] ?? "";
    const lead = text.slice(Math.max(0, (m.index ?? 0) - 20), m.index ?? 0).toLowerCase();
    if (/\b(pr|pull[ -]?request|merge[ -]?request|mr|merged)\s*$/.test(lead)) {
      out.pull_requests.push({
        host: "github.com",
        slug,
        number: Number(m[2] ?? ""),
        url: null,
        source,
        confidence: 0.6,
      });
      out.repos.push(repoFrom("github.com", slug, source));
    } else {
      out.issues.push({
        provider: "github",
        key: m[2] ?? "",
        slug,
        url: null,
        source,
        confidence: 0.55,
      });
    }
  }

  if (source === "model") {
    for (const m of text.matchAll(BARE_TICKET)) {
      out.issues.push({
        provider: "other",
        key: m[1] ?? "",
        slug: null,
        url: null,
        source,
        confidence: 0.4,
      });
    }
  }

  return out;
}

/** Mine ticket keys (`TEAM-123`) out of a branch name — a high-signal,
 * low-noise place to find them (`feature/ENG-742-retry-logic`). Returns
 * `git`-sourced issue refs since a branch is deterministic, not content. */
export function detectBranchTickets(branch: string | null | undefined): IssueRef[] {
  if (!branch) return [];
  const out: IssueRef[] = [];
  for (const m of branch.matchAll(BARE_TICKET)) {
    out.push({
      provider: "other",
      key: m[1] ?? "",
      slug: null,
      url: null,
      source: "git",
      confidence: 0.7,
    });
  }
  return out;
}

/** Build a {@link RepoRef} from a git context (the event's `git` field or a
 * `resolveGitContext` result). Null when there's no slug to anchor on. */
export function repoRefFromGit(
  git: { remote_host?: string | null; remote_slug?: string | null; branch?: string | null },
  source: RefSource = "git",
): RepoRef | null {
  if (!git.remote_slug) return null;
  return {
    host: git.remote_host ?? null,
    slug: git.remote_slug,
    branches: git.branch ? [git.branch] : [],
    source,
  };
}

// ── Dedupe / merge ───────────────────────────────────────────────────

function stronger(a: RefSource, b: RefSource): RefSource {
  return SOURCE_RANK[a] >= SOURCE_RANK[b] ? a : b;
}

/** Generic keyed dedupe that keeps the highest-precedence/confidence copy. */
function dedupe<T extends { source: RefSource; confidence?: number }>(
  items: T[],
  keyOf: (item: T) => string,
  merge: (existing: T, next: T) => T,
): T[] {
  const byKey = new Map<string, T>();
  for (const item of items) {
    const key = keyOf(item);
    const existing = byKey.get(key);
    byKey.set(key, existing ? merge(existing, item) : item);
  }
  return [...byKey.values()];
}

/**
 * Fold any number of {@link DetectedRefs} (from every channel + every event/
 * segment of a session) into one validated, deduped, capped
 * {@link SessionMetadata}.
 *
 * Repos dedupe by slug (case-insensitive), unioning branches and keeping the
 * strongest host + source. PRs/issues dedupe by their natural key,
 * keeping the highest-confidence, strongest-source copy — so a deterministic
 * `git` PR URL always beats a low-confidence `model` mention of the same PR.
 */
export function dedupeSessionMetadata(parts: DetectedRefs[]): SessionMetadata {
  const all = emptyDetectedRefs();
  for (const p of parts) {
    all.repos.push(...p.repos);
    all.pull_requests.push(...p.pull_requests);
    all.issues.push(...p.issues);
  }

  const repos = dedupe(
    all.repos,
    (r) => r.slug.toLowerCase(),
    (a, b) => ({
      host: a.host ?? b.host,
      slug: a.slug,
      branches: [...new Set([...a.branches, ...b.branches])].slice(0, 50),
      source: stronger(a.source, b.source),
    }),
  );

  // Higher score wins a tie-break: source trust dominates, confidence breaks
  // ties within a source. The loser still backfills a missing url/slug.
  const score = (r: { source: RefSource; confidence?: number }): number =>
    SOURCE_RANK[r.source] * 2 + (r.confidence ?? 0);

  const pull_requests = dedupe(
    all.pull_requests,
    (p) => `${(p.slug ?? "").toLowerCase()}#${p.number}`,
    (a, b) => {
      const [win, lose] = score(a) >= score(b) ? [a, b] : [b, a];
      return {
        host: win.host ?? lose.host,
        slug: win.slug ?? lose.slug,
        number: win.number,
        url: win.url ?? lose.url,
        source: win.source,
        confidence: Math.max(win.confidence, lose.confidence),
      };
    },
  );
  const issues = dedupe(
    all.issues,
    (i) => `${i.provider}:${(i.slug ?? "").toLowerCase()}#${i.key.toLowerCase()}`,
    (a, b) => {
      const [win, lose] = score(a) >= score(b) ? [a, b] : [b, a];
      return {
        provider: win.provider,
        key: win.key,
        slug: win.slug ?? lose.slug,
        url: win.url ?? lose.url,
        source: win.source,
        confidence: Math.max(win.confidence, lose.confidence),
      };
    },
  );

  // GitHub shares one number space across issues and PRs, so the ambiguous
  // `org/repo#N` shorthand (recorded as a low-confidence issue) is really the
  // PR when a `/pull/N` for the same (slug, number) was also found — drop the
  // phantom issue rather than report both.
  const prKeys = new Set(pull_requests.map((p) => `${(p.slug ?? "").toLowerCase()}#${p.number}`));
  const reconciledIssues = issues.filter(
    (i) => !(i.provider === "github" && prKeys.has(`${(i.slug ?? "").toLowerCase()}#${i.key}`)),
  );

  // Drop any single reference that violates the per-field wire caps (an
  // adversarial or malformed capture — e.g. a 300-char slug from a hostile
  // excerpt) rather than letting one bad ref throw out of here and wipe
  // metadata for the whole session/batch. The schema stays the single source
  // of truth for the caps; the blast radius is just the one bad reference.
  const keepValid = <T>(
    schema: { safeParse: (value: unknown) => { success: boolean } },
    items: T[],
  ): T[] => items.filter((item) => schema.safeParse(item).success);

  return {
    repos: keepValid(RepoRef, repos).slice(0, 50),
    pull_requests: keepValid(PullRequestRef, pull_requests).slice(0, 100),
    issues: keepValid(IssueRef, reconciledIssues).slice(0, 100),
    // Files and commits are git-collected per repo *after* dedupe (like the
    // PR-outcome enrichment), so the fold leaves them empty; the daemon fills
    // them via dedupeFiles / dedupeCommits.
    files: [],
    commits: [],
  };
}

/** Fold a session's {@link FileRef}s (collected per repo from git) into one
 * deduped, capped list: the same `(slug, path)` sums its line counts and keeps
 * the strongest source. Separate from {@link dedupeSessionMetadata} because
 * files aren't mined from the detected `parts` — they're collected after dedupe. */
export function dedupeFiles(files: FileRef[]): FileRef[] {
  const merged = dedupe(
    files,
    (f) => `${(f.slug ?? "").toLowerCase()}:${f.path}`,
    (a, b) => ({
      slug: a.slug ?? b.slug,
      path: a.path,
      lines_added: a.lines_added + b.lines_added,
      lines_deleted: a.lines_deleted + b.lines_deleted,
      source: stronger(a.source, b.source),
    }),
  );
  return merged.filter((f) => FileRef.safeParse(f).success).slice(0, 500);
}

/** Fold a session's {@link CommitRef}s (collected per repo from git) into one
 * deduped, capped list: the same sha (case-insensitive) keeps its first-seen
 * copy, backfilling a missing slug and keeping the strongest source. Like
 * {@link dedupeFiles}, commits aren't mined from the detected `parts` —
 * they're collected after dedupe. */
export function dedupeCommits(commits: CommitRef[]): CommitRef[] {
  const merged = dedupe(
    commits,
    (c) => c.sha.toLowerCase(),
    (a, b) => ({
      slug: a.slug ?? b.slug,
      sha: a.sha,
      committed_at: a.committed_at,
      source: stronger(a.source, b.source),
    }),
  );
  return merged
    .filter((c) => c.committed_at.length > 0 && CommitRef.safeParse(c).success)
    .slice(0, 100);
}

/** True when a {@link SessionMetadata} carries no references — callers use
 * this to avoid shipping (and overwriting server state with) an empty map. */
export function isEmptySessionMetadata(m: SessionMetadata): boolean {
  return (
    m.repos.length === 0 &&
    m.pull_requests.length === 0 &&
    m.issues.length === 0 &&
    m.files.length === 0 &&
    m.commits.length === 0
  );
}

/**
 * Extract + dedupe the public references in ONE event's full text — PR/MR,
 * issue URLs plus the `org/repo#N` shorthand. Returns null when
 * none are found so the caller can leave the optional `RawEvent.references`
 * field off. This is the high-recall counterpart to scanning the ≤320-char
 * excerpt: the parser runs it over the whole turn.
 *
 * PRIVACY: it pulls ONLY public reference shapes (forge URLs, slugs, numbers,
 * ticket keys) — never arbitrary substrings — so it is safe to run over the
 * un-redacted turn text, the same safety class as a repo slug. Bare `TEAM-123`
 * keys are deliberately NOT mined here (that path is gated to the model
 * channel) to avoid `UTF-8`/`SHA-256`-style false positives in prose; ticket
 * keys still arrive via forge URLs and branch names.
 */
export function detectEventReferences(text: string): EventReferences | null {
  if (!text) return null;
  const m = dedupeSessionMetadata([detectReferences(text, "content")]);
  if (isEmptySessionMetadata(m)) return null;
  return {
    repos: m.repos.slice(0, 24),
    pull_requests: m.pull_requests.slice(0, 24),
    issues: m.issues.slice(0, 24),
  };
}
