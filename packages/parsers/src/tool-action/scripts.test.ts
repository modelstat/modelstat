import { strict as assert } from "node:assert";
import { test } from "node:test";
import { detectScriptRefs, resolveScriptPath, scriptCandidates } from "./scripts.js";

test("detects every script in a command, in order", () => {
  assert.deepEqual(
    detectScriptRefs("./build.sh && ./deploy.sh && bash scripts/test.sh"),
    ["./build.sh", "./deploy.sh", "scripts/test.sh"],
  );
});

test("detects scripts by extension, relative prefix, and leading path", () => {
  assert.deepEqual(detectScriptRefs("python tools/migrate.py --apply"), ["tools/migrate.py"]);
  assert.deepEqual(detectScriptRefs("../bin/run.rb"), ["../bin/run.rb"]);
  assert.deepEqual(detectScriptRefs("/opt/ci/release.sh prod"), ["/opt/ci/release.sh"]);
});

test("ignores flags, URLs, and plain programs", () => {
  assert.deepEqual(detectScriptRefs("git status && curl https://x.example.com/a.sh"), []);
  assert.deepEqual(detectScriptRefs("kubectl rollout restart deploy/api -n prod"), []);
});

test("candidates are ordered most-absolute / longest first", () => {
  const cands = scriptCandidates("deploy.sh", ["/repo/project", "/repo", ""]);
  // Absolute candidates first; among them the longest (most specific) root wins.
  assert.equal(cands[0], "/repo/project/deploy.sh");
  assert.equal(cands[1], "/repo/deploy.sh");
  assert.equal(cands[cands.length - 1], "deploy.sh"); // bare, last
});

test("resolveScriptPath picks the first existing candidate", () => {
  const present = new Set(["/repo/deploy.sh"]);
  const got = resolveScriptPath("deploy.sh", ["/repo/project", "/repo"], (p) => present.has(p));
  assert.equal(got, "/repo/deploy.sh");
  // None exist → null (caller may fall back to the LLM).
  assert.equal(resolveScriptPath("missing.sh", ["/repo"], () => false), null);
});

test("an already-absolute ref is tried verbatim first", () => {
  const got = resolveScriptPath("/srv/x/go.sh", ["/repo"], (p) => p === "/srv/x/go.sh");
  assert.equal(got, "/srv/x/go.sh");
});
