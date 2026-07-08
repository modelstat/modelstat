/**
 * Summariser-mode settings — persisted in their OWN file
 * (`<modelstatHome()>/mode.json`), NOT in state.json.
 *
 * This mirrors `auto-update.json` (see update.ts) and for the same reason: the
 * long-running daemon holds a write-through cache of `state.json` and rewrites
 * the whole file on every cursor/counter update. A CLI-writable setting kept in
 * there would be CLOBBERED the moment the daemon persisted — which is exactly
 * what made an early `modelstat mode` change silently revert. The daemon only
 * ever READS this file, so an external `modelstat mode` / install write always
 * survives. A mode change still needs a daemon restart to take runtime effect
 * (the pipeline provider is resolved once per process); `modelstat mode` does
 * that restart.
 */

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homePath, modelstatHome } from "./paths.js";

/**
 * Where each session gets summarised — chosen at install, changeable later via
 * `modelstat mode`. Redaction (secrets + on-device NER/PII + emails + paths)
 * runs client-side in EVERY mode; only the summarisation LOCATION differs.
 *
 * - `local`       — the bundled Qwen model summarises on THIS machine (the only
 *   mode that downloads/loads the ~2.7 GB model). Ships abstracts to /v1/ingest.
 * - `self-hosted` — an org-run OpenAI-compatible endpoint summarises the cleaned
 *   excerpts (see selfHostedUrl/selfHostedModel). Ships abstracts to /v1/ingest.
 * - `cloud`       — no local summariser; cleaned turns ship to /v1/ingest/raw
 *   and modelstat's cloud summarises server-side. The install default.
 */
export type SummarizerMode = "local" | "self-hosted" | "cloud";

/** All valid modes, in menu order (Cloud first — it's the default). */
export const SUMMARIZER_MODES: readonly SummarizerMode[] = ["cloud", "local", "self-hosted"];

/** The install default: cloud (no local model, server-side summarisation). */
export const DEFAULT_SUMMARIZER_MODE: SummarizerMode = "cloud";

/** Narrow an arbitrary string to a {@link SummarizerMode}, else `null`. */
export function parseSummarizerMode(v: string | null | undefined): SummarizerMode | null {
  const s = (v ?? "").trim().toLowerCase();
  return (SUMMARIZER_MODES as readonly string[]).includes(s) ? (s as SummarizerMode) : null;
}

export interface ModeSettings {
  mode: SummarizerMode;
  /** Self-hosted only: the org's OpenAI-compatible summariser base URL. */
  selfHostedUrl: string;
  /** Self-hosted only: the model id to request from {@link selfHostedUrl}. */
  selfHostedModel: string;
}

function modePath(): string {
  return homePath("mode.json");
}

/**
 * Read the persisted mode settings, fresh from disk each call (cheap; read on a
 * cold path). A missing/corrupt/garbage file resolves to the defaults — a fresh
 * install has no file yet and is therefore `cloud`.
 */
export function readModeSettings(): ModeSettings {
  try {
    const o = JSON.parse(readFileSync(modePath(), "utf8")) as Partial<ModeSettings>;
    return {
      mode: parseSummarizerMode(o.mode) ?? DEFAULT_SUMMARIZER_MODE,
      selfHostedUrl: typeof o.selfHostedUrl === "string" ? o.selfHostedUrl : "",
      selfHostedModel: typeof o.selfHostedModel === "string" ? o.selfHostedModel : "",
    };
  } catch {
    return { mode: DEFAULT_SUMMARIZER_MODE, selfHostedUrl: "", selfHostedModel: "" };
  }
}

function write(settings: ModeSettings): void {
  mkdirSync(modelstatHome(), { recursive: true, mode: 0o700 });
  const tmp = `${modePath()}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(settings, null, 2), { mode: 0o600 });
  renameSync(tmp, modePath());
}

/** Persist the mode, preserving the stored self-hosted endpoint. */
export function writeMode(mode: SummarizerMode): void {
  write({ ...readModeSettings(), mode });
}

/** Persist the self-hosted endpoint (base URL + model id). */
export function writeSelfHosted(url: string, model: string): void {
  write({ ...readModeSettings(), selfHostedUrl: url, selfHostedModel: model });
}
