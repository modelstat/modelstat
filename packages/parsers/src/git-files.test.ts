/**
 * Pure per-file numstat aggregation. The `git log` invocation in
 * collectFilesChanged is a thin wrapper; the load-bearing parse + per-file
 * summation across commits is pure and pinned here.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { parseNumstat } from "./git-files.js";

test("parseNumstat reads added/deleted/path lines", () => {
  const out = parseNumstat("40\t12\tsrc/forward.ts\n3\t0\tsrc/util.ts\n");
  assert.deepEqual(out, [
    { path: "src/forward.ts", lines_added: 40, lines_deleted: 12 },
    { path: "src/util.ts", lines_added: 3, lines_deleted: 0 },
  ]);
});

test("parseNumstat sums the same path across commits", () => {
  // Two commits both touching forward.ts; the `--format=%H` SHA header lines
  // carry no tabs, so they're ignored and the two deltas sum.
  const out = parseNumstat("aaaa1111\n10\t2\tsrc/forward.ts\nbbbb2222\n5\t1\tsrc/forward.ts\n");
  assert.equal(out.length, 1);
  assert.deepEqual(out[0], { path: "src/forward.ts", lines_added: 15, lines_deleted: 3 });
});

test("parseNumstat counts binary files (-) as zero lines", () => {
  const out = parseNumstat("-\t-\tassets/logo.png\n");
  assert.deepEqual(out, [{ path: "assets/logo.png", lines_added: 0, lines_deleted: 0 }]);
});

test("parseNumstat ignores non-numstat lines", () => {
  const out = parseNumstat("commit abc123\nAuthor: Dev\n\n7\t1\tREADME.md\n");
  assert.deepEqual(out, [{ path: "README.md", lines_added: 7, lines_deleted: 1 }]);
});

test("parseNumstat returns empty for empty input", () => {
  assert.deepEqual(parseNumstat(""), []);
});
