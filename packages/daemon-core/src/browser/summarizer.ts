/**
 * Tiered browser summariser: try Chrome's built-in Gemini Nano first
 * (zero-cost when available), fall back to the user's opted-in
 * WebLLM engine, finally to a no-op (empty abstract). The pipeline
 * tolerates an empty abstract — tags still attach, tokens still
 * count, only the natural-language summary is missing.
 *
 * Used by apps/extension's offscreen document to build the
 * `summarize` adapter passed into buildSegmentsForSession().
 */

import type { Summarizer } from "../pipeline/index.js";
import { promptApiReady, promptApiSummarize } from "./prompt-api.js";
import { webllmSummarize, type WebLLMChatLike } from "./webllm.js";

export interface TieredSummariserOptions {
  /** Resolved WebLLM engine the caller has already loaded, or a
   * factory that loads it lazily on first use. Undefined → skip the
   * WebLLM tier. */
  webllm?: WebLLMChatLike | (() => Promise<WebLLMChatLike>);
  /** If false, skip the Chrome Prompt API tier even when available —
   * mainly for testing. Defaults to true. */
  usePromptApi?: boolean;
}

/**
 * Returns a Summarizer that auto-selects the best available tier on
 * each call. Picks Prompt API when availability() === "readily",
 * otherwise WebLLM if configured, otherwise returns "" so the
 * pipeline falls through to its deterministic-facts abstract.
 */
export function tieredSummarize(opts: TieredSummariserOptions = {}): Summarizer {
  const usePromptApi = opts.usePromptApi !== false;
  let webllmEngine: WebLLMChatLike | null = null;
  let webllmLoad: Promise<WebLLMChatLike> | null = null;

  const getWebLLM = (): Promise<WebLLMChatLike | null> => {
    if (webllmEngine) return Promise.resolve(webllmEngine);
    if (!opts.webllm) return Promise.resolve(null);
    if (typeof opts.webllm !== "function") {
      webllmEngine = opts.webllm;
      return Promise.resolve(webllmEngine);
    }
    if (!webllmLoad) {
      webllmLoad = opts.webllm().then((e) => {
        webllmEngine = e;
        return e;
      });
    }
    return webllmLoad;
  };

  return async (input) => {
    if (usePromptApi && (await promptApiReady())) {
      try {
        return await promptApiSummarize()(input);
      } catch {
        /* fall through to WebLLM */
      }
    }
    const engine = await getWebLLM();
    if (engine) {
      try {
        return await webllmSummarize(engine)(input);
      } catch {
        /* fall through to no-op */
      }
    }
    return "";
  };
}
