/**
 * Tokenizer dispatch — adapter configs reference tokenizers by name
 * (e.g. "tiktoken/o200k_base"). This module resolves that name to a
 * concrete count function and runs it. Unknown names → estimate via
 * default heuristic and log a warning.
 */

import { createLogger } from "@/common/logger.js";
import { countTiktoken } from "./tiktoken.js";
import {
  countAnthropicApprox,
  countGrokHeuristic,
  countSentencePieceGemma,
} from "./approximations.js";

const log = createLogger("tokenizers");

export type TokenizerAccuracy = "exact" | "estimated";

export type TokenizerResult = {
  tokens: number;
  accuracy: TokenizerAccuracy;
  name: string;
};

export async function tokenize(name: string, text: string): Promise<TokenizerResult> {
  if (name.startsWith("tiktoken/")) {
    const vocab = name.slice("tiktoken/".length);
    try {
      const tokens = await countTiktoken(vocab, text);
      return { tokens, accuracy: "exact", name };
    } catch (e) {
      log.warn(`tiktoken ${vocab} failed — falling back to estimate`, e);
      return { tokens: countAnthropicApprox(text), accuracy: "estimated", name };
    }
  }
  switch (name) {
    case "anthropic/approx":
      return { tokens: countAnthropicApprox(text), accuracy: "estimated", name };
    case "sentencepiece/gemma":
      return { tokens: countSentencePieceGemma(text), accuracy: "estimated", name };
    case "grok/heuristic":
      return { tokens: countGrokHeuristic(text), accuracy: "estimated", name };
    default:
      log.warn(`unknown tokenizer ${name} — estimating`);
      return { tokens: countAnthropicApprox(text), accuracy: "estimated", name };
  }
}

export function resolveTokenizerName(
  binding: { default: string; byModel?: Record<string, string> },
  model: string | null,
): string {
  if (!model || !binding.byModel) return binding.default;
  // Glob-ish matcher: only `*` supported, matched greedily against
  // the model name. Declared order = precedence.
  for (const [pattern, name] of Object.entries(binding.byModel)) {
    if (matchGlob(pattern, model)) return name;
  }
  return binding.default;
}

function matchGlob(pattern: string, input: string): boolean {
  if (!pattern.includes("*")) return pattern === input;
  const re = new RegExp(
    `^${pattern.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`,
  );
  return re.test(input);
}
