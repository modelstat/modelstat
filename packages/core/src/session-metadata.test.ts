import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  type DetectedRefs,
  dedupeFiles,
  dedupeSessionMetadata,
  detectBranchTickets,
  detectEventReferences,
  detectReferences,
  type FileRef,
  isEmptySessionMetadata,
  PullRequestRef,
  repoRefFromGit,
  SessionMetadata,
} from "./session-metadata.js";

test("detects a GitHub PR URL and its repo", () => {
  const r = detectReferences("opened https://github.com/acme/web/pull/42 for review");
  assert.equal(r.pull_requests.length, 1);
  assert.equal(r.pull_requests[0]?.number, 42);
  assert.equal(r.pull_requests[0]?.slug, "acme/web");
  assert.equal(r.pull_requests[0]?.host, "github.com");
  assert.equal(r.pull_requests[0]?.source, "content");
  // The PR link also yields its repo.
  assert.equal(r.repos.length, 1);
  assert.equal(r.repos[0]?.slug, "acme/web");
  assert.equal(r.repos[0]?.host, "github.com");
});

test("detects a GitLab merge request with a nested group path", () => {
  const r = detectReferences("see https://gitlab.com/grp/sub/api/-/merge_requests/7");
  assert.equal(r.pull_requests.length, 1);
  assert.equal(r.pull_requests[0]?.number, 7);
  assert.equal(r.pull_requests[0]?.slug, "grp/sub/api");
  assert.equal(r.repos[0]?.slug, "grp/sub/api");
});

test("detects a Bitbucket pull request", () => {
  const r = detectReferences("https://bitbucket.org/team/svc/pull-requests/9 merged");
  assert.equal(r.pull_requests[0]?.number, 9);
  assert.equal(r.pull_requests[0]?.host, "bitbucket.org");
});

test("detects GitHub + GitLab issue URLs", () => {
  const gh = detectReferences("fixes https://github.com/acme/web/issues/13");
  assert.equal(gh.issues[0]?.provider, "github");
  assert.equal(gh.issues[0]?.key, "13");
  assert.equal(gh.issues[0]?.slug, "acme/web");

  const gl = detectReferences("closes https://gitlab.com/acme/web/-/issues/8");
  assert.equal(gl.issues[0]?.provider, "gitlab");
  assert.equal(gl.issues[0]?.key, "8");
});

test("detects Linear and Jira issue URLs", () => {
  const linear = detectReferences("ticket https://linear.app/acme/issue/ENG-742/retry-logic");
  assert.equal(linear.issues[0]?.provider, "linear");
  assert.equal(linear.issues[0]?.key, "ENG-742");

  const jira = detectReferences("https://acme.atlassian.net/browse/PROJ-1234");
  assert.equal(jira.issues[0]?.provider, "jira");
  assert.equal(jira.issues[0]?.key, "PROJ-1234");
});

test("detects org/repo#123 shorthand as a low-confidence issue", () => {
  const r = detectReferences("addresses acme/web#321 nicely");
  assert.equal(r.issues.length, 1);
  assert.equal(r.issues[0]?.provider, "github");
  assert.equal(r.issues[0]?.key, "321");
  assert.equal(r.issues[0]?.slug, "acme/web");
  assert.ok((r.issues[0]?.confidence ?? 1) < 0.7);
});

test("a PR cue disambiguates org/repo#N shorthand toward the PR entity", () => {
  // An adjacent "PR"/"merged" cue → a PR (so a shorthand-only PR is captured),
  for (const text of ["opened PR acme/web#7", "merged acme/web#7", "pull request acme/web#7"]) {
    const r = detectReferences(text);
    assert.equal(r.pull_requests.length, 1, `PR for: ${text}`);
    assert.equal(r.pull_requests[0]?.slug, "acme/web");
    assert.equal(r.pull_requests[0]?.number, 7);
    assert.equal(r.issues.length, 0, `no stray issue for: ${text}`);
    assert.ok(r.repos.some((x) => x.slug === "acme/web"), "the repo is anchored too");
  }
  // …but an issue cue or no cue stays an issue (no false PRs).
  for (const text of ["fixes acme/web#7", "closes acme/web#7", "see acme/web#7"]) {
    const r = detectReferences(text);
    assert.equal(r.pull_requests.length, 0, `no PR for: ${text}`);
    assert.equal(r.issues.length, 1, `issue for: ${text}`);
  }
});

test("bare TEAM-123 tickets are mined ONLY from the model channel", () => {
  const fromContent = detectReferences("works on ENG-501 today", "content");
  assert.equal(fromContent.issues.length, 0, "content must not mine bare tickets (UTF-8 noise)");

  const fromModel = detectReferences("ENG-501", "model");
  assert.equal(fromModel.issues.length, 1);
  assert.equal(fromModel.issues[0]?.key, "ENG-501");
  assert.equal(fromModel.issues[0]?.source, "model");
});

test("detectBranchTickets pulls a key out of a branch name", () => {
  const refs = detectBranchTickets("feature/ENG-742-retry-logic");
  assert.equal(refs.length, 1);
  assert.equal(refs[0]?.key, "ENG-742");
  assert.equal(refs[0]?.source, "git");
  assert.equal(detectBranchTickets("main").length, 0);
  assert.equal(detectBranchTickets(null).length, 0);
});

test("repoRefFromGit builds a repo with its branch, or null without a slug", () => {
  const ok = repoRefFromGit({ remote_host: "github.com", remote_slug: "acme/web", branch: "main" });
  assert.equal(ok?.slug, "acme/web");
  assert.deepEqual(ok?.branches, ["main"]);
  assert.equal(ok?.source, "git");
  assert.equal(repoRefFromGit({ remote_slug: null, branch: "x" }), null);
});

test("dedupe merges repos by slug, unioning branches and keeping strongest source", () => {
  const parts: DetectedRefs[] = [
    {
      repos: [{ host: null, slug: "acme/web", branches: ["feature/x"], source: "content" }],
      pull_requests: [],
      issues: [],
    },
    {
      repos: [{ host: "github.com", slug: "Acme/Web", branches: ["main"], source: "git" }],
      pull_requests: [],
      issues: [],
    },
  ];
  const m = dedupeSessionMetadata(parts);
  assert.equal(m.repos.length, 1, "case-insensitive slug merge");
  assert.equal(m.repos[0]?.host, "github.com");
  assert.equal(m.repos[0]?.source, "git", "git beats content");
  assert.deepEqual([...m.repos[0]!.branches].sort(), ["feature/x", "main"]);
});

test("dedupe keeps the highest-confidence/strongest copy of a PR", () => {
  const parts: DetectedRefs[] = [
    {
      repos: [],
      pull_requests: [
        { host: null, slug: "acme/web", number: 5, url: null, source: "model", confidence: 0.4 },
      ],
      issues: [],
    },
    {
      repos: [],
      pull_requests: [
        {
          host: "github.com",
          slug: "acme/web",
          number: 5,
          url: "https://github.com/acme/web/pull/5",
          source: "content",
          confidence: 0.95,
        },
      ],
      issues: [],
    },
  ];
  const m = dedupeSessionMetadata(parts);
  assert.equal(m.pull_requests.length, 1);
  assert.equal(m.pull_requests[0]?.url, "https://github.com/acme/web/pull/5");
  assert.equal(m.pull_requests[0]?.confidence, 0.95);
});

test("dedupe carries a URL over from a weaker copy when the winner lacks one", () => {
  // Winner is the git issue (no URL); loser is a content issue WITH a url.
  const parts: DetectedRefs[] = [
    {
      repos: [],
      pull_requests: [],
      issues: [
        {
          provider: "github",
          key: "9",
          slug: "acme/web",
          url: null,
          source: "content",
          confidence: 0.9,
        },
      ],
    },
    {
      repos: [],
      pull_requests: [],
      issues: [
        {
          provider: "github",
          key: "9",
          slug: "acme/web",
          url: "https://github.com/acme/web/issues/9",
          source: "content",
          confidence: 0.55,
        },
      ],
    },
  ];
  const m = dedupeSessionMetadata(parts);
  assert.equal(m.issues.length, 1);
  assert.equal(m.issues[0]?.url, "https://github.com/acme/web/issues/9");
});

test("a full session blob yields plural, deduped metadata", () => {
  const text = [
    "Worked across https://github.com/acme/web/pull/10 and",
    "https://github.com/acme/web/pull/11, fixed acme/web#12.",
  ].join(" ");
  const m = dedupeSessionMetadata([detectReferences(text)]);
  assert.equal(m.pull_requests.length, 2);
  assert.equal(m.issues.length, 1);
  assert.deepEqual(
    m.repos.map((r) => r.slug),
    ["acme/web"],
  );
});

test("empty input → empty, shippable-guard agrees", () => {
  const m = dedupeSessionMetadata([detectReferences("just some prose, no links")]);
  assert.ok(isEmptySessionMetadata(m));
  assert.ok(SessionMetadata.safeParse(m).success);
});

test("an oversized capture is dropped, never thrown (one bad ref can't poison the rest)", () => {
  const longSlug = "a".repeat(260);
  const text = `bad https://github.com/${longSlug}/x/pull/1 good https://github.com/acme/web/pull/2`;
  const m = dedupeSessionMetadata([detectReferences(text)]);
  assert.ok(
    m.pull_requests.some((p) => p.number === 2),
    "the valid PR survives alongside the dropped oversized one",
  );
  assert.ok(
    !m.pull_requests.some((p) => (p.slug ?? "").length > 200),
    "the over-cap slug ref is dropped, not shipped",
  );
  assert.ok(SessionMetadata.safeParse(m).success, "result is always wire-valid");
});

test("a phantom org/repo#N issue is reconciled away when the real PR is present", () => {
  const m = dedupeSessionMetadata([
    detectReferences("shipped https://github.com/acme/web/pull/5 (was acme/web#5)"),
  ]);
  assert.equal(m.pull_requests.length, 1);
  assert.equal(m.pull_requests[0]?.number, 5);
  assert.equal(m.issues.length, 0, "the #5 shorthand is the PR, not a separate issue");
});

test("detectEventReferences pulls public refs from full text, null when none", () => {
  const r = detectEventReferences(
    "opened https://github.com/acme/web/pull/42 and fixed acme/web#7",
  );
  assert.equal(r?.pull_requests[0]?.number, 42);
  assert.ok(r?.issues.some((i) => i.key === "7"));
  assert.equal(detectEventReferences("just prose, nothing linked"), null);
  assert.equal(detectEventReferences(""), null);
});

test("detectEventReferences does NOT mine bare ticket keys (UTF-8 / SHA-256 safety)", () => {
  // Prose with ticket-shaped noise but no URL → nothing detected.
  assert.equal(detectEventReferences("we use UTF-8 and SHA-256 and touched ENG-9"), null);
  // …but a real Linear URL in the same turn is caught.
  assert.equal(
    detectEventReferences("ticket https://linear.app/acme/issue/ENG-9")?.issues[0]?.key,
    "ENG-9",
  );
});

test("dedupeFiles sums line counts per (slug, path) and keeps the same path in different repos separate", () => {
  const files: FileRef[] = [
    { slug: "acme/api", path: "src/forward.ts", lines_added: 10, lines_deleted: 2, source: "git" },
    { slug: "acme/api", path: "src/forward.ts", lines_added: 5, lines_deleted: 1, source: "git" },
    { slug: "acme/api", path: "src/util.ts", lines_added: 3, lines_deleted: 0, source: "git" },
    { slug: "acme/web", path: "src/forward.ts", lines_added: 1, lines_deleted: 0, source: "git" },
  ];
  const out = dedupeFiles(files);
  assert.equal(out.length, 3);
  const fwd = out.find((f) => f.slug === "acme/api" && f.path === "src/forward.ts");
  assert.equal(fwd?.lines_added, 15);
  assert.equal(fwd?.lines_deleted, 3);
});

test("dedupeFiles drops malformed refs and caps at 500", () => {
  const tooMany: FileRef[] = Array.from({ length: 600 }, (_, i) => ({
    slug: "acme/api",
    path: `src/f${i}.ts`,
    lines_added: 1,
    lines_deleted: 0,
    source: "git",
  }));
  assert.equal(dedupeFiles(tooMany).length, 500);
  // An over-long path violates the wire cap → that one ref is dropped, not thrown.
  const bad: FileRef[] = [
    { slug: "acme/api", path: "x".repeat(401), lines_added: 1, lines_deleted: 0, source: "git" },
    { slug: "acme/api", path: "ok.ts", lines_added: 1, lines_deleted: 0, source: "git" },
  ];
  const out = dedupeFiles(bad);
  assert.equal(out.length, 1);
  assert.equal(out[0]?.path, "ok.ts");
});

test("isEmptySessionMetadata accounts for files", () => {
  const withFiles = SessionMetadata.parse({
    files: [{ slug: "acme/api", path: "src/a.ts", lines_added: 1, lines_deleted: 0, source: "git" }],
  });
  assert.equal(isEmptySessionMetadata(withFiles), false);
  assert.equal(isEmptySessionMetadata(SessionMetadata.parse({})), true);
});

/**
 * The daemon measures a PR's change primitives from local git and ships them
 * on `PullRequestRef`. This is the exact JSON the Rust struct serializes —
 * pinned byte-for-byte by
 * `references::tests::change_primitives_are_absent_until_measured_and_never_null_or_zero`
 * in daemon/crates/modelstat-parsers/src/references.rs. Change one, change both.
 */
const RUST_EMITTED_PR_REF =
  '{"host":"github.com","slug":"acme/api","number":42,"url":null,"source":"git","confidence":0.9,"merged":true,"merged_at":"2026-07-16T11:00:00Z","reverted":false,"merge_sha":"c0ffee1","merge_subject":"feat: retries (#42)","merge_method":"subject_ref_convention","files_changed":3,"lines_added":120,"lines_deleted":0,"commits_count":2}';

test("TS accepts the Rust-emitted PR ref, change primitives included", () => {
  const pr = PullRequestRef.parse(JSON.parse(RUST_EMITTED_PR_REF));
  assert.equal(pr.files_changed, 3);
  assert.equal(pr.lines_added, 120);
  // A measured zero survives as zero — it is a measurement, not a gap.
  assert.equal(pr.lines_deleted, 0);
  assert.equal(pr.commits_count, 2);
});

test("unmeasured change primitives stay absent — not null, not zero", () => {
  // A PR whose repo was not on disk: the daemon omits all four keys.
  const pr = PullRequestRef.parse({
    host: "github.com",
    slug: "acme/api",
    number: 7,
    url: null,
    source: "content",
    confidence: 0.9,
  });
  for (const key of ["files_changed", "lines_added", "lines_deleted", "commits_count"] as const) {
    assert.equal(pr[key], undefined, `${key} must be absent`);
    assert.ok(!(key in pr), `${key} must not be materialised as a null`);
  }
  // A squash merge: the diff is known, the flattened branch is not.
  const squashed = PullRequestRef.parse({
    number: 8,
    files_changed: 2,
    lines_added: 30,
    lines_deleted: 1,
  });
  assert.equal(squashed.files_changed, 2);
  assert.equal(squashed.commits_count, undefined);
  // `null` is not a stand-in for "unknown" here: absence is.
  assert.throws(() => PullRequestRef.parse({ number: 9, files_changed: null }));
  assert.throws(() => PullRequestRef.parse({ number: 9, lines_added: -1 }));
});
