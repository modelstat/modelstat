import { describe, expect, it } from "vitest";
import {
  countAnthropicApprox,
  countGrokHeuristic,
  countSentencePieceGemma,
} from "../src/offscreen/tokenizers/approximations.js";

describe("approximate tokenizers", () => {
  it("counts plain ASCII within reasonable bounds", () => {
    const text = "The quick brown fox jumps over the lazy dog.";
    const an = countAnthropicApprox(text);
    const sp = countSentencePieceGemma(text);
    const gk = countGrokHeuristic(text);
    // Rough OpenAI-token reference: ~10 tokens.
    expect(an).toBeGreaterThan(5);
    expect(an).toBeLessThan(20);
    expect(sp).toBeGreaterThan(5);
    expect(sp).toBeLessThan(20);
    expect(gk).toBeGreaterThan(5);
    expect(gk).toBeLessThan(20);
  });

  it("counts CJK densely (≈1 token per char)", () => {
    const text = "这是一段中文文本用于测试分词器";
    const n = countAnthropicApprox(text);
    expect(n).toBeGreaterThanOrEqual(text.length);
    expect(n).toBeLessThan(text.length * 3);
  });

  it("scales roughly linearly with length", () => {
    const a = "hello world ".repeat(10);
    const b = "hello world ".repeat(100);
    const na = countAnthropicApprox(a);
    const nb = countAnthropicApprox(b);
    expect(nb / na).toBeGreaterThan(8);
    expect(nb / na).toBeLessThan(12);
  });
});
