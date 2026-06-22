/**
 * REAL-LLM eval for the cognition prompt — proves the prompt design surfaces
 * sensible mood + posture from real-looking session abstracts. Gated on
 * `LLM_BASE_URL` + `LLM_API_KEY` (an OpenAI-compatible endpoint, e.g. DeepSeek),
 * so without creds the suite SKIPS; run locally/manually to validate quality:
 *
 *   LLM_BASE_URL=https://api.deepseek.com/v1 LLM_API_KEY=… \
 *     node --import tsx --test src/pipeline/cognition.eval.test.ts
 *
 * The prod daemon runs a smaller on-device model; this validates the PROMPT, not
 * that model. Assertions are soft (LLM variance) + every result is logged so the
 * tag quality is eyeballable.
 */
import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  buildCognitionUserPrompt,
  COGNITION_SYSTEM_PROMPT,
  parseCognitionReply,
} from "./cognition.js";

const BASE = process.env.LLM_BASE_URL;
const KEY = process.env.LLM_API_KEY;
const MODEL = process.env.COGNITION_EVAL_MODEL ?? "deepseek-chat";

const FIXTURES: Array<{ abstract: string; expect: "mood" | "posture" }> = [
  { abstract: "Spent two hours fighting a flaky CI test, gave up and skipped it for now.", expect: "mood" },
  { abstract: "Shipped a hotfix straight to prod on a Friday without waiting for review.", expect: "posture" },
  { abstract: "Carefully reviewed every diff and added tests before merging the auth change.", expect: "posture" },
  { abstract: "Pushed back on the agent's plan twice and made it redo the refactor properly.", expect: "posture" },
  { abstract: "Excitedly built a new dashboard end to end and everything just worked first try.", expect: "mood" },
  { abstract: "Felt overwhelmed by the migration — too many moving parts to keep straight.", expect: "mood" },
];

async function cognize(abstract: string) {
  const res = await fetch(`${BASE}/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${KEY}` },
    body: JSON.stringify({
      model: MODEL,
      messages: [
        { role: "system", content: COGNITION_SYSTEM_PROMPT },
        { role: "user", content: buildCognitionUserPrompt(abstract) },
      ],
      temperature: 0.2,
      max_tokens: 120,
    }),
  });
  if (!res.ok) throw new Error(`LLM ${res.status}: ${await res.text()}`);
  const json = (await res.json()) as { choices: Array<{ message: { content: string } }> };
  return parseCognitionReply(json.choices[0]?.message?.content ?? "");
}

describe("cognition prompt eval (real LLM — gated on LLM_BASE_URL+LLM_API_KEY)", { skip: !(BASE && KEY) }, () => {
  it("derives plausible mood + posture from real-looking abstracts", { timeout: 90_000 }, async () => {
    let hits = 0;
    for (const fx of FIXTURES) {
      const tags = await cognize(fx.abstract);
      // biome-ignore lint/suspicious/noConsole: eval visibility
      console.log(
        `[cognition-eval] "${fx.abstract.slice(0, 48)}…" → mood=${JSON.stringify(tags?.emotions)} stance=${JSON.stringify(tags?.posture)} mind=${JSON.stringify(tags?.meta)}`,
      );
      assert.ok(tags !== null, "reply parsed");
      const field = fx.expect === "mood" ? tags?.emotions : tags?.posture;
      if (field && field.length > 0) hits += 1;
    }
    // The clear-signal field should surface for the large majority; allow one
    // miss to model variance.
    assert.ok(hits >= FIXTURES.length - 1, `only ${hits}/${FIXTURES.length} surfaced the expected field`);
  });
});
