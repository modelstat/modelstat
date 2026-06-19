/**
 * Extension-side daemon pipeline binding.
 *
 * The offscreen document hosts the ML runtimes (transformers.js for
 * 384-dim MiniLM embeddings, Chrome Prompt API / WebLLM for
 * summarisation); this module plugs them into the shared pipeline so
 * the service worker ships Segment[] that look identical in shape to
 * what the CLI emits via Ollama.
 */
import type { RawEvent, Segment } from "@modelstat/core";
import {
  buildSegmentsForSession,
  type PipelineAdapters,
} from "@modelstat/daemon-core/pipeline";
import {
  offscreenEmbed,
  offscreenSummarize,
  offscreenTokenize,
} from "./offscreen.js";

let adapters: PipelineAdapters | null = null;

function getAdapters(): PipelineAdapters {
  if (adapters) return adapters;
  adapters = {
    embed: async (text: string) => {
      try {
        return await offscreenEmbed(text);
      } catch {
        // Offscreen not ready yet (embedder loading, no WebGPU, etc.) —
        // fall through to a zero-vector so segmentation still runs on
        // time gaps + deterministic tags.
        return [];
      }
    },
    summarize: async ({ prompt }: { prompt: string; maxTokens: number }) => {
      try {
        const { summary } = await offscreenSummarize(prompt);
        return (summary ?? "").slice(0, 240);
      } catch {
        return "";
      }
    },
    tokenize: async (text: string) => {
      try {
        const n = await offscreenTokenize("o200k_base", text);
        return n;
      } catch {
        // Cheap fallback — matches the Ollama adapter's heuristic.
        return Math.max(1, Math.ceil(text.length / 3.3));
      }
    },
  };
  return adapters;
}

export async function buildSegments(events: RawEvent[]): Promise<Segment[]> {
  if (events.length === 0) return [];
  return buildSegmentsForSession(events, getAdapters());
}
