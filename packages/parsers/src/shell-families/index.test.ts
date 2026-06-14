import { strict as assert } from "node:assert";
import { test } from "node:test";
import { extractCommandFamilies, MAX_COMMAND_FAMILIES, SHELL_FAMILY_ALLOWLIST } from "./index.js";

test("simple command yields its verb", () => {
  assert.deepEqual(extractCommandFamilies("git status"), ["git"]);
});

test("absolute path verb is basenamed", () => {
  assert.deepEqual(extractCommandFamilies("/usr/local/bin/git pull --rebase"), ["git"]);
});

test("&& chain dedupes repeated verbs", () => {
  assert.deepEqual(
    extractCommandFamilies('git add -A && git commit -m "wip" && git push'),
    ["git"],
    "three git invocations collapse to one family",
  );
});

test("; and | separators each contribute a part", () => {
  assert.deepEqual(extractCommandFamilies("pnpm install; pnpm test"), ["pnpm"]);
  assert.deepEqual(extractCommandFamilies("cat package.json | jq .name | grep modelstat"), [
    "cat",
    "jq",
    "grep",
  ]);
});

test("|| splits like | (empty middle part is harmless)", () => {
  assert.deepEqual(extractCommandFamilies("pytest || python -m pytest"), ["pytest", "python"]);
});

test("newlines act as separators", () => {
  assert.deepEqual(extractCommandFamilies("git fetch\nnpm test"), ["git", "npm"]);
});

test("separators inside double quotes do not split", () => {
  assert.deepEqual(
    extractCommandFamilies('echo "a && b; c | d" && git push'),
    ["git"],
    "quoted separators stay inside the echo part",
  );
  assert.deepEqual(extractCommandFamilies('grep "foo|bar" src/index.ts'), ["grep"]);
});

test("separators inside single quotes do not split", () => {
  assert.deepEqual(extractCommandFamilies("node -e 'a; b && c | d'"), ["node"]);
});

test("backslash-escaped separator does not split", () => {
  assert.deepEqual(extractCommandFamilies("find . -name foo \\; -print && git diff"), [
    "find",
    "git",
  ]);
});

test("quoted verb is unwrapped", () => {
  assert.deepEqual(extractCommandFamilies('"git" status'), ["git"]);
});

test("sudo wrapper is stripped", () => {
  assert.deepEqual(extractCommandFamilies("sudo npm install -g something"), ["npm"]);
});

test("env wrapper and VAR= assignments are stripped", () => {
  assert.deepEqual(extractCommandFamilies("env FOO=bar npm run build"), ["npm"]);
  assert.deepEqual(extractCommandFamilies("RUST_LOG=debug cargo test"), ["cargo"]);
});

test("time/nice wrappers are stripped", () => {
  assert.deepEqual(extractCommandFamilies("time make -j8"), ["make"]);
});

test("stacked wrappers are stripped in sequence", () => {
  assert.deepEqual(extractCommandFamilies("sudo env FOO=1 time git push"), ["git"]);
});

test("wrapper option flags are skipped", () => {
  assert.deepEqual(extractCommandFamilies("env -i git status"), ["git"]);
});

test("cd-chains: the cd part is dropped, the payload verb survives", () => {
  assert.deepEqual(extractCommandFamilies("cd packages/core && pnpm test"), ["pnpm"]);
  assert.deepEqual(extractCommandFamilies("cd /tmp/x; git init"), ["git"]);
});

test("non-allowlist verbs are dropped, never echoed", () => {
  assert.deepEqual(
    extractCommandFamilies("echo hello && rm -rf ./dist && ./scripts/deploy.sh"),
    [],
  );
  assert.deepEqual(extractCommandFamilies("my-secret-binary --token abc"), []);
});

test("cap at 3 families, first-seen order", () => {
  assert.deepEqual(
    extractCommandFamilies(
      "git status && npm test && cargo build && docker ps && kubectl get pods",
    ),
    ["git", "npm", "cargo"],
    "stops at MAX_COMMAND_FAMILIES",
  );
});

test("dedupe across mixed separators", () => {
  assert.deepEqual(extractCommandFamilies("git status | grep foo; git diff"), ["git", "grep"]);
});

test("docker-compose and redis-cli (hyphenated verbs) match exactly", () => {
  assert.deepEqual(extractCommandFamilies("docker-compose up -d"), ["docker-compose"]);
  assert.deepEqual(extractCommandFamilies("redis-cli ping"), ["redis-cli"]);
});

test("empty and whitespace-only commands yield no families", () => {
  assert.deepEqual(extractCommandFamilies(""), []);
  assert.deepEqual(extractCommandFamilies("   \n  "), []);
  assert.deepEqual(extractCommandFamilies(" && ; | "), []);
});

test("unterminated quote never throws and never leaks", () => {
  assert.deepEqual(extractCommandFamilies('git commit -m "unterminated && npm test'), ["git"]);
});

test("output invariants: only allowlist members, ≤3, unique", () => {
  const gnarly =
    "sudo env A=1 B=2 git pull && cat x | jq . | grep y; npm run z\nmake all && weird-tool --flag | sed s/a/b/";
  const out = extractCommandFamilies(gnarly);
  assert.ok(out.length <= MAX_COMMAND_FAMILIES, "cap respected");
  assert.equal(new Set(out).size, out.length, "no duplicates");
  for (const fam of out) {
    assert.ok(
      (SHELL_FAMILY_ALLOWLIST as readonly string[]).includes(fam),
      `family '${fam}' must be on the allowlist`,
    );
  }
});
