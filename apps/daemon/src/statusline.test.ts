/**
 * Statusline rendering tests — the pure `renderStatusline` contract: tokens
 * come from Claude Code's context window (instant), the modelstat layer ($ +
 * taxonomy) comes from the cached insights, and `analyzing` / missing data
 * degrade gracefully. ANSI codes are stripped before asserting so the tests
 * read against the visible text.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { formatCost, formatTokens, renderStatusline, type StatuslineInput } from "./statusline.js";

// Build the SGR-strip regex from the escape char's code point so the source
// carries no literal control character (biome noControlCharactersInRegex).
const ESC = String.fromCharCode(27);
const SGR = new RegExp(`${ESC}\\[[0-9;]*m`, "g");
const strip = (s: string): string => s.replace(SGR, "");

test("formatTokens compacts to k / M", () => {
  assert.equal(formatTokens(0), "0");
  assert.equal(formatTokens(950), "950");
  assert.equal(formatTokens(1234), "1.2k");
  assert.equal(formatTokens(15500), "16k");
  assert.equal(formatTokens(2_300_000), "2.3M");
});

test("formatCost: sub-cent gets 4dp, else 2dp, junk → null", () => {
  assert.equal(formatCost("0.42"), "$0.42");
  assert.equal(formatCost(0.0031), "$0.0031");
  assert.equal(formatCost("0"), null);
  assert.equal(formatCost(null), null);
  assert.equal(formatCost(undefined), null);
  assert.equal(formatCost("not-a-number"), null);
});

const cwInput: StatuslineInput = {
  session_id: "s1",
  context_window: { total_input_tokens: 12000, total_output_tokens: 300 },
};

test("tokens render from the context window even with no insights", () => {
  const line = strip(renderStatusline(cwInput, null));
  assert.match(line, /modelstat/);
  assert.match(line, /12k tok/);
});

test("ready insights add $ and taxonomy chips", () => {
  const line = strip(
    renderStatusline(cwInput, {
      status: "ready",
      cost_usd: "0.42",
      taxonomy_nodes: [
        { id: "n1", name: "debugging", emoji: "🐛" },
        { id: "n2", name: "auth" },
      ],
    }),
  );
  assert.match(line, /12k tok/);
  assert.match(line, /\$0\.42/);
  assert.match(line, /🐛 debugging/);
  assert.match(line, /auth/);
});

test("more than 3 taxonomy nodes collapse to a +N suffix", () => {
  const line = strip(
    renderStatusline(cwInput, {
      status: "ready",
      taxonomy_nodes: [
        { id: "1", name: "a" },
        { id: "2", name: "b" },
        { id: "3", name: "c" },
        { id: "4", name: "d" },
        { id: "5", name: "e" },
      ],
    }),
  );
  assert.match(line, /a, b, c \+2/);
});

test("analyzing status shows a quiet placeholder, no $ yet", () => {
  const line = strip(renderStatusline(cwInput, { status: "analyzing" }));
  assert.match(line, /12k tok/);
  assert.match(line, /analyzing…/);
  assert.doesNotMatch(line, /\$/);
});

test("not_ingested insights contribute nothing beyond tokens", () => {
  const line = strip(
    renderStatusline(cwInput, { status: "not_ingested", cost_usd: "0", taxonomy_nodes: [] }),
  );
  assert.match(line, /12k tok/);
  assert.doesNotMatch(line, /analyzing/);
});

test("empty context window + no insights still renders the marker", () => {
  const line = strip(renderStatusline({ session_id: "s" }, null));
  assert.equal(line.trim(), "modelstat");
});

test("falls back to summing current_usage when totals are absent", () => {
  const line = strip(
    renderStatusline(
      {
        session_id: "s",
        context_window: {
          current_usage: {
            input_tokens: 1000,
            output_tokens: 200,
            cache_read_input_tokens: 300,
            cache_creation_input_tokens: 0,
          },
        },
      },
      null,
    ),
  );
  assert.match(line, /1\.5k tok/);
});
