/**
 * The wire-boundary byte-clamp. Real transcripts carry multibyte text (CJK,
 * emoji, accents), so a string that passes the client's Zod `.max(N)` (code
 * units) can exceed the server's N-BYTE cap and draw a permanent 400 that the
 * never-drop loop then wedges on. These tests pin that every bounded string
 * leaves ≤ its cap in BYTES, cut only on code-point boundaries, driven off the
 * real IngestBatch schema so new fields are covered automatically.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { IngestBatch } from "@modelstat/core/schemas";
import { clampToSchemaBytes, clampUtf8Bytes } from "./clamp.js";

const bytes = (s: string): number => Buffer.byteLength(s, "utf8");

test("clampUtf8Bytes: ASCII under budget is returned verbatim", () => {
  const s = "a".repeat(400);
  assert.equal(clampUtf8Bytes(s, 512), s);
});

test("clampUtf8Bytes: a string exactly at the byte budget is untouched", () => {
  const s = "中".repeat(4); // 12 bytes
  assert.equal(bytes(s), 12);
  assert.equal(clampUtf8Bytes(s, 12), s);
});

test("clampUtf8Bytes: multibyte over budget is clamped to ≤ maxBytes", () => {
  const s = "中".repeat(400); // 1200 bytes, 400 code units
  const out = clampUtf8Bytes(s, 512);
  assert.ok(bytes(out) <= 512, `clamped to ${bytes(out)} bytes`);
  assert.equal(out, "中".repeat(170)); // 170 * 3 = 510 ≤ 512, 171 would be 513
});

test("clampUtf8Bytes: never splits a code point (surrogate pair stays whole)", () => {
  const s = "🎉".repeat(10); // each emoji = 4 bytes
  const out = clampUtf8Bytes(s, 10); // fits 2 whole emoji (8 bytes); a third would be 12
  assert.equal(out, "🎉🎉");
  assert.ok(bytes(out) <= 10);
  // A clean cut leaves no lone surrogate: a well-formed string is its own toWellFormed().
  assert.equal(out, out.toWellFormed());
});

test("clampToSchemaBytes: every bounded string in a batch fits its BYTE cap", () => {
  const batch = {
    device_id: "dev1",
    segments: [
      {
        // abstract .max(512), user_intent .max(512) — both multibyte-overflowing.
        abstract: "中".repeat(400), // 1200 bytes
        user_intent: "🎉".repeat(300), // 400 code units after the client slice → 1200 bytes
      },
    ],
    // .max(262144) — SPEC 0005's extreme guard; 131_073 two-byte chars overflow it.
    events: [{ content_excerpt: "é".repeat(131_073) }],
    session_titles: { s1: "本".repeat(100) }, // record value .max(120), 300 bytes
  } as never;

  const out = clampToSchemaBytes(IngestBatch, batch) as {
    segments: { abstract: string; user_intent: string }[];
    events: { content_excerpt: string }[];
    session_titles: Record<string, string>;
  };

  assert.ok(bytes(out.segments[0]!.abstract) <= 512, "abstract ≤ 512 bytes");
  assert.ok(bytes(out.segments[0]!.user_intent) <= 512, "user_intent ≤ 512 bytes");
  assert.ok(
    bytes(out.events[0]!.content_excerpt) <= 262144,
    "content_excerpt ≤ 262144 bytes (the SPEC 0005 extreme guard — 320 was the pre-verbatim cap)",
  );
  assert.ok(bytes(out.session_titles.s1!) <= 120, "session title ≤ 120 bytes");
});

test("clampToSchemaBytes: an all-ASCII batch is returned by reference (no needless clone)", () => {
  const batch = {
    device_id: "dev1",
    segments: [{ abstract: "a".repeat(400), user_intent: "plain ascii intent" }],
    events: [{ content_excerpt: "hello world" }],
  } as never;
  assert.equal(clampToSchemaBytes(IngestBatch, batch), batch);
});
