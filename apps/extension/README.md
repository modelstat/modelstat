# modelstat — Chrome extension

Track token usage and cost across **ChatGPT**, **Claude.ai**, **Gemini**, and **Grok** web UIs. Local-first; optional sync to your modelstat account.

This is a thin-shell MV3 extension: a static interpreter bundled in the extension reads **declarative, signed adapter configs** downloaded from the modelstat API. When a provider changes its DOM or API shape, we push a new signed JSON to the API and every user picks it up within 15 minutes — no extension update required.

## Contents

- `manifest.json` — MV3 manifest, host-restricted to the four providers (no `<all_urls>`).
- `src/background/` — service worker: adapter registry, ingest queue, auth, discovery, alarms.
- `src/content/` — content script (ISOLATED) + `main-world.ts` injector (patches `fetch` + XHR at `document_start`).
- `src/interpreter/` — pure data-in, facts-out interpreter (DOM, URL, JSONPath, SSE, Ed25519 verify).
- `src/offscreen/` — tokenizers (tiktoken WASM + approximations) and local AI (transformers.js + WebLLM).
- `src/popup/`, `src/options/` — React + Tailwind UIs.
- `adapters/*.json` — bundled fallback adapter configs (chatgpt_web, claude_web, gemini_web, grok_web).
- `public/` — static assets: `logo.svg`, icon PNGs, bundled Ed25519 public key (`pubkey.ed25519`).

## Install (development — load unpacked)

Prereqs: Node 20+, pnpm 10+. Run once from the repo root:

```sh
pnpm install
pnpm extension:gen-key            # writes apps/extension/public/pubkey.ed25519 + prints env vars
# paste the printed EXTENSION_ADAPTER_PUBLIC_KEY + EXTENSION_ADAPTER_PRIVATE_KEY into .env
pnpm extension:build-prices       # emits apps/extension/public/assets/prices.json
pnpm extension:sign-adapters      # signs adapter configs with the Ed25519 key
pnpm extension:build              # prepare-assets + typecheck + vite build → apps/extension/dist
```

Then in Chrome: `chrome://extensions` → toggle **Developer mode** → **Load unpacked** → select `apps/extension/dist`.

Pin the extension; click its icon to see live token counts. Click the gear for auth, sync toggle, and on-device AI settings.

### Dev mode with HMR

```sh
pnpm extension:dev
```

This starts vite in watch mode. Every file change rebuilds incrementally; Chrome auto-reloads the extension.

## Install (production — Chrome Web Store)

```sh
pnpm extension:zip
```

Produces `apps/extension/modelstat-extension.zip`. Upload that file via the Chrome Web Store Developer Dashboard.

### CWS reviewer notes — remote code policy

This extension downloads **adapter configs** at runtime from `https://modelstat.ai/v1/extension/adapters/*`. The configs are **pure data**, not code:

- The schema is `packages/adapters-protocol/src/schema.ts` — a fixed set of extractor kinds (`dom.selector.text`, `url.regexGroup`, `network.responseJsonPath`, etc.).
- The interpreter that executes them is bundled statically at `src/interpreter/` — no `eval`, no `new Function`, no dynamic `import()`, no string-to-function dispatch.
- Every config is signed with **Ed25519**; the public key is compiled into the extension at build time (`public/pubkey.ed25519`). Invalid or missing signatures fall back to the bundled last-known-good config — the extension cannot execute unsigned data.

This mirrors what ad-blockers (uBlock Origin, AdGuard) do with filter lists: data-driven behaviour without running remote code.

## How it works (one paragraph)

A content script is injected at `document_start` on each supported host; it in turn injects a tiny MAIN-world script that monkey-patches `fetch` and `XMLHttpRequest` before the page captures them. SSE streams are tee'd with a manual `TextDecoder`; chunks are batched every 50ms and forwarded via `window.postMessage` (with a per-page nonce + origin check) to the ISOLATED-world content script, which forwards to the service worker via `chrome.runtime.sendMessage`. The SW runs the active adapter against each frame: JSONPath expressions extract messages and token usage; DOM observers and URL regexes provide redundant fallback variants. Messages are buffered in IndexedDB under a two-phase commit — a message is finalised once its commit window closes (30s) **or** the stream has ended and the DOM is stable for 2s. Finalised events carry exact tiktoken counts (for OpenAI-family) or labelled approximations (Anthropic / Gemma / Grok) and a computed $-equivalent from a bundled price table. Opt-in cloud sync batches finalised events into the existing modelstat `POST /v1/ingest` endpoint.

## Privacy

- Default: **local-only**. All data stays in IndexedDB on your device.
- Opt-in cloud sync sends only `{ tool, model, token counts, session_id, timestamp }` — **never** chat content, system prompts, or tool-call bodies.
- On-device AI (Chrome's Prompt API / WebLLM / embeddings) runs entirely in-browser. Nothing leaves the machine.

See [Options → Privacy](src/options/Options.tsx) inside the extension for the full statement.

## Troubleshooting

- **Popup shows "local-only" forever** — you haven't connected. Click ⚙ → Connect.
- **Sync toggle is disabled** — connect first; the toggle unlocks once the bearer token is stored.
- **Token counts drift by ±3-10%** on Claude / Gemini / Grok — expected; their tokenizers aren't public. The UI shows "est." in those cases.
- **Provider UI broke the adapter** — adapters are versioned and updated independently of the extension; the extension polls the manifest endpoint for newer signed adapter configs, so a fix can ship without a full extension release.
