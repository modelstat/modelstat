import assert from "node:assert/strict";
import { test } from "node:test";
import type { GitContext, RawEvent } from "@modelstat/core";
import { type GitResolver, type RootResolver, resolveAuthoritativeGit } from "./git-enrich.js";

function mkGit(over: Partial<GitContext> = {}): GitContext {
  return { remote_url: null, remote_host: null, remote_slug: null, branch: null, ...over };
}

function mkEvent(over: Partial<RawEvent> = {}): RawEvent {
  return {
    source_event_id: "e1",
    ts: "2026-01-01T00:00:00.000Z",
    kind: "assistant_message",
    agent: "claude_code",
    provider: "anthropic",
    model: "claude",
    session_id: "s1",
    turn_index: null,
    parent_event_id: null,
    cwd: null,
    git: null,
    tokens: null,
    duration_ms: null,
    tool_calls: {},
    files_touched: [],
    source_file: "f.jsonl",
    source_byte_offset: 0,
    pricing_mode: "subscription",
    ...over,
  };
}

/** A remote resolver backed by a fixed cwd→GitContext table. */
function resolverFor(table: Record<string, GitContext>): GitResolver {
  return async (cwd) => (cwd ? (table[cwd] ?? null) : null);
}

/** A repo-root resolver backed by a fixed cwd→root-dir table. Default: no root. */
function rootResolverFor(table: Record<string, string> = {}): RootResolver {
  return (cwd) => (cwd ? (table[cwd] ?? null) : null);
}

const NO_ROOT: RootResolver = () => null;

test("overwrites a path-guessed slug with the authoritative remote, keeping the historical branch", async () => {
  // The parser guessed a SUBDIRECTORY (`accounting/controllers`) off an in-repo
  // `…/src/…` cwd; disk says the repo is really `acme/backend`.
  const ev = mkEvent({
    cwd: "/Users/dev/work/backend/src/accounting/controllers",
    git: mkGit({ remote_slug: "accounting/controllers", branch: "feature/x" }),
  });
  const resolveGit = resolverFor({
    "/Users/dev/work/backend/src/accounting/controllers": mkGit({
      remote_url: "git@github.com:acme/backend.git",
      remote_host: "github.com",
      remote_slug: "acme/backend",
      branch: "main", // today's checkout — must NOT clobber the turn's branch
    }),
  });

  const [out] = await resolveAuthoritativeGit([ev], resolveGit, NO_ROOT);

  assert.equal(out?.git?.remote_slug, "acme/backend");
  assert.equal(out?.git?.remote_host, "github.com");
  assert.equal(out?.git?.remote_url, "git@github.com:acme/backend.git");
  assert.equal(out?.git?.branch, "feature/x", "keeps the branch the parser recorded for the turn");
});

test("no remote → uses the repo-ROOT dir name (bare, never a subpath)", async () => {
  // A local-only repo (no origin) inside its own `src/` tree: the parser guessed
  // the subdirectory, but the repo is the `.git` root's folder, `backend`.
  const ev = mkEvent({
    cwd: "/Users/dev/work/backend/src/app/case-studies",
    git: mkGit({ remote_slug: "app/case-studies", branch: "feature/x" }),
  });
  const resolveGit = resolverFor({
    "/Users/dev/work/backend/src/app/case-studies": mkGit({ branch: "main" }), // root found, no origin
  });
  const resolveRoot = rootResolverFor({
    "/Users/dev/work/backend/src/app/case-studies": "/Users/dev/work/backend",
  });

  const [out] = await resolveAuthoritativeGit([ev], resolveGit, resolveRoot);

  assert.equal(out?.git?.remote_slug, "backend", "bare repo-root name, not app/case-studies");
  assert.equal(out?.git?.remote_host, null);
  assert.equal(out?.git?.branch, "feature/x", "historical branch preserved");
});

test("repo-root fallback overrides a subpath guess even when the event had no branch", async () => {
  const ev = mkEvent({ cwd: "/home/x/src/erpc-site/src/public/images", git: null });
  const resolveGit = resolverFor({}); // nothing resolves a remote
  const resolveRoot = rootResolverFor({
    "/home/x/src/erpc-site/src/public/images": "/home/x/src/erpc-site",
  });

  const [out] = await resolveAuthoritativeGit([ev], resolveGit, resolveRoot);

  assert.equal(out?.git?.remote_slug, "erpc-site");
  assert.equal(out?.git?.branch, null);
});

test("no remote AND no reachable .git → keeps the parser's guess (last resort)", async () => {
  const ev = mkEvent({
    cwd: "/tmp/gone-repo/src/app/thing",
    git: mkGit({ remote_slug: "app/thing", branch: "main" }),
  });
  const resolveGit = resolverFor({}); // no remote

  const [out] = await resolveAuthoritativeGit([ev], resolveGit, NO_ROOT);

  assert.equal(out, ev, "unchanged event reference — nothing to override");
  assert.equal(out?.git?.remote_slug, "app/thing");
});

test("adds git identity when the event had none but disk resolves a remote", async () => {
  const ev = mkEvent({ cwd: "/Users/dev/work/api", git: null });
  const resolveGit = resolverFor({
    "/Users/dev/work/api": mkGit({
      remote_host: "github.com",
      remote_slug: "acme/api",
      branch: "main",
    }),
  });

  const [out] = await resolveAuthoritativeGit([ev], resolveGit, NO_ROOT);

  assert.equal(out?.git?.remote_slug, "acme/api");
  assert.equal(out?.git?.branch, "main", "no parsed branch → falls back to the on-disk branch");
});

test("a throwing git resolver with no reachable root leaves the event untouched", async () => {
  const ev = mkEvent({
    cwd: "/Users/dev/work/backend",
    git: mkGit({ remote_slug: "accounting/controllers" }),
  });
  const resolveGit: GitResolver = async () => {
    throw new Error("git exploded");
  };

  const [out] = await resolveAuthoritativeGit([ev], resolveGit, NO_ROOT);

  assert.equal(out?.git?.remote_slug, "accounting/controllers");
});

test("resolves each distinct cwd independently", async () => {
  const a = mkEvent({ source_event_id: "a", cwd: "/w/a", git: mkGit({ remote_slug: "x/a-sub" }) });
  const b = mkEvent({ source_event_id: "b", cwd: "/w/b", git: mkGit({ remote_slug: "x/b-sub" }) });
  const resolveGit = resolverFor({
    "/w/a": mkGit({ remote_slug: "acme/a", remote_host: "github.com" }),
    "/w/b": mkGit({ remote_slug: "acme/b", remote_host: "github.com" }),
  });

  const out = await resolveAuthoritativeGit([a, b], resolveGit, NO_ROOT);

  assert.equal(out[0]?.git?.remote_slug, "acme/a");
  assert.equal(out[1]?.git?.remote_slug, "acme/b");
});

test("short-circuits (same array, no resolver calls) when no event has a cwd", async () => {
  const evs = [mkEvent({ cwd: null }), mkEvent({ source_event_id: "e2", cwd: null })];
  let gitCalls = 0;
  let rootCalls = 0;
  const resolveGit: GitResolver = async () => {
    gitCalls += 1;
    return null;
  };
  const resolveRoot: RootResolver = () => {
    rootCalls += 1;
    return null;
  };

  const out = await resolveAuthoritativeGit(evs, resolveGit, resolveRoot);

  assert.equal(out, evs, "returns the input array unchanged");
  assert.equal(gitCalls, 0, "no cwds → git resolver never invoked");
  assert.equal(rootCalls, 0, "no cwds → root resolver never invoked");
});
