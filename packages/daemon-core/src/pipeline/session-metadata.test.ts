import { strict as assert } from "node:assert";
import { test } from "node:test";
import type { GitContext } from "@modelstat/core/schemas";
import { RawEvent, Segment } from "@modelstat/core/schemas";
import {
  buildLinkExtractUserPrompt,
  buildSessionMetadata,
  type LinkExtractor,
} from "./session-metadata.js";

let n = 0;

function mkGit(over: Partial<GitContext>): GitContext {
  return {
    remote_url: null,
    remote_host: null,
    remote_slug: null,
    branch: null,
    ...over,
  };
}

function mkEvent(over: {
  session_id?: string;
  cwd?: string | null;
  git?: GitContext | null;
  content_excerpt?: string;
}): RawEvent {
  n += 1;
  return RawEvent.parse({
    source_event_id: `e${n}`,
    ts: "2026-06-15T10:00:00.000Z",
    kind: "assistant_message",
    agent: "claude_code",
    provider: "anthropic",
    model: "claude-opus-4-8",
    session_id: over.session_id ?? "s1",
    turn_index: null,
    parent_event_id: null,
    cwd: over.cwd ?? null,
    git: over.git ?? null,
    tokens: { input: 1, output: 1, cache_creation: 0, cache_read: 0, reasoning: 0 },
    duration_ms: null,
    source_file: "f.jsonl",
    source_byte_offset: 0,
    ...(over.content_excerpt ? { content_excerpt: over.content_excerpt } : {}),
  });
}

function mkSegment(session_id: string, abstract: string): Segment {
  n += 1;
  return Segment.parse({
    segment_id: `seg${n}`,
    session_id,
    agent: "claude_code",
    started_at: "2026-06-15T10:00:00.000Z",
    ended_at: "2026-06-15T10:01:00.000Z",
    abstract,
    tokens: { input: 1, output: 1, cache_creation: 0, cache_read: 0, reasoning: 0 },
    redaction: {},
    source_event_ids: [`e${n}`],
  });
}

test("repos come from event git context, with the branch", async () => {
  const ev = mkEvent({
    git: mkGit({ remote_host: "github.com", remote_slug: "acme/web", branch: "main" }),
  });
  const seg = mkSegment("s1", "Refactored the auth module.");
  const out = await buildSessionMetadata([seg], [ev]);
  assert.equal(out.s1?.repos.length, 1);
  assert.equal(out.s1?.repos[0]?.slug, "acme/web");
  assert.deepEqual(out.s1?.repos[0]?.branches, ["main"]);
  assert.equal(out.s1?.repos[0]?.source, "git");
});

test("PRs + issues are mined from redacted abstracts", async () => {
  const ev = mkEvent({});
  const seg = mkSegment("s1", "Reviewed https://github.com/acme/web/pull/42 and fixed acme/web#7.");
  const out = await buildSessionMetadata([seg], [ev]);
  assert.equal(out.s1?.pull_requests[0]?.number, 42);
  assert.equal(out.s1?.issues.find((i) => i.key === "7")?.slug, "acme/web");
});

test("resolveGit enriches repos + branch tickets from disk", async () => {
  const ev = mkEvent({ cwd: "/Users/dev/Documents/api" });
  const seg = mkSegment("s1", "Implemented retry logic.");
  const resolveGit = async (cwd: string | null): Promise<GitContext | null> =>
    cwd === "/Users/dev/Documents/api"
      ? mkGit({ remote_host: "github.com", remote_slug: "acme/api", branch: "feature/ENG-9-retry" })
      : null;
  const out = await buildSessionMetadata([seg], [ev], { resolveGit });
  assert.equal(out.s1?.repos[0]?.slug, "acme/api");
  // The ticket key in the branch becomes an issue ref.
  assert.ok(out.s1?.issues.some((i) => i.key === "ENG-9"));
});

test("collectFilesChanged stamps per-repo files with their line counts", async () => {
  const ev = mkEvent({ cwd: "/Users/dev/Documents/api" });
  const seg = mkSegment("s1", "Refactored Forward().");
  const resolveGit = async (cwd: string | null): Promise<GitContext | null> =>
    cwd === "/Users/dev/Documents/api"
      ? mkGit({ remote_host: "github.com", remote_slug: "acme/api", branch: "main" })
      : null;
  const collectFilesChanged = async (cwd: string) =>
    cwd === "/Users/dev/Documents/api"
      ? [
          { path: "src/forward.ts", lines_added: 40, lines_deleted: 12 },
          { path: "src/util.ts", lines_added: 3, lines_deleted: 0 },
        ]
      : null;
  const out = await buildSessionMetadata([seg], [ev], { resolveGit, collectFilesChanged });
  const files = out.s1?.files ?? [];
  assert.equal(files.length, 2);
  const fwd = files.find((f) => f.path === "src/forward.ts");
  assert.equal(fwd?.slug, "acme/api");
  assert.equal(fwd?.lines_added, 40);
  assert.equal(fwd?.source, "git");
});

test("a failing collectFilesChanged never blocks the rest of the metadata", async () => {
  const ev = mkEvent({ cwd: "/Users/dev/Documents/api" });
  const seg = mkSegment("s1", "Landed the fix.");
  const resolveGit = async (): Promise<GitContext | null> =>
    mkGit({ remote_host: "github.com", remote_slug: "acme/api", branch: "main" });
  const collectFilesChanged = async () => {
    throw new Error("git exploded");
  };
  const out = await buildSessionMetadata([seg], [ev], { resolveGit, collectFilesChanged });
  assert.equal(out.s1?.repos[0]?.slug, "acme/api", "repos survive a files-capture failure");
  assert.deepEqual(out.s1?.files, []);
});

test("the per-file capture window extends past the session (grace) and caps at the next session", async () => {
  // Commits usually land when wrapping up, AFTER the active window — so `until`
  // is extended by the grace period, but capped at the next session's start so a
  // later session never double-claims a commit.
  const captured = new Map<string, { since: string; until: string }>();
  const collectFilesChanged = async (cwd: string, since: string, until: string) => {
    captured.set(cwd, { since, until });
    return [];
  };
  const resolveGit = async (): Promise<GitContext | null> =>
    mkGit({ remote_host: "github.com", remote_slug: "acme/api" });
  const evAt = (sid: string, cwd: string, ts: string): RawEvent =>
    RawEvent.parse({
      source_event_id: `e_${sid}`,
      ts,
      kind: "assistant_message",
      agent: "claude_code",
      provider: "anthropic",
      model: "claude-opus-4-8",
      session_id: sid,
      turn_index: null,
      parent_event_id: null,
      cwd,
      git: null,
      tokens: { input: 1, output: 1, cache_creation: 0, cache_read: 0, reasoning: 0 },
      duration_ms: null,
      source_file: "f.jsonl",
      source_byte_offset: 0,
    });
  const segAt = (sid: string, startIso: string, endIso: string): Segment =>
    Segment.parse({
      segment_id: `seg_${sid}`,
      session_id: sid,
      agent: "claude_code",
      started_at: startIso,
      ended_at: endIso,
      abstract: "work",
      tokens: { input: 1, output: 1, cache_creation: 0, cache_read: 0, reasoning: 0 },
      redaction: {},
      source_event_ids: [`e_${sid}`],
    });
  // s1 active 10:00–10:30 (cwd /r1); s2 active 11:00–11:10 (cwd /r2).
  const segs = [
    segAt("s1", "2026-06-15T10:00:00.000Z", "2026-06-15T10:30:00.000Z"),
    segAt("s2", "2026-06-15T11:00:00.000Z", "2026-06-15T11:10:00.000Z"),
  ];
  const evs = [
    evAt("s1", "/r1", "2026-06-15T10:15:00.000Z"),
    evAt("s2", "/r2", "2026-06-15T11:05:00.000Z"),
  ];
  await buildSessionMetadata(segs, evs, { resolveGit, collectFilesChanged });
  // s1 ends 10:30; +4h grace would be 14:30, but capped at s2's start 11:00.
  assert.equal(captured.get("/r1")?.since, "2026-06-15T10:00:00.000Z");
  assert.equal(captured.get("/r1")?.until, "2026-06-15T11:00:00.000Z");
  // s2 ends 11:10; no next session → +4h grace = 15:10.
  assert.equal(captured.get("/r2")?.until, "2026-06-15T15:10:00.000Z");
});

test("the model channel surfaces refs for content with no URL (any provider)", async () => {
  const ev = mkEvent({});
  const seg = mkSegment("s1", "Opened a pull request for the retry-logic fix.");
  const extractLinks: LinkExtractor = async () => "https://github.com/acme/web/pull/99";
  const out = await buildSessionMetadata([seg], [ev], { extractLinks });
  const pr = out.s1?.pull_requests[0];
  assert.equal(pr?.number, 99);
  assert.equal(pr?.source, "model");
});

test("sessions with no detectable references are omitted", async () => {
  const ev = mkEvent({});
  const seg = mkSegment("s1", "Discussed the weekly plan, nothing concrete.");
  const out = await buildSessionMetadata([seg], [ev]);
  assert.equal(out.s1, undefined);
  assert.deepEqual(Object.keys(out), []);
});

test("a failing git/model channel never blocks the deterministic one", async () => {
  const ev = mkEvent({ content_excerpt: "see https://github.com/acme/web/pull/5" });
  const seg = mkSegment("s1", "Landed the fix.");
  const resolveGit = async (): Promise<GitContext | null> => {
    throw new Error("git exploded");
  };
  const extractLinks: LinkExtractor = async () => {
    throw new Error("model exploded");
  };
  const out = await buildSessionMetadata([seg], [ev], { resolveGit, extractLinks });
  assert.equal(out.s1?.pull_requests[0]?.number, 5, "content channel survives sibling failures");
});

test("metadata is split per session", async () => {
  const a = mkEvent({ session_id: "sa", git: mkGit({ remote_slug: "acme/a" }) });
  const b = mkEvent({ session_id: "sb", git: mkGit({ remote_slug: "acme/b" }) });
  const out = await buildSessionMetadata([mkSegment("sa", "x"), mkSegment("sb", "y")], [a, b]);
  assert.equal(out.sa?.repos[0]?.slug, "acme/a");
  assert.equal(out.sb?.repos[0]?.slug, "acme/b");
});

test("event.references (the parser's full-text scan) feeds session metadata", async () => {
  const ev = RawEvent.parse({
    source_event_id: "r1",
    ts: "2026-06-15T10:00:00.000Z",
    kind: "assistant_message",
    agent: "claude_code",
    provider: "anthropic",
    model: "claude-opus-4-8",
    session_id: "s1",
    turn_index: null,
    parent_event_id: null,
    cwd: null,
    git: null,
    tokens: { input: 1, output: 1, cache_creation: 0, cache_read: 0, reasoning: 0 },
    duration_ms: null,
    source_file: "f.jsonl",
    source_byte_offset: 0,
    references: {
      repos: [],
      pull_requests: [
        {
          host: "github.com",
          slug: "acme/web",
          number: 12,
          url: "https://github.com/acme/web/pull/12",
          source: "content",
          confidence: 0.95,
        },
      ],
      issues: [],
    },
  });
  const out = await buildSessionMetadata([mkSegment("s1", "did stuff")], [ev]);
  assert.equal(out.s1?.pull_requests[0]?.number, 12);
});

test("buildLinkExtractUserPrompt lays out the abstracts and asks for one-per-line", () => {
  const p = buildLinkExtractUserPrompt(["did A", "did B"]);
  assert.match(p, /\[part 1\] did A/);
  assert.match(p, /\[part 2\] did B/);
  assert.match(p, /one per line/i);
});
