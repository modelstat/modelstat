/**
 * Per-session metadata pass — assembles the repos, pull requests, commits,
 * and issues a session touched, then ships one {@link SessionMetadata} per
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
 *   3. redacted content — PR/issue/commit URLs surviving in the segment
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
  dedupeSessionMetadata,
  detectBranchTickets,
  detectReferences,
  emptyDetectedRefs,
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
  for (const sessionId of sessionIds) {
    try {
      const evs = eventsBySession.get(sessionId) ?? [];
      const segs = segsBySession.get(sessionId) ?? [];
      const parts: DetectedRefs[] = [];

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
        if (e.git.commit_sha) {
          refs.commits.push({
            sha: e.git.commit_sha,
            slug: e.git.remote_slug ?? null,
            url: null,
            source: "git",
            confidence: 0.6,
          });
        }
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
      // 3b. fallback scan of the redacted excerpts (covers older companions /
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
      if (!isEmptySessionMetadata(meta)) out[sessionId] = meta;
    } catch {
      // Defence-in-depth: a single session's failure never drops metadata for
      // the rest of the batch. (dedupe is total, so this should not fire — it
      // guards against an unforeseen throw in the git/model channels.)
    }
  }
  return out;
}
