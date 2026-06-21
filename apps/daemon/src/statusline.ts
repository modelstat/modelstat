/**
 * `modelstat statusline` — the always-on surface for Feature 1 (live
 * per-session insights). Claude Code runs this on every render, piping a JSON
 * status object to stdin; we print ONE compact line at the bottom of the
 * prompt.
 *
 * Two layers:
 *   1. Tokens — read straight from Claude Code's `context_window` (instant, no
 *      dependency on modelstat at all).
 *   2. modelstat — the effective $ assigned + taxonomy nodes detected for this
 *      session, read from the LOCAL insights cache
 *      (~/.modelstat/sessions/<session_id>.json) that the daemon refreshes
 *      after an eager scan. When the cache says `analyzing` (or isn't there
 *      yet) we show a quiet "analyzing…" so the line still renders.
 *
 * HARD CONSTRAINTS (a statusline that blocks or throws wedges the prompt):
 *   - NEVER block: cache reads are synchronous + tiny; we never await the
 *     network. The eager scan that populates the cache is kicked elsewhere
 *     (the `/stat` plugin / `modelstat scan --session`), not here.
 *   - NEVER throw: every parse is defensive and the CLI wrapper swallows
 *     errors, printing a minimal line instead.
 */
import { readCachedInsightsSync, type SessionInsights } from "./insights.js";

/** The subset of Claude Code's statusLine stdin payload we read. Everything is
 * optional — fields come and go by session state (no repo, pre-first-call
 * context window, etc.), so we treat the whole thing as best-effort. The
 * authoritative schema is Claude Code's; we stay loose so a field rename
 * upstream degrades gracefully instead of crashing. */
export interface StatuslineInput {
  session_id?: string;
  cwd?: string;
  model?: { id?: string; display_name?: string };
  workspace?: {
    current_dir?: string;
    project_dir?: string;
    repo?: { host?: string; owner?: string; name?: string };
  };
  context_window?: {
    total_input_tokens?: number;
    total_output_tokens?: number;
    context_window_size?: number;
    used_percentage?: number | null;
    current_usage?: {
      input_tokens?: number;
      output_tokens?: number;
      cache_creation_input_tokens?: number;
      cache_read_input_tokens?: number;
    } | null;
  };
  cost?: { total_cost_usd?: number };
}

// ── ANSI (kept tiny + self-contained; no dependency) ──────────────────
const DIM = "\x1b[2m";
const RESET = "\x1b[0m";
const CYAN = "\x1b[36m";
const GREEN = "\x1b[32m";
const SEP = `${DIM} · ${RESET}`;

/** Compact a token count: 1234 → "1.2k", 1_200_000 → "1.2M". */
export function formatTokens(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0";
  if (n < 1000) return String(Math.round(n));
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/** Format an effective-cost value (the server sends an exact decimal string;
 * tolerate a number too). Returns null when there's nothing meaningful. */
export function formatCost(cost: string | number | null | undefined): string | null {
  if (cost === null || cost === undefined) return null;
  const n = typeof cost === "number" ? cost : Number(cost);
  if (!Number.isFinite(n) || n <= 0) return null;
  // Sub-cent spend still matters when attributing — show 4dp under a cent.
  return n < 0.01 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
}

/** Total context-window tokens for this turn, from Claude Code's own counters.
 * Prefer the explicit totals; fall back to summing current usage. */
function contextTokens(cw: StatuslineInput["context_window"]): number {
  if (!cw) return 0;
  const totals = (cw.total_input_tokens ?? 0) + (cw.total_output_tokens ?? 0);
  if (totals > 0) return totals;
  const u = cw.current_usage;
  if (!u) return 0;
  return (
    (u.input_tokens ?? 0) +
    (u.output_tokens ?? 0) +
    (u.cache_creation_input_tokens ?? 0) +
    (u.cache_read_input_tokens ?? 0)
  );
}

/** Render up to `max` taxonomy chips (emoji + name), comma-joined. */
function renderTaxonomy(insights: SessionInsights, max = 3): string {
  const nodes = insights.taxonomy_nodes ?? [];
  if (nodes.length === 0) return "";
  const chips = nodes.slice(0, max).map((n) => {
    const emoji = n.emoji ? `${n.emoji} ` : "";
    return `${emoji}${n.name}`;
  });
  const extra = nodes.length > max ? ` +${nodes.length - max}` : "";
  return chips.join(", ") + extra;
}

/**
 * Build the one-line statusline string (no trailing newline). Pure — takes the
 * parsed stdin payload + the (optional) cached insights, returns the text. The
 * CLI wrapper handles reading stdin + the cache + printing.
 *
 * Shape: `<tokens> tok · <$ effective> · <taxonomy chips>` with a leading
 * `modelstat` marker. Missing pieces are simply omitted; while the server is
 * still enriching we show `analyzing…` in place of the $ / chips.
 */
export function renderStatusline(
  input: StatuslineInput,
  insights: SessionInsights | null,
): string {
  const parts: string[] = [];

  // 1. Tokens — instant, from Claude Code's context window.
  const tok = contextTokens(input.context_window);
  if (tok > 0) parts.push(`${CYAN}${formatTokens(tok)}${RESET}${DIM} tok${RESET}`);

  // 2. modelstat layer — effective $ + taxonomy from the local cache.
  if (insights && insights.status !== "not_ingested") {
    const cost = formatCost(insights.cost_usd);
    if (cost) parts.push(`${GREEN}${cost}${RESET}`);
    const tax = renderTaxonomy(insights);
    if (tax) {
      parts.push(`${DIM}${tax}${RESET}`);
    } else if (insights.status === "analyzing") {
      parts.push(`${DIM}analyzing…${RESET}`);
    }
  } else if (insights?.status === "analyzing") {
    parts.push(`${DIM}analyzing…${RESET}`);
  }

  const body = parts.join(SEP);
  // The marker keeps the line recognisable when composed after another tool's
  // statusline segment (the installer can chain ours onto an existing one).
  return body ? `${DIM}modelstat${RESET} ${body}` : `${DIM}modelstat${RESET}`;
}

/** Read all of stdin as text (Claude Code pipes the JSON status object). Caps
 * the read so a misbehaving caller can't make us buffer unbounded. */
async function readStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of process.stdin) {
    size += (chunk as Buffer).length;
    if (size > 1024 * 1024) break; // 1 MB is far more than any status payload
    chunks.push(chunk as Buffer);
  }
  return Buffer.concat(chunks).toString("utf8");
}

/**
 * CLI entry — `modelstat statusline`. Reads stdin, looks up the local insights
 * cache for the reported session, prints the one-liner. Never blocks on the
 * network and never throws: any failure degrades to a minimal `modelstat`
 * marker so the prompt still renders.
 */
export async function runStatusline(): Promise<void> {
  let input: StatuslineInput = {};
  try {
    const raw = await readStdin();
    if (raw.trim()) input = JSON.parse(raw) as StatuslineInput;
  } catch {
    // Unreadable / non-JSON stdin → render from nothing.
  }
  const insights = input.session_id ? readCachedInsightsSync(input.session_id) : null;
  process.stdout.write(renderStatusline(input, insights));
}
