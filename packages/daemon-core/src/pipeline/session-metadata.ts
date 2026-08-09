/**
 * Per-session metadata pass — assembles the repos, pull requests, issues,
 * and files a session touched, then ships one {@link SessionMetadata} per
 * session on the ingest batch (under `session_metadata[session_id]`).
 *
 * This is the on-device half of the spend→outcome join. It fuses four
 * channels, in descending order of trust (see {@link RefSource}):
 *
 *   1. git context already on each event (`event.git`) — repo slug, host,
 *      branch (the historical branch, captured at session time).
 *   2. `resolveGit` — an injected, best-effort read of the repo on disk for
 *      the session's cwds (authoritative remote slug/host; wires up the
 *      otherwise-dormant `git.ts`). Cwd-cached by the caller.
 *   3. redacted content — PR/issue URLs surviving in the segment
 *      abstracts + event excerpts (deterministic regex, never raw text).
 *   4. the on-device model — a single best-effort call per session over the
 *      redacted abstracts, whose free-text reply is re-parsed deterministically
 *      here. This is what makes detection work for ANY provider, even clients
 *      whose logs carry no structured git data (web chat, Cursor).
 *
 * The detection + dedupe logic itself is pure and lives in
 * `@modelstat/core/session-metadata`; this module only orchestrates the
 * channels and the (optional, best-effort) git + model I/O. Mirrors
 * `buildSessionTitles` in shape: group by session, enrich, fall back
 * gracefully — a model or git hiccup never blocks the batch.
 */
import type { GitContext, RawEvent, Segment } from "@modelstat/core/schemas";
import {
  type DetectedRefs,
  dedupeFiles,
  dedupeSessionMetadata,
  detectBranchTickets,
  detectReferences,
  emptyDetectedRefs,
  type FileRef,
  isEmptySessionMetadata,
  type SessionMetadata,
} from "@modelstat/core/session-metadata";
import { sampleAbstracts, stripCognitionSuffix } from "./title.js";

/** The input every {@link LinkExtractor} adapter accepts — the session's
 * already-summarised, already-redacted abstracts (never raw turns), so the
 * model only ever sees post-redaction text. */
export interface LinkExtractInput {
  abstracts: string[];
}

/**
 * Adapter contract for the on-device link-extraction model. Returns a raw
 * free-text reply (one reference per line, or "none") which the caller feeds
 * back through the deterministic parser — so we never trust model-structured
 * output, only model-surfaced *text*. Returns null when the runtime can't
 * produce anything; callers treat null as "no signal".
 */
export type LinkExtractor = (input: LinkExtractInput) => Promise<string | null>;

/** Canonical system prompt — the SAME for every runtime, centralised here
 * like COGNITION_SYSTEM_PROMPT / TITLER_SYSTEM_PROMPT. */
export const LINK_EXTRACT_SYSTEM_PROMPT =
  "You extract code-collaboration references from one-sentence summaries of an " +
  "AI-coding session. Report ONLY references that explicitly appear in the text: " +
  "pull/merge request URLs, issue URLs, commit URLs, the `org/repo#123` shorthand, " +
  "and ticket keys like ENG-123 or PROJ-4567. Output one reference per line, " +
  "verbatim, nothing else — no prose, no numbering, no markdown. If the summaries " +
  "contain no such reference, reply with exactly `none`. Never invent a reference, " +
  "a repo, a number, or a URL that is not present in the text.";

export const LINK_EXTRACT_MAX_TOKENS = 120;
export const LINK_EXTRACT_TEMPERATURE = 0.1;

/** At most this many abstracts are sampled into one extraction call — the
 * same envelope the titler uses, keeping the prompt small + cheap. */
export const LINK_EXTRACT_MAX_ABSTRACTS = 12;

/** Build the user message for the link-extraction call. Pure / cheap so tests
 * can assert on the exact prompt without a runtime. */
export function buildLinkExtractUserPrompt(abstracts: string[]): string {
  const lines = abstracts
    .map((a, i) => `  [part ${i + 1}] ${a.replace(/\s+/g, " ").trim().slice(0, 240)}`)
    .join("\n");
  return `Summaries of the session's parts:\n${lines}\n\nList the references, one per line, or "none".`;
}

/** Options for {@link buildSessionMetadata}. Both are optional + best-effort:
 * absent or failing, the relevant channel simply contributes nothing. */
export interface SessionMetadataOptions {
  /** Resolve a cwd to its git context on disk (e.g. parsers' resolveGitContext).
   * Best-effort; cwd-cached by the implementation. */
  resolveGit?: (cwd: string | null) => Promise<GitContext | null>;
  /** On-device model that surfaces references from redacted abstracts. */
  extractLinks?: LinkExtractor;
  /** Local git verified-outcome check for a PR in `cwd` (parsers'
   * `checkPullRequestOutcome`) — fills a referenced PR's merged/merged_at/
   * reverted signals for the server's CPVO engine. Best-effort. */
  checkPrOutcome?: (
    cwd: string,
    prNumber: number,
  ) => Promise<{ merged: boolean; merged_at: string | null; reverted: boolean } | null>;
  /** Aggregate the files a repo changed in [`since`, `until`] (parsers'
   * `collectFilesChanged`) — the per-file token re-spend signal. Best-effort. */
  collectFilesChanged?: (
    cwd: string,
    since: string,
    until: string,
  ) => Promise<Array<{ path: string; lines_added: number; lines_deleted: number }> | null>;
}

function groupBy<T>(items: T[], keyOf: (item: T) => string): Map<string, T[]> {
  const out = new Map<string, T[]>();
  for (const item of items) {
    const key = keyOf(item);
    const arr = out.get(key) ?? [];
    arr.push(item);
    out.set(key, arr);
  }
  return out;
}

/** Commits usually land when a session wraps up — a little AFTER its active
 * window closes — so the per-file capture extends `until` by this grace period
 * (capped at the next session's start; see step 6). 4h covers "commit when
 * you're done" without claiming unrelated later work when there's no next
 * session. The active window itself (`sessionWindow`) stays the lower bound. */
const COMMIT_GRACE_MS = 4 * 60 * 60 * 1000;

/** The session's [since, until] as ISO-8601 instants — the span of its segments
 * and events. Null when nothing is timestamped. ISO-8601 sorts lexically =
 * chronologically, so a string min/max bounds the active window (step 6 extends
 * `until` past it by {@link COMMIT_GRACE_MS} to catch the commit-on-wrap). */
function sessionWindow(
  evs: RawEvent[],
  segs: Segment[],
): { since: string; until: string } | null {
  const starts: string[] = [];
  const ends: string[] = [];
  for (const s of segs) {
    if (s.started_at) starts.push(s.started_at);
    if (s.ended_at) ends.push(s.ended_at);
  }
  for (const e of evs) {
    if (e.ts) {
      starts.push(e.ts);
      ends.push(e.ts);
    }
  }
  starts.sort();
  ends.sort();
  const since = starts[0];
  const until = ends[ends.length - 1];
  return since && until ? { since, until } : null;
}

/**
 * Build one {@link SessionMetadata} per session from a batch's segments +
 * events. Sessions whose channels surface no reference are omitted from the
 * result — shipping an empty map would only overwrite better server state.
 *
 * Returns a map suitable for `IngestBatch.session_metadata`.
 */
export async function buildSessionMetadata(
  segments: Segment[],
  events: RawEvent[],
  opts: SessionMetadataOptions = {},
): Promise<Record<string, SessionMetadata>> {
  const eventsBySession = groupBy(events, (e) => e.session_id);
  const segsBySession = groupBy(segments, (s) => s.session_id);
  const sessionIds = new Set<string>([...eventsBySession.keys(), ...segsBySession.keys()]);

  const out: Record<string, SessionMetadata> = {};
  // All sessions' start instants (ms), sorted — used to cap each session's
  // commit-capture grace window (step 6) at the NEXT session's start, so a later
  // session never double-claims a commit made after this one wrapped up.
  const allStartsMs = [...sessionIds]
    .map((sid) => {
      const w = sessionWindow(eventsBySession.get(sid) ?? [], segsBySession.get(sid) ?? []);
      return w ? Date.parse(w.since) : Number.NaN;
    })
    .filter((m) => !Number.isNaN(m))
    .sort((a, b) => a - b);
  for (const sessionId of sessionIds) {
    try {
      const evs = eventsBySession.get(sessionId) ?? [];
      const segs = segsBySession.get(sessionId) ?? [];
      const parts: DetectedRefs[] = [];
      // repo slug → a cwd on disk for it (built in step 2), used to run the
      // verified-outcome git-check against the right local repo.
      const slugToCwd = new Map<string, string>();

      // 1. git context already on the events.
      const cwds = new Set<string>();
      for (const e of evs) {
        if (e.cwd) cwds.add(e.cwd);
        if (!e.git) continue;
        const refs = emptyDetectedRefs();
        if (e.git.remote_slug) {
          refs.repos.push({
            host: e.git.remote_host ?? null,
            slug: e.git.remote_slug,
            branches: e.git.branch ? [e.git.branch] : [],
            source: "git",
          });
        }
        if (e.git.branch) refs.issues.push(...detectBranchTickets(e.git.branch));
        parts.push(refs);
      }

      // 2. resolve git on disk for the session's cwds (best-effort, cwd-cached).
      if (opts.resolveGit) {
        for (const cwd of cwds) {
          let g: GitContext | null = null;
          try {
            g = await opts.resolveGit(cwd);
          } catch {
            g = null;
          }
          if (!g?.remote_slug) continue;
          slugToCwd.set(g.remote_slug.toLowerCase(), cwd);
          const refs = emptyDetectedRefs();
          refs.repos.push({
            host: g.remote_host ?? null,
            slug: g.remote_slug,
            branches: g.branch ? [g.branch] : [],
            source: "git",
          });
          if (g.branch) refs.issues.push(...detectBranchTickets(g.branch));
          parts.push(refs);
        }
      }

      // 3a. references the parser already pulled from each event's FULL text
      //     (high recall — whole turn, not just the ≤320-char excerpt).
      for (const e of evs) {
        if (e.references) parts.push(e.references);
      }
      // 3b. fallback scan of the redacted excerpts (covers older daemons /
      //     replayed events that predate the `references` field).
      for (const e of evs) {
        if (e.content_excerpt) parts.push(detectReferences(e.content_excerpt, "content"));
      }
      const abstracts = [...segs]
        .sort((a, b) => a.started_at.localeCompare(b.started_at))
        .map((s) => stripCognitionSuffix(s.abstract))
        .filter((a) => a.length > 0);
      for (const a of abstracts) parts.push(detectReferences(a, "content"));

      // 4. provider-agnostic model pass — one call per session over the sample.
      if (opts.extractLinks && abstracts.length > 0) {
        try {
          const reply = await opts.extractLinks({
            abstracts: sampleAbstracts(abstracts, LINK_EXTRACT_MAX_ABSTRACTS),
          });
          if (reply) parts.push(detectReferences(reply, "model"));
        } catch {
          // Best-effort — the deterministic channels stand on their own.
        }
      }

      const meta = dedupeSessionMetadata(parts);

      // 5. enrich PRs with on-device verified-outcome signals (CPVO), where the
      //    PR's repo is on disk. Best-effort + per-PR isolated.
      if (opts.checkPrOutcome && meta.pull_requests.length > 0) {
        for (const pr of meta.pull_requests) {
          const cwd = pr.slug ? slugToCwd.get(pr.slug.toLowerCase()) : undefined;
          if (!cwd) continue;
          try {
            const o = await opts.checkPrOutcome(cwd, pr.number);
            if (o) {
              pr.merged = o.merged;
              pr.merged_at = o.merged_at;
              pr.reverted = o.reverted;
            }
          } catch {
            // best-effort — a failed git-check just leaves the PR unenriched.
          }
        }
      }

      // 6. enrich with the files each resolved repo changed in the session — the
      //    per-file token re-spend signal. Commits usually land when wrapping up,
      //    just AFTER the active window, so extend `until` by a grace period
      //    (capped at the next session's start so a later session can't claim it).
      //    One git pass per repo, best-effort + isolated; slug-stamped for rollup.
      if (opts.collectFilesChanged && slugToCwd.size > 0) {
        const range = sessionWindow(evs, segs);
        if (range) {
          const endMs = Date.parse(range.until);
          const nextStartMs = allStartsMs.find((m) => m > endMs);
          const untilMs =
            nextStartMs !== undefined
              ? Math.min(endMs + COMMIT_GRACE_MS, nextStartMs)
              : endMs + COMMIT_GRACE_MS;
          const until = new Date(untilMs).toISOString();
          const fileRefs: FileRef[] = [];
          for (const [slug, cwd] of slugToCwd) {
            try {
              const changes = await opts.collectFilesChanged(cwd, range.since, until);
              if (!changes) continue;
              for (const c of changes) {
                fileRefs.push({
                  slug,
                  path: c.path,
                  lines_added: c.lines_added,
                  lines_deleted: c.lines_deleted,
                  source: "git",
                });
              }
            } catch {
              // best-effort — a failed git pass just leaves that repo's files out.
            }
          }
          if (fileRefs.length > 0) meta.files = dedupeFiles([...meta.files, ...fileRefs]);
        }
      }

      if (!isEmptySessionMetadata(meta)) out[sessionId] = meta;
    } catch {
      // Defence-in-depth: a single session's failure never drops metadata for
      // the rest of the batch. (dedupe is total, so this should not fire — it
      // guards against an unforeseen throw in the git/model channels.)
    }
  }
  return out;
}
