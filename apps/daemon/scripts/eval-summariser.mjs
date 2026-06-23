#!/usr/bin/env node
/**
 * Eval harness for the bundled summariser. Sweeps a configurable set
 * of {systemPrompt, userTemplate, temperature, maxTokens, excerptCount,
 * excerptMaxChars} variants over a folder of fixtures and prints a
 * comparison table.
 *
 *   node scripts/eval-summariser.mjs [--fixtures DIR] [--variants FILE]
 *
 * Default fixtures dir: ~/.modelstat/evals
 * Default variants:     a hardcoded baseline + 3 candidate tweaks
 *
 * Each fixture is a JSON file with this shape (see synthetic
 * fixtures alongside this script for examples):
 *   {
 *     "name": "...",
 *     "tool": "claude_code",
 *     "git_remote_slug": "owner/repo",
 *     "git_branch": "main",
 *     "excerpts": ["already-redacted excerpt 1", ...],
 *     "reference_summary": "the perfect summary I want the model to produce",
 *     "expected_keywords": ["keyword1", "keyword2", ...]
 *   }
 *
 * The script:
 *   1. Loads the bundled Qwen model once (no reload between variants).
 *   2. For every (variant × fixture) builds the prompt, runs the model,
 *      strips <think>, slices to ABSTRACT_OUTPUT_MAX_CHARS, scores it.
 *   3. Prints generated text + scores + the reference + a final
 *      per-variant aggregate.
 *
 * Scoring is intentionally simple — there's no reliable autograder for
 * "is this a good content summary?" so the harness just measures:
 *   - keyword recall   = % of expected_keywords present (case-insensitive)
 *   - length           = generated chars
 *   - latency          = wall time per call
 *   - filler hits      = count of low-info phrases ("worked on",
 *     "developer", "session", etc) — high is bad
 * The HUMAN reads the table and picks the winner.
 *
 * Real fixtures (containing the user's own session content) are kept
 * in `~/.modelstat/evals/`. Synthetic fixtures alongside this script
 * (`./eval-fixtures/*.json`) are inspired by generic backend/RPC service
 * patterns but use fully-randomised names/numbers/labels so they're publishable.
 */

import {
  createWriteStream,
  existsSync,
  readFileSync,
  readdirSync,
  statSync,
} from "node:fs";
import { mkdir, rename } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";

// Inlined model config — kept in sync with
// packages/daemon-core/src/node/llama.ts. Duplicating these few
// lines keeps the eval harness runnable as a plain Node script
// (daemon-core is .ts source with no built .js, so importing it
// directly from Node fails). When the bundled summariser model
// changes there, change it here too.
const DEFAULT_MODEL_URL =
  "https://huggingface.co/lmstudio-community/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf";

function defaultLlamaConfig() {
  const env = process.env;
  const modelsDir =
    env.MODELSTAT_MODELS_DIR ?? join(homedir(), ".modelstat", "models");
  const modelUrl = env.MODELSTAT_LLAMA_MODEL_URL ?? DEFAULT_MODEL_URL;
  const modelPath =
    env.MODELSTAT_LLAMA_MODEL_PATH ??
    join(modelsDir, modelUrl.split("/").pop() ?? "model.gguf");
  return {
    modelPath,
    modelUrl,
    modelsDir,
    contextSize: Number(env.MODELSTAT_LLAMA_CONTEXT ?? 4096),
  };
}

async function ensureLlamaModel(cfg) {
  if (existsSync(cfg.modelPath)) return cfg.modelPath;
  await mkdir(dirname(cfg.modelPath), { recursive: true });
  const res = await fetch(cfg.modelUrl);
  if (!res.ok || !res.body) {
    throw new Error(
      `model download failed: ${res.status} ${res.statusText} (${cfg.modelUrl})`,
    );
  }
  const total = Number(res.headers.get("content-length") ?? 0);
  const totalMb = total > 0 ? (total / 1048576).toFixed(0) : "?";
  console.log(
    `[eval] downloading summariser model (~${totalMb} MB) → ${cfg.modelPath}`,
  );
  const tmp = `${cfg.modelPath}.partial`;
  const out = createWriteStream(tmp);
  let received = 0;
  let lastLog = 0;
  const nodeStream = Readable.fromWeb(res.body);
  nodeStream.on("data", (chunk) => {
    received += chunk.length;
    const now = Date.now();
    if (now - lastLog > 2000 && total > 0) {
      const pct = ((received / total) * 100).toFixed(1);
      const mb = (received / 1048576).toFixed(1);
      console.log(`[eval]   ${mb} / ${totalMb} MB (${pct}%)`);
      lastLog = now;
    }
  });
  await new Promise((resolve, reject) => {
    nodeStream.pipe(out);
    out.on("finish", () => resolve());
    out.on("error", reject);
    nodeStream.on("error", reject);
  });
  await rename(tmp, cfg.modelPath);
  console.log(`[eval] model ready (${cfg.modelPath})`);
  return cfg.modelPath;
}

// ── CLI args ───────────────────────────────────────────────────────
const argv = process.argv.slice(2);
const argFor = (flag) => {
  const i = argv.indexOf(flag);
  return i >= 0 ? argv[i + 1] : undefined;
};
const fixturesDir =
  argFor("--fixtures") ||
  process.env.MODELSTAT_EVAL_FIXTURES ||
  join(homedir(), ".modelstat", "evals");
const variantsFile = argFor("--variants");
const onlyFixture = argFor("--only");

if (!existsSync(fixturesDir)) {
  console.error(`fixtures dir not found: ${fixturesDir}`);
  console.error(`  pass --fixtures <dir> to point at synthetic ones in this repo`);
  process.exit(1);
}

// ── Variants — edit / extend / pass via --variants <file> ──────────
// Each entry produces ONE column in the comparison table.
const DEFAULT_VARIANTS = [
  {
    name: "current-prompt",
    systemPrompt:
      "Write the SHORTEST paragraph (1-3 sentences) that captures EXACTLY " +
      "what was ACHIEVED in this coding segment, packed with the concrete " +
      "domain keywords the developer used. Lead with an outcome verb — " +
      "shipped, fixed, migrated, ramped, wired, diagnosed, refactored, " +
      "designed, deployed, reverted, instrumented. Name the specific " +
      "things touched: feature names, frameworks, components, bug classes, " +
      "decisions. Density beats length: a 50-char sentence that names the " +
      "actual feature beats 200 chars of filler. Skip narration ('the " +
      "developer'), skip vague verbs ('worked on', 'explored', 'looked " +
      "into'), skip preamble. If only metadata is given, name the project " +
      "+ tool + visible action concisely. Never quote excerpts verbatim. " +
      "No PII, no API keys, no file paths, no code literals. Reply with " +
      "ONLY the paragraph.",
    temperature: 0.2,
    maxTokens: 1024, // thinking budget — actual answer ~80 tokens
    sliceChars: 400,
  },
  {
    name: "tighter-density",
    systemPrompt:
      "Output ONE compressed paragraph (≤350 chars) describing exactly " +
      "what was achieved in this coding segment. Maximise concrete-noun " +
      "density: name every feature, framework, component, bug class, " +
      "metric, and decision the developer touched. Start with a strong " +
      "outcome verb (fixed/diagnosed/refactored/shipped/migrated/wired/" +
      "deployed/replaced/designed). Forbidden words: 'the developer', " +
      "'worked on', 'session', 'explored', 'investigated' (use 'diagnosed' " +
      "or 'traced'), 'looked into', 'something', 'various'. No " +
      "introductory clauses. Reply with ONLY the paragraph, no preamble.",
    temperature: 0.2,
    maxTokens: 1024,
    sliceChars: 350,
  },
  {
    name: "outcome-first-strict",
    systemPrompt:
      "Summarise what was DONE in this coding segment in 1-3 short " +
      "sentences (≤300 chars total). Rules: " +
      "(1) First word is a past-tense outcome verb. " +
      "(2) Mention the SPECIFIC component/file-area/bug/feature by name. " +
      "(3) State the result if visible (commit, PR, deploy, metric, fix). " +
      "(4) Pack technical keywords; remove every filler word. " +
      "(5) No 'the developer / user / human'. No 'a coding session'. " +
      "(6) Never wrap in quotes. " +
      "Reply with ONLY the paragraph.",
    temperature: 0.15,
    maxTokens: 1024,
    sliceChars: 300,
  },
];

const variants = variantsFile
  ? JSON.parse(readFileSync(variantsFile, "utf8"))
  : DEFAULT_VARIANTS;

// ── Fixture loader ────────────────────────────────────────────────
function loadFixtures(dir) {
  const out = [];
  for (const f of readdirSync(dir)) {
    if (!f.endsWith(".json")) continue;
    const path = join(dir, f);
    if (!statSync(path).isFile()) continue;
    const data = JSON.parse(readFileSync(path, "utf8"));
    if (onlyFixture && !data.name?.includes(onlyFixture)) continue;
    out.push(data);
  }
  return out;
}

// ── Prompt construction (mirrors daemon-core/pipeline) ─────────
function buildPrompt(fixture, systemPromptUnused, excerptCountFromVariant) {
  // The prompt template lives here so each variant can also
  // override excerpt count / size if we extend the variant shape.
  // For now we keep the user-message template fixed and only vary
  // the system prompt + decoding params.
  const facts = [
    fixture.git_remote_slug ? `repo ${fixture.git_remote_slug}` : null,
    fixture.git_branch ? `branch ${fixture.git_branch}` : null,
    `${fixture.excerpts.length} sampled turns on ${fixture.tool ?? "unknown"}`,
  ]
    .filter(Boolean)
    .join("; ");
  const pickCount = Math.min(
    fixture.excerpts.length,
    excerptCountFromVariant ?? 7,
  );
  const picks = pickEvenly(fixture.excerpts, pickCount);
  const block = picks
    .map((e, i) => `  [turn ${i + 1}] "${e.replace(/\s+/g, " ").trim()}"`)
    .join("\n");
  return `Session context: ${facts}.

Sampled excerpts from the conversation (already redacted of PII and secrets):
${block}

Write the SHORTEST keyword-dense paragraph naming exactly what was achieved. Lead with an outcome verb. Pack with concrete domain keywords (frameworks, features, components, decisions). Skip narration and filler.`;
}

function pickEvenly(arr, n) {
  if (arr.length <= n) return arr.slice();
  const out = [arr[0]];
  for (let i = 1; i < n - 1; i++) {
    const idx = Math.round((i * (arr.length - 1)) / (n - 1));
    out.push(arr[idx]);
  }
  out.push(arr[arr.length - 1]);
  return out;
}

// ── Scoring ───────────────────────────────────────────────────────
const FILLER_PATTERNS = [
  /\bthe (developer|user|human|engineer|team)\b/gi,
  /\bworked on\b/gi,
  /\b(a |the )?coding session\b/gi,
  /\bexplored\b/gi,
  /\blooked into\b/gi,
  /\bvarious\b/gi,
  /\bsomething\b/gi,
  /\bin order to\b/gi,
];

function scoreOutput(text, fixture) {
  const lower = text.toLowerCase();
  const expected = fixture.expected_keywords ?? [];
  const hit = expected.filter((k) => lower.includes(k.toLowerCase()));
  const recall = expected.length > 0 ? hit.length / expected.length : 1;
  let fillerHits = 0;
  for (const re of FILLER_PATTERNS) {
    const m = text.match(re);
    if (m) fillerHits += m.length;
  }
  return {
    chars: text.length,
    recallPct: Math.round(recall * 100),
    keywordHits: hit,
    keywordMisses: expected.filter((k) => !lower.includes(k.toLowerCase())),
    fillerHits,
  };
}

function stripThinking(text) {
  return text
    .replace(/<think>[\s\S]*?<\/think>/gi, "")
    .replace(/<think>[\s\S]*$/i, "")
    .trim();
}

// ── Model bootstrap (one model load, many sessions) ───────────────
const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));

async function loadModel() {
  // Resolve through daemon-dev's node_modules where node-llama-cpp
  // lives. Node walks up looking for the package, so this works from
  // any working directory under the workspace.
  const llamaMod = await import("node-llama-cpp");
  const cfg = defaultLlamaConfig();
  const modelPath = await ensureLlamaModel(cfg);
  const llama = await llamaMod.getLlama();
  const model = await llama.loadModel({ modelPath });
  return { llamaMod, model, ctxSize: cfg.contextSize };
}

async function runVariant({ llamaMod, model }, variant, fixture, ctxSize) {
  // One fresh context per call so each variant gets its own
  // sequence and we don't run into "No sequences left" errors when
  // sweeping many variants. Cheap (no model reload) — only the KV
  // cache reallocates.
  const context = await model.createContext({ contextSize: ctxSize });
  const session = new llamaMod.LlamaChatSession({
    contextSequence: context.getSequence(),
    systemPrompt: variant.systemPrompt,
  });
  const prompt = buildPrompt(fixture, variant.systemPrompt);
  const t0 = Date.now();
  let raw;
  let error;
  try {
    raw = await session.prompt(prompt, {
      temperature: variant.temperature,
      maxTokens: variant.maxTokens,
    });
  } catch (err) {
    error = err?.message ?? String(err);
  }
  await context.dispose().catch(() => undefined);
  const latencyMs = Date.now() - t0;
  if (error) return { error, latencyMs };
  const stripped = stripThinking(raw ?? "");
  const text = stripped.slice(0, variant.sliceChars ?? 400);
  return { text, raw, latencyMs };
}

// ── Main ──────────────────────────────────────────────────────────
async function main() {
  const fixtures = loadFixtures(fixturesDir);
  if (fixtures.length === 0) {
    console.error(`no fixtures found in ${fixturesDir}`);
    process.exit(1);
  }
  console.log(`fixtures dir: ${fixturesDir}`);
  console.log(`variants:     ${variants.map((v) => v.name).join(", ")}`);
  console.log(`fixtures:     ${fixtures.map((f) => f.name).join(", ")}`);
  console.log("");
  console.log("loading model… (first run downloads ~2.7 GB)");
  const ctx = await loadModel();
  console.log("model loaded; running sweep");

  const results = []; // [{fixture, variant, scoring, text, latency}]
  for (const fixture of fixtures) {
    console.log(`\n${"━".repeat(72)}`);
    console.log(`FIXTURE: ${fixture.name}`);
    console.log(`REFERENCE: ${fixture.reference_summary}`);
    console.log(`KEYWORDS:  ${(fixture.expected_keywords ?? []).join(", ")}`);
    console.log("─".repeat(72));
    for (const variant of variants) {
      const r = await runVariant(ctx, variant, fixture, ctx.ctxSize);
      if (r.error) {
        console.log(
          `\n  [${variant.name}]  ERROR: ${r.error} (${r.latencyMs}ms)`,
        );
        continue;
      }
      const score = scoreOutput(r.text, fixture);
      results.push({ fixture: fixture.name, variant: variant.name, ...score, latencyMs: r.latencyMs });
      console.log(
        `\n  [${variant.name}]  chars=${score.chars}  recall=${score.recallPct}%  filler=${score.fillerHits}  ${r.latencyMs}ms`,
      );
      console.log(`  → ${r.text}`);
      if (score.keywordMisses.length) {
        console.log(`  missing: ${score.keywordMisses.join(", ")}`);
      }
    }
  }

  // Aggregate per variant
  console.log(`\n${"━".repeat(72)}\nAGGREGATE\n${"━".repeat(72)}`);
  for (const variant of variants) {
    const rows = results.filter((r) => r.variant === variant.name);
    if (!rows.length) continue;
    const avgRecall = avg(rows.map((r) => r.recallPct));
    const avgChars = avg(rows.map((r) => r.chars));
    const avgFiller = avg(rows.map((r) => r.fillerHits));
    const avgLatency = avg(rows.map((r) => r.latencyMs));
    console.log(
      `  ${variant.name.padEnd(24)}  recall=${avgRecall.toFixed(0)}%  chars=${avgChars.toFixed(0)}  filler=${avgFiller.toFixed(1)}  latency=${avgLatency.toFixed(0)}ms`,
    );
  }
}

function avg(xs) {
  return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : 0;
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
