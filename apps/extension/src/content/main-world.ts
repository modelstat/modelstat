/**
 * MAIN-world injector — runs inside the page's JS context at
 * `document_start`, before the site captures `window.fetch` /
 * `XMLHttpRequest` into module closures.
 *
 * Responsibilities:
 *   1. Monkey-patch `fetch` and `XMLHttpRequest.prototype.send` /
 *      `.open` / `.addEventListener`. Patches are IDEMPOTENT — on SPA
 *      navigations the content script re-runs; we detect the marker
 *      and no-op.
 *   2. For each intercepted request, tee the response body (if it's a
 *      ReadableStream) into a second branch we consume manually with
 *      `TextDecoder` (never TextDecoderStream — it locks the stream
 *      inside pipeThrough). Chunks are buffered and flushed to the
 *      ISOLATED-world content script every SSE_FLUSH_INTERVAL_MS.
 *   3. Forward {url, method, requestBody, status, chunks, headers}
 *      frames via `window.postMessage` using a nonce + origin check
 *      (wrapped by the receiver).
 *
 * Memory discipline: tee() buffers for the slower branch. We consume
 * our branch in a detached async loop and never await anything slow
 * per-chunk — chunks go into a ring buffer that the flush timer
 * drains. Unbounded growth is prevented by a hard cap per-request.
 */
// Injected directly by manifest content_scripts with world: "MAIN" at
// document_start — no imports, no bundler rewriting needed. Messages
// are scoped by origin check + fixed tag; the page cannot distinguish
// our messages from its own but no untrusted extension can read them
// cross-origin.

(() => {
  const INSTALLED_MARKER = "__modelstat_main_world_installed__";
  const w = window as unknown as Record<string, unknown>;
  if (w[INSTALLED_MARKER]) return;
  w[INSTALLED_MARKER] = true;

  const BRIDGE_TAG = "__modelstat_bridge__";
  const SSE_FLUSH_INTERVAL_MS = 50;
  const MAX_BUFFERED_CHUNKS_PER_REQUEST = 1024;
  const MAX_BUFFERED_BYTES_PER_REQUEST = 4 * 1024 * 1024;

  type RequestFrame = {
    type: "request";
    id: string;
    url: string;
    method: string;
    requestBody: string | null;
    startedAt: number;
  };
  type ResponseStartFrame = {
    type: "response_start";
    id: string;
    status: number;
    contentType: string | null;
  };
  type ResponseChunkFrame = {
    type: "response_chunk";
    id: string;
    chunks: string[];
  };
  type ResponseEndFrame = {
    type: "response_end";
    id: string;
    endedAt: number;
    aborted: boolean;
  };

  type Frame = RequestFrame | ResponseStartFrame | ResponseChunkFrame | ResponseEndFrame;

  const post = (frame: Frame) => {
    window.postMessage({ tag: BRIDGE_TAG, frame }, window.location.origin);
  };

  const nextId = (() => {
    let n = 0;
    return () => `${Date.now().toString(36)}-${(++n).toString(36)}`;
  })();

  // ─── Chunk buffering + timed flush ──────────────────────────────
  const pendingChunks = new Map<string, string[]>();
  const pendingBytes = new Map<string, number>();
  let flushTimer: number | null = null;

  const scheduleFlush = () => {
    if (flushTimer !== null) return;
    flushTimer = window.setTimeout(() => {
      flushTimer = null;
      for (const [id, chunks] of pendingChunks) {
        if (chunks.length === 0) continue;
        post({ type: "response_chunk", id, chunks });
        pendingChunks.set(id, []);
        pendingBytes.set(id, 0);
      }
    }, SSE_FLUSH_INTERVAL_MS);
  };

  const bufferChunk = (id: string, text: string) => {
    const arr = pendingChunks.get(id) ?? [];
    const bytes = pendingBytes.get(id) ?? 0;
    if (arr.length >= MAX_BUFFERED_CHUNKS_PER_REQUEST) return;
    if (bytes + text.length > MAX_BUFFERED_BYTES_PER_REQUEST) return;
    arr.push(text);
    pendingChunks.set(id, arr);
    pendingBytes.set(id, bytes + text.length);
    scheduleFlush();
  };

  const closeRequest = (id: string, aborted: boolean) => {
    // Final flush first, so chunks arrive before response_end.
    const arr = pendingChunks.get(id);
    if (arr && arr.length) {
      post({ type: "response_chunk", id, chunks: arr });
    }
    pendingChunks.delete(id);
    pendingBytes.delete(id);
    post({ type: "response_end", id, endedAt: Date.now(), aborted });
  };

  // ─── fetch() patch ──────────────────────────────────────────────
  const origFetch = window.fetch.bind(window);
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const id = nextId();
    let url: string;
    let method: string;
    let requestBody: string | null = null;
    try {
      if (typeof input === "string") {
        url = input;
        method = (init?.method ?? "GET").toUpperCase();
        if (init?.body && typeof init.body === "string") requestBody = init.body;
      } else if (input instanceof URL) {
        url = input.toString();
        method = (init?.method ?? "GET").toUpperCase();
        if (init?.body && typeof init.body === "string") requestBody = init.body;
      } else {
        // Request object
        url = input.url;
        method = input.method;
        // Cloning the body stream is expensive — only peek if it's
        // explicitly opted-in by the adapter (TODO: plumb a
        // whitelist from the registry). v1: skip request bodies
        // when the input is a Request.
      }
      post({ type: "request", id, url, method, requestBody, startedAt: Date.now() });
    } catch {
      // Swallow — never break the page.
    }

    let response: Response;
    try {
      response = await origFetch(input as RequestInfo, init);
    } catch (e) {
      closeRequest(id, true);
      throw e;
    }

    try {
      const contentType = response.headers.get("content-type");
      post({ type: "response_start", id, status: response.status, contentType });
      if (response.body) {
        const [mine, theirs] = response.body.tee();
        consumeStream(id, mine).catch(() => closeRequest(id, true));
        return new Response(theirs, {
          status: response.status,
          statusText: response.statusText,
          headers: response.headers,
        });
      }
      closeRequest(id, false);
    } catch {
      closeRequest(id, true);
    }
    return response;
  };

  async function consumeStream(id: string, stream: ReadableStream<Uint8Array>): Promise<void> {
    const reader = stream.getReader();
    const decoder = new TextDecoder("utf-8", { fatal: false });
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) {
          const text = decoder.decode(value, { stream: true });
          if (text) bufferChunk(id, text);
        }
      }
      const tail = decoder.decode();
      if (tail) bufferChunk(id, tail);
      closeRequest(id, false);
    } finally {
      try {
        reader.releaseLock();
      } catch {
        /* ignore */
      }
    }
  }

  // ─── XMLHttpRequest patch ───────────────────────────────────────
  // Many chat UIs still use XHR for some endpoints. We capture the
  // full `responseText` on "loadend" — no streaming needed for XHR.
  const XHRProto = XMLHttpRequest.prototype;
  const origOpen = XHRProto.open;
  const origSend = XHRProto.send;

  type XHRState = { id: string; url: string; method: string; startedAt: number };
  const xhrState = new WeakMap<XMLHttpRequest, XHRState>();

  XHRProto.open = function (
    method: string,
    url: string | URL,
    async?: boolean,
    username?: string | null,
    password?: string | null,
  ) {
    const urlStr = typeof url === "string" ? url : url.toString();
    xhrState.set(this, {
      id: nextId(),
      url: urlStr,
      method: method.toUpperCase(),
      startedAt: Date.now(),
    });
    // biome-ignore lint/style/noArguments: forwarding a variadic polyfill signature
    return origOpen.apply(this, arguments as unknown as Parameters<typeof origOpen>);
  };

  XHRProto.send = function (body?: Document | XMLHttpRequestBodyInit | null) {
    const state = xhrState.get(this);
    if (state) {
      const requestBody = typeof body === "string" ? body : null;
      post({
        type: "request",
        id: state.id,
        url: state.url,
        method: state.method,
        requestBody,
        startedAt: state.startedAt,
      });
      this.addEventListener("loadend", () => {
        try {
          const contentType = this.getResponseHeader("content-type");
          post({ type: "response_start", id: state.id, status: this.status, contentType });
          if (typeof this.responseText === "string" && this.responseText.length) {
            bufferChunk(state.id, this.responseText);
          }
          closeRequest(state.id, false);
        } catch {
          closeRequest(state.id, true);
        }
      });
    }
    return origSend.call(this, body as XMLHttpRequestBodyInit | null | undefined);
  };
})();
