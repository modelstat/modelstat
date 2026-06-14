/**
 * Network-frame interpreter. Runs in the SW: receives MainFrame objects
 * forwarded by the content script, buffers chunks per request, and
 * emits extracted messages/scalars against the active adapter.
 *
 * SSE parsing is deliberately simple:
 *   - split on "\n\n" (event boundary)
 *   - each event = zero or more lines; take the concatenation of all
 *     "data: ..." lines for that event as a single JSON doc
 *   - ignore "event: ", "id: ", retry directives
 *   - buffer partial events across chunks
 */

import type {
  AdapterConfig,
  MessagesExtractor,
  NetworkResponseJsonPathExtractor,
} from "@modelstat/adapters-protocol";
import type { MainFrame } from "@/content/bridge.js";
import { asInt, asString, jsonPath } from "./jsonpath.js";

export type NetworkMessage = {
  messageId: string;
  role: "user" | "assistant";
  text: string | null;
  model: string | null;
  usage: {
    input: number | null;
    output: number | null;
    cache_creation: number | null;
    cache_read: number | null;
    reasoning: number | null;
  };
  host: string;
  href: string;
  observedAt: number;
};

export type NetworkScalar = {
  field: "model" | "conversation_id";
  value: string;
  host: string;
  observedAt: number;
};

export type NetworkEmit = {
  messages: NetworkMessage[];
  scalars: NetworkScalar[];
};

type ReqState = {
  url: string;
  method: string;
  host: string;
  href: string;
  status: number | null;
  contentType: string | null;
  sseBuffer: string;
  bodyBuffer: string;
  isSse: boolean;
  startedAt: number;
};

const requests = new Map<string, ReqState>();

function uniteUsage(
  base: NetworkMessage["usage"],
  add: NetworkMessage["usage"],
): NetworkMessage["usage"] {
  return {
    input: add.input ?? base.input,
    output: add.output ?? base.output,
    cache_creation: add.cache_creation ?? base.cache_creation,
    cache_read: add.cache_read ?? base.cache_read,
    reasoning: add.reasoning ?? base.reasoning,
  };
}

function extractUsageFromPath(
  ex: NetworkResponseJsonPathExtractor,
  root: unknown,
): NetworkMessage["usage"] {
  const out: NetworkMessage["usage"] = {
    input: null,
    output: null,
    cache_creation: null,
    cache_read: null,
    reasoning: null,
  };
  if (ex.usagePath) {
    const u = jsonPath(root, ex.usagePath);
    if (u && typeof u === "object" && !Array.isArray(u)) {
      const obj = u as Record<string, unknown>;
      // Common provider shapes — map by name.
      out.input = asInt(obj.input_tokens ?? obj.prompt_tokens ?? obj.input);
      out.output = asInt(obj.output_tokens ?? obj.completion_tokens ?? obj.output);
      out.cache_creation = asInt(obj.cache_creation_input_tokens ?? obj.cache_creation);
      out.cache_read = asInt(obj.cache_read_input_tokens ?? obj.cache_read);
      out.reasoning = asInt(obj.reasoning_tokens);
    }
  }
  if (ex.usageFields) {
    if (ex.usageFields.input) out.input = asInt(jsonPath(root, ex.usageFields.input)) ?? out.input;
    if (ex.usageFields.output) out.output = asInt(jsonPath(root, ex.usageFields.output)) ?? out.output;
    if (ex.usageFields.cache_creation)
      out.cache_creation = asInt(jsonPath(root, ex.usageFields.cache_creation)) ?? out.cache_creation;
    if (ex.usageFields.cache_read)
      out.cache_read = asInt(jsonPath(root, ex.usageFields.cache_read)) ?? out.cache_read;
    if (ex.usageFields.reasoning)
      out.reasoning = asInt(jsonPath(root, ex.usageFields.reasoning)) ?? out.reasoning;
  }
  return out;
}

function runMessagesNetworkExtractor(
  ex: NetworkResponseJsonPathExtractor,
  doc: unknown,
  state: ReqState,
): NetworkMessage[] {
  if (!ex.messageIdPath) return [];
  const messageId = asString(jsonPath(doc, ex.messageIdPath));
  if (!messageId) return [];
  const role = (asString(jsonPath(doc, ex.rolePath ?? "")) as "user" | "assistant" | null) ?? "assistant";
  const text = ex.textPath ? asString(jsonPath(doc, ex.textPath)) : null;
  const usage = extractUsageFromPath(ex, doc);
  const model = ex.path ? asString(jsonPath(doc, ex.path)) : null;
  return [
    {
      messageId,
      role,
      text,
      model,
      usage,
      host: state.host,
      href: state.href,
      observedAt: Date.now(),
    },
  ];
}

function processDoc(
  doc: unknown,
  ex: NetworkResponseJsonPathExtractor,
  state: ReqState,
  emit: NetworkEmit,
): void {
  // Messages (if this extractor is a messages-variant)
  if (ex.messageIdPath) {
    const msgs = runMessagesNetworkExtractor(ex, doc, state);
    // Merge per-message by messageId: network emits may come in parts
    // (text in one frame, usage in another) — collapse here.
    for (const m of msgs) {
      const existing = emit.messages.find((x) => x.messageId === m.messageId);
      if (existing) {
        existing.text = m.text ?? existing.text;
        existing.model = m.model ?? existing.model;
        existing.usage = uniteUsage(existing.usage, m.usage);
      } else {
        emit.messages.push(m);
      }
    }
  }
  // Scalars (model, conversation_id) — when the extractor targets
  // only `path` (not messageIdPath).
  if (!ex.messageIdPath && ex.path) {
    const v = asString(jsonPath(doc, ex.path));
    if (v) {
      // We don't know which scalar field this maps to from the
      // extractor alone — the adapter wires the extractor under
      // `extractors.model` or `extractors.conversation_id`. The caller
      // (runAdapterOnFrame) provides the field label.
      emit.scalars.push({
        field: "model",
        value: v,
        host: state.host,
        observedAt: Date.now(),
      });
    }
  }
}

function matchUrl(pattern: string, url: string, method: string, exMethod?: string): boolean {
  if (exMethod && exMethod !== method) return false;
  try {
    return new RegExp(pattern).test(url);
  } catch {
    return false;
  }
}

/** Entry point — hand it a frame + adapter, receive extracted msgs. */
export function processFrame(
  frame: MainFrame,
  adapter: AdapterConfig,
  host: string,
  href: string,
): NetworkEmit {
  const out: NetworkEmit = { messages: [], scalars: [] };

  if (frame.type === "request") {
    requests.set(frame.id, {
      url: frame.url,
      method: frame.method,
      host,
      href,
      status: null,
      contentType: null,
      sseBuffer: "",
      bodyBuffer: "",
      isSse: false,
      startedAt: frame.startedAt,
    });
    // Request-body extractors (rare, but supported) for model scalar
    for (const ex of adapter.extractors.model) {
      if (ex.kind !== "network.requestJsonPath") continue;
      if (!matchUrl(ex.urlPattern, frame.url, frame.method, ex.method)) continue;
      if (!frame.requestBody) continue;
      try {
        const doc = JSON.parse(frame.requestBody);
        const v = asString(jsonPath(doc, ex.path));
        if (v) out.scalars.push({ field: "model", value: v, host, observedAt: Date.now() });
      } catch {
        /* skip non-JSON bodies */
      }
    }
    return out;
  }

  const state = requests.get(frame.id);
  if (!state) return out;

  if (frame.type === "response_start") {
    state.status = frame.status;
    state.contentType = frame.contentType;
    state.isSse = (frame.contentType ?? "").toLowerCase().includes("text/event-stream");
    return out;
  }

  if (frame.type === "response_chunk") {
    const joined = frame.chunks.join("");
    if (state.isSse) {
      state.sseBuffer += joined;
      const events: unknown[] = [];
      // SSE events delimited by \n\n; partial events stay in buffer.
      let idx: number;
      while ((idx = state.sseBuffer.indexOf("\n\n")) >= 0) {
        const rawEvent = state.sseBuffer.slice(0, idx);
        state.sseBuffer = state.sseBuffer.slice(idx + 2);
        const dataLines: string[] = [];
        for (const line of rawEvent.split("\n")) {
          if (line.startsWith("data:")) dataLines.push(line.slice(5).trimStart());
        }
        if (dataLines.length === 0) continue;
        const body = dataLines.join("\n");
        if (body === "[DONE]") continue;
        try {
          events.push(JSON.parse(body));
        } catch {
          /* non-JSON frame — skip */
        }
      }
      for (const doc of events) {
        runAllNetworkExtractors(doc, adapter, state, out);
      }
    } else {
      state.bodyBuffer += joined;
    }
    return out;
  }

  if (frame.type === "response_end") {
    if (!state.isSse && state.bodyBuffer) {
      try {
        const doc = JSON.parse(state.bodyBuffer);
        runAllNetworkExtractors(doc, adapter, state, out);
      } catch {
        /* non-JSON response body */
      }
    }
    requests.delete(frame.id);
    return out;
  }

  return out;
}

function runAllNetworkExtractors(
  doc: unknown,
  adapter: AdapterConfig,
  state: ReqState,
  out: NetworkEmit,
): void {
  // messages channel
  for (const ex of adapter.extractors.messages) {
    if (ex.kind !== "network.responseJsonPath") continue;
    if (!matchUrl(ex.urlPattern, state.url, state.method, ex.method)) continue;
    processDoc(doc, ex, state, out);
  }
  // model scalar
  for (const ex of adapter.extractors.model) {
    if (ex.kind !== "network.responseJsonPath") continue;
    if (!matchUrl(ex.urlPattern, state.url, state.method, ex.method)) continue;
    if (ex.path) {
      const v = asString(jsonPath(doc, ex.path));
      if (v) {
        out.scalars.push({
          field: "model",
          value: v,
          host: state.host,
          observedAt: Date.now(),
        });
      }
    }
  }
  // conversation_id scalar
  for (const ex of adapter.extractors.conversation_id) {
    if (ex.kind !== "network.responseJsonPath") continue;
    if (!matchUrl(ex.urlPattern, state.url, state.method, ex.method)) continue;
    if (ex.path) {
      const v = asString(jsonPath(doc, ex.path));
      if (v) {
        out.scalars.push({
          field: "conversation_id",
          value: v,
          host: state.host,
          observedAt: Date.now(),
        });
      }
    }
  }
}

/** Silence unused-import — schema types are narrative only. */
type _UnusedMessages = MessagesExtractor;
