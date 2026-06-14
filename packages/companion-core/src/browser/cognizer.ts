/**
 * Tiered browser cognizer — mirrors `tieredSummarize` for the
 * cognition pass. Picks Chrome's built-in Prompt API first (zero
 * download), then the user's WebLLM engine if loaded, otherwise
 * returns null.
 *
 * Cognition is best-effort: a "no available runtime" path is the
 * normal degraded behaviour, NOT an error. Older browsers, users who
 * haven't opted into WebLLM, even users on a fresh Chrome that
 * temporarily lost Prompt API readiness — all sail through with
 * `cognition` simply absent from the abstract suffix.
 */

import type { Cognizer } from "../pipeline/cognition.js";
import { promptApiCognize, promptApiReady } from "./prompt-api.js";
import { webllmCognize, type WebLLMChatLike } from "./webllm.js";

export interface TieredCognizerOptions {
  /** Resolved WebLLM engine the caller has already loaded, or a
   * factory that loads it lazily on first use. Undefined → skip the
   * WebLLM tier. The summariser typically owns engine loading; pass
   * the same engine here to avoid double-loading. */
  webllm?: WebLLMChatLike | (() => Promise<WebLLMChatLike>);
  /** If false, skip the Chrome Prompt API tier even when available —
   * mainly for testing. Defaults to true. */
  usePromptApi?: boolean;
}

export function tieredCognize(opts: TieredCognizerOptions = {}): Cognizer {
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
        const out = await promptApiCognize()(input);
        if (out) return out;
      } catch {
        /* fall through to WebLLM */
      }
    }
    const engine = await getWebLLM();
    if (engine) {
      try {
        const out = await webllmCognize(engine)(input);
        if (out) return out;
      } catch {
        /* fall through to null */
      }
    }
    return null;
  };
}
